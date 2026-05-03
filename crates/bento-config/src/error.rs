use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse {kind} at {path}:\n{message}")]
    Parse {
        kind: &'static str,
        path: PathBuf,
        message: String,
    },

    #[error("invalid {kind} at {path}: {message}")]
    Invalid {
        kind: &'static str,
        path: PathBuf,
        message: String,
    },

    #[error("missing required file: {path}")]
    Missing { path: PathBuf },

    #[error("duplicate {kind} name '{name}' in {path_a} and {path_b}")]
    Duplicate {
        kind: &'static str,
        name: String,
        path_a: PathBuf,
        path_b: PathBuf,
    },

    #[error("bento '{bento}' references dish path '{dish_path}' which has no dish.toml")]
    DanglingDishRef { bento: String, dish_path: PathBuf },
}
