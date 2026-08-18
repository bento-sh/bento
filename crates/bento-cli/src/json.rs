//! One place that decides how `--json` output is shaped.
//!
//! Pretty-printed when stdout is a terminal (a human is reading it),
//! compact when it's a pipe — that's the agent path, and indentation
//! is ~40% of the bytes an agent then pays for as tokens.

use std::io::IsTerminal;

pub fn to_string<T: serde::Serialize + ?Sized>(value: &T) -> serde_json::Result<String> {
    if std::io::stdout().is_terminal() {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    }
}

pub fn emit<T: serde::Serialize + ?Sized>(value: &T) -> anyhow::Result<()> {
    println!("{}", to_string(value)?);
    Ok(())
}
