//! JSON-RPC 2.0 message types + bento-specific error codes.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub const JSONRPC_VERSION: &str = "2.0";

/// Outbound request: bento → plugin. The `id` is a `u64` so we can
/// pre-allocate from an `AtomicU64`.
#[derive(Debug, Clone, Serialize)]
pub struct Request<'a, P: Serialize> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<&'a P>,
}

impl<'a, P: Serialize> Request<'a, P> {
    pub fn new(id: u64, method: &'a str, params: Option<&'a P>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            id,
            method,
            params,
        }
    }
}

/// Inbound message — either a Response (carries `id`) or a Notification
/// (no `id`, `method` + `params`). We deserialise to this enum and let
/// the client dispatch.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Inbound {
    Response(Response),
    Notification(Notification),
}

#[derive(Debug, Clone, Deserialize)]
pub struct Response {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub id: u64,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Notification {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

/// JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize, Error)]
#[error("rpc error {code}: {message}")]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Bento-specific error codes layered over the JSON-RPC spec.
///
/// `-32700..-32600` are reserved by JSON-RPC 2.0 (parse error, invalid
/// request, method not found, invalid params, internal error).
/// `2000+` are bento additions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ErrorCode {
    ParseError = -32700,
    InvalidRequest = -32600,
    MethodNotFound = -32601,
    InvalidParams = -32602,
    InternalError = -32603,

    InstallFailed = 2001,
    ToolchainUnparseable = 2002,
    PluginIo = 2003,
    PluginInternal = 2099,
}

impl ErrorCode {
    pub fn from_i32(code: i32) -> Option<Self> {
        Some(match code {
            -32700 => Self::ParseError,
            -32600 => Self::InvalidRequest,
            -32601 => Self::MethodNotFound,
            -32602 => Self::InvalidParams,
            -32603 => Self::InternalError,
            2001 => Self::InstallFailed,
            2002 => Self::ToolchainUnparseable,
            2003 => Self::PluginIo,
            2099 => Self::PluginInternal,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serialises_without_params() {
        let req: Request<()> = Request::new(7, "shutdown", None);
        let s = serde_json::to_string(&req).unwrap();
        assert_eq!(s, r#"{"jsonrpc":"2.0","id":7,"method":"shutdown"}"#);
    }

    #[test]
    fn request_serialises_with_params() {
        #[derive(Serialize)]
        struct P {
            dir: &'static str,
        }
        let p = P { dir: "/tmp" };
        let req = Request::new(1, "detect", Some(&p));
        let s = serde_json::to_string(&req).unwrap();
        assert_eq!(
            s,
            r#"{"jsonrpc":"2.0","id":1,"method":"detect","params":{"dir":"/tmp"}}"#
        );
    }

    #[test]
    fn inbound_parses_response() {
        let s = r#"{"jsonrpc":"2.0","id":1,"result":{"matches":true}}"#;
        let m: Inbound = serde_json::from_str(s).unwrap();
        match m {
            Inbound::Response(r) => {
                assert_eq!(r.id, 1);
                assert!(r.error.is_none());
                assert_eq!(r.result.unwrap()["matches"], true);
            }
            _ => panic!("expected response"),
        }
    }

    #[test]
    fn inbound_parses_response_with_error() {
        let s = r#"{"jsonrpc":"2.0","id":4,"error":{"code":2001,"message":"boom"}}"#;
        let m: Inbound = serde_json::from_str(s).unwrap();
        match m {
            Inbound::Response(r) => {
                let err = r.error.unwrap();
                assert_eq!(err.code, 2001);
                assert_eq!(err.message, "boom");
            }
            _ => panic!("expected response"),
        }
    }

    #[test]
    fn inbound_parses_notification() {
        let s = r#"{"jsonrpc":"2.0","method":"notifications/log","params":{"level":"info"}}"#;
        let m: Inbound = serde_json::from_str(s).unwrap();
        match m {
            Inbound::Notification(n) => {
                assert_eq!(n.method, "notifications/log");
                assert_eq!(n.params.unwrap()["level"], "info");
            }
            _ => panic!("expected notification"),
        }
    }

    #[test]
    fn error_code_roundtrip() {
        for code in [
            ErrorCode::ParseError,
            ErrorCode::InvalidRequest,
            ErrorCode::MethodNotFound,
            ErrorCode::InstallFailed,
            ErrorCode::PluginInternal,
        ] {
            assert_eq!(ErrorCode::from_i32(code as i32), Some(code));
        }
        assert_eq!(ErrorCode::from_i32(9999), None);
    }
}
