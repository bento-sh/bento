//! LSP-style `Content-Length` framing.
//!
//! Each message is a header block (`Content-Length: <n>\r\n` plus optional
//! other headers we ignore, terminated by `\r\n\r\n`) followed by exactly
//! `<n>` bytes of UTF-8 JSON body. Robust against JSON values containing
//! literal newlines, and trivially debuggable with a hex dump.

use std::io::{BufRead, Write};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("io error reading frame: {0}")]
    Io(#[from] std::io::Error),
    #[error("unexpected end of stream while reading {while_doing}")]
    UnexpectedEof { while_doing: &'static str },
    #[error("malformed header line {line:?}")]
    MalformedHeader { line: String },
    #[error("missing Content-Length header")]
    MissingContentLength,
    #[error("invalid Content-Length value {value:?}: {source}")]
    InvalidContentLength {
        value: String,
        #[source]
        source: std::num::ParseIntError,
    },
    #[error("body was not valid UTF-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
}

/// Read one framed message from `r`. Returns the body as a `String` (we
/// always parse it as JSON downstream).
pub fn read_message<R: BufRead>(r: &mut R) -> Result<String, FrameError> {
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            return Err(FrameError::UnexpectedEof {
                while_doing: "headers",
            });
        }

        // End-of-headers is a bare CRLF (or LF — be lenient).
        if line == "\r\n" || line == "\n" {
            break;
        }

        // `name: value\r\n`. Trim the trailing newline.
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let Some((name, value)) = trimmed.split_once(':') else {
            return Err(FrameError::MalformedHeader {
                line: trimmed.into(),
            });
        };
        let name = name.trim();
        let value = value.trim();

        // Header names are case-insensitive per RFC 7230 (and LSP follows
        // suit). We only care about Content-Length; ignore everything else.
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(value.parse::<usize>().map_err(|source| {
                FrameError::InvalidContentLength {
                    value: value.into(),
                    source,
                }
            })?);
        }
    }

    let len = content_length.ok_or(FrameError::MissingContentLength)?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            FrameError::UnexpectedEof {
                while_doing: "body",
            }
        } else {
            FrameError::Io(e)
        }
    })?;
    Ok(String::from_utf8(buf)?)
}

/// Write `body` to `w` as a single framed message. The body should be
/// pre-serialised JSON (we don't validate; that's the caller's job).
pub fn write_message<W: Write>(w: &mut W, body: &str) -> std::io::Result<()> {
    let bytes = body.as_bytes();
    write!(w, "Content-Length: {}\r\n\r\n", bytes.len())?;
    w.write_all(bytes)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    fn frame(body: &str) -> Vec<u8> {
        let mut out = Vec::new();
        write_message(&mut out, body).unwrap();
        out
    }

    #[test]
    fn roundtrip_simple_body() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let bytes = frame(body);
        let mut r = BufReader::new(Cursor::new(bytes));
        assert_eq!(read_message(&mut r).unwrap(), body);
    }

    #[test]
    fn roundtrip_body_with_embedded_newlines() {
        // The whole point of length-framing is bodies with newlines work.
        let body = "{\n  \"key\": \"value with\\nliteral newline in source\"\n}";
        let bytes = frame(body);
        let mut r = BufReader::new(Cursor::new(bytes));
        assert_eq!(read_message(&mut r).unwrap(), body);
    }

    #[test]
    fn reads_back_to_back_messages() {
        let mut bytes = frame("first");
        bytes.extend(frame("second"));
        let mut r = BufReader::new(Cursor::new(bytes));
        assert_eq!(read_message(&mut r).unwrap(), "first");
        assert_eq!(read_message(&mut r).unwrap(), "second");
    }

    #[test]
    fn ignores_unknown_headers_case_insensitively() {
        let bytes = b"X-Whatever: foo\r\nCONTENT-length: 2\r\n\r\nhi";
        let mut r = BufReader::new(Cursor::new(&bytes[..]));
        assert_eq!(read_message(&mut r).unwrap(), "hi");
    }

    #[test]
    fn missing_content_length_is_error() {
        let bytes = b"X-Whatever: foo\r\n\r\nhi";
        let mut r = BufReader::new(Cursor::new(&bytes[..]));
        let err = read_message(&mut r).unwrap_err();
        assert!(matches!(err, FrameError::MissingContentLength));
    }

    #[test]
    fn malformed_header_is_error() {
        let bytes = b"no-colon-here\r\n\r\n";
        let mut r = BufReader::new(Cursor::new(&bytes[..]));
        let err = read_message(&mut r).unwrap_err();
        assert!(matches!(err, FrameError::MalformedHeader { .. }));
    }

    #[test]
    fn truncated_body_is_eof() {
        let bytes = b"Content-Length: 10\r\n\r\nshort";
        let mut r = BufReader::new(Cursor::new(&bytes[..]));
        let err = read_message(&mut r).unwrap_err();
        assert!(matches!(
            err,
            FrameError::UnexpectedEof {
                while_doing: "body"
            }
        ));
    }

    #[test]
    fn empty_stream_is_eof() {
        let bytes: &[u8] = b"";
        let mut r = BufReader::new(Cursor::new(bytes));
        let err = read_message(&mut r).unwrap_err();
        assert!(matches!(
            err,
            FrameError::UnexpectedEof {
                while_doing: "headers"
            }
        ));
    }

    #[test]
    fn lone_lf_terminator_is_accepted() {
        // Some plugin authors may emit bare LF instead of CRLF. Be lenient.
        let bytes = b"Content-Length: 2\n\nok";
        let mut r = BufReader::new(Cursor::new(&bytes[..]));
        assert_eq!(read_message(&mut r).unwrap(), "ok");
    }
}
