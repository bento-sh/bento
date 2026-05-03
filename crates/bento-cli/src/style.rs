//! Terminal styling helpers.
//!
//! ANSI escape codes when stdout is a TTY, passthrough otherwise. Keeping
//! this in one place lets the rest of the CLI stay opinion-free — it asks
//! for `style::green(...)` and never thinks about whether output will land
//! in a pipe.
//!
//! Deliberately avoids a third-party colour crate: a handful of escapes
//! covers every place we actually render.

use std::io::IsTerminal;
use std::sync::OnceLock;

fn colours_enabled() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        // Standard overrides first (NO_COLOR: https://no-color.org).
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        if std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| v != "0") {
            return true;
        }
        std::io::stdout().is_terminal()
    })
}

fn wrap(code: &str, s: &str) -> String {
    if colours_enabled() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn green(s: &str) -> String {
    wrap("32", s)
}

pub fn red(s: &str) -> String {
    wrap("31", s)
}

pub fn yellow(s: &str) -> String {
    wrap("33", s)
}

pub fn cyan(s: &str) -> String {
    wrap("36", s)
}

pub fn dim(s: &str) -> String {
    wrap("2", s)
}

pub fn bold(s: &str) -> String {
    wrap("1", s)
}
