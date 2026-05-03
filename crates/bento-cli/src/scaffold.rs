//! `bento dish add <path> [--lang <ecosystem>]`.
//!
//! Two flows, picked automatically from the target directory's state:
//!
//! - **Scaffold** — the path is empty or absent. `--lang` is required.
//!   We write a minimal compilable starter plus a `dish.toml`.
//! - **Adopt** — the path has existing code. We leave the sources
//!   untouched and write only `dish.toml`. `--lang` is optional; when
//!   omitted the adapter registry's `detect()` picks it.
//!
//! In both cases the new dish is spliced into the target bento's
//! `dishes` list via `toml_edit`, so comments and formatting survive.
//!
//! Supported languages: `go`, `node-npm`, `node-pnpm`, `node-yarn`,
//! `bun`, `deno`, `cargo`, `python`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Serialize;

use bento_config::Workspace;

/// Typed scaffold failures. Downcast-friendly so the error classifier
/// can turn each variant into a stable `BentoError.kind`.
#[derive(Debug, thiserror::Error)]
pub enum ScaffoldError {
    #[error("--lang is required to scaffold an empty directory")]
    MissingLanguage,

    #[error("unsupported language: '{lang}'")]
    UnsupportedLanguage { lang: String },

    #[error("invalid dish path '{path}': {reason}")]
    InvalidDishPath { path: String, reason: String },

    #[error("dish path '{path}' is already registered in a bento")]
    DishPathRegistered { path: String },

    #[error("dish name '{name}' already exists in this workspace")]
    DishNameCollision { name: String },

    #[error("no bentos defined — create one first with `bento box add <name>`")]
    NoBentos,

    #[error("multiple bentos defined ({available}); pass --bento <name>")]
    MultipleBentos { available: String },

    #[error("no bento named '{name}' (known: {available})")]
    UnknownBento { name: String, available: String },

    #[error("bento file at {path} has no `dishes` array")]
    BentoConfigShape { path: String },

    #[error("dish.toml already exists at {path} — refusing to overwrite")]
    DishAlreadyConfigured { path: String },

    #[error(
        "could not auto-detect the language in {path}; pass --lang <go|node-npm|node-pnpm|node-yarn|bun|deno|cargo|python>"
    )]
    LanguageUnknown { path: String },

    #[error("I/O failure: {source}")]
    Io {
        #[source]
        source: std::io::Error,
    },
}

impl ScaffoldError {
    fn io(source: std::io::Error) -> Self {
        Self::Io { source }
    }
}

// ── public surface ─────────────────────────────────────────────────

pub struct ScaffoldRequest<'a> {
    pub workspace_root: &'a Path,
    pub dish_rel: &'a Path,
    /// Language id. When `None`:
    /// - If the target dir is non-empty we'll auto-detect via the adapter registry.
    /// - If the target dir is empty we return [`ScaffoldError::MissingLanguage`].
    pub language: Option<&'a str>,
    pub bento: Option<&'a str>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ScaffoldResult {
    pub dish_name: String,
    pub bento_name: String,
    pub language: String,
    pub mode: ScaffoldMode,
    pub files_written: Vec<PathBuf>,
    pub next_steps: Vec<String>,
}

/// Did `bento dish add` write fresh boilerplate, or just register an
/// existing tree as a dish? Agents switch on this to decide whether to
/// commit generated sources or leave the tree untouched.
#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScaffoldMode {
    /// Fresh directory — bento wrote a working starter.
    Scaffolded,
    /// Existing code — bento only wrote `dish.toml` alongside it.
    Adopted,
}

pub fn run(req: ScaffoldRequest, workspace: &Workspace) -> Result<ScaffoldResult> {
    let bento_name = pick_bento(workspace, req.bento)?;
    let bento_source = workspace.bentos[&bento_name].source.clone();

    let dish_name = derive_dish_name(req.dish_rel, req.workspace_root)?;
    if workspace.dishes_by_name.contains_key(&dish_name) {
        return Err(ScaffoldError::DishNameCollision { name: dish_name }.into());
    }

    let dish_rel_str = normalize_rel(req.dish_rel);
    if workspace
        .dishes_by_path
        .keys()
        .any(|p| normalize_rel(p) == dish_rel_str)
    {
        return Err(ScaffoldError::DishPathRegistered { path: dish_rel_str }.into());
    }

    let dish_abs = req.workspace_root.join(req.dish_rel);
    let non_empty = dish_abs.is_dir()
        && std::fs::read_dir(&dish_abs)
            .map_err(ScaffoldError::io)?
            .next()
            .is_some();

    let registry = crate::plugins::build_registry(workspace);
    let (language_id, mode, mut files_written, next_steps) = if non_empty {
        adopt(&dish_abs, &dish_name, req.language, &registry)?
    } else {
        scaffold(&dish_abs, &dish_name, req.language)?
    };

    wire_into_bento(&bento_source, &dish_rel_str)?;
    files_written.push(bento_source);

    // Stable, repo-relative display paths.
    let files_written: Vec<PathBuf> = files_written
        .into_iter()
        .map(|p| relativize(&p, req.workspace_root))
        .collect();

    Ok(ScaffoldResult {
        dish_name,
        bento_name,
        language: language_id,
        mode,
        files_written,
        next_steps,
    })
}

/// Scaffold a brand-new dish. The target dir is empty or absent so we
/// own every file we write. `language` must be supplied — we don't
/// guess at boilerplate.
fn scaffold(
    dish_abs: &Path,
    dish_name: &str,
    language: Option<&str>,
) -> Result<(String, ScaffoldMode, Vec<PathBuf>, Vec<String>)> {
    let Some(lang) = language else {
        return Err(ScaffoldError::MissingLanguage.into());
    };
    let (files, next_steps) = match lang {
        "go" => scaffold_go(dish_abs, dish_name)?,
        "node-npm" => scaffold_js_family(dish_abs, dish_name, JsTool::Npm)?,
        "node-pnpm" => scaffold_js_family(dish_abs, dish_name, JsTool::Pnpm)?,
        "node-yarn" => scaffold_js_family(dish_abs, dish_name, JsTool::Yarn)?,
        "bun" => scaffold_js_family(dish_abs, dish_name, JsTool::Bun)?,
        "deno" => scaffold_deno(dish_abs, dish_name)?,
        "cargo" => scaffold_cargo(dish_abs, dish_name)?,
        "python" => scaffold_python(dish_abs, dish_name)?,
        "python-uv" => {
            // Same starter source as the pip-based python adapter; the
            // registry routes to python-uv once `uv.lock` materialises.
            // Tell the user how to get there.
            let (files, mut steps) = scaffold_python(dish_abs, dish_name)?;
            steps.push(
                "Run `uv lock` (or `uv sync`) inside the dish to materialise uv.lock; \
                 bento will then route this dish through the python-uv adapter automatically."
                    .to_string(),
            );
            (files, steps)
        }
        "ruby" => scaffold_ruby(dish_abs, dish_name)?,
        "php" => scaffold_php(dish_abs, dish_name)?,
        "maven" => scaffold_maven(dish_abs, dish_name)?,
        "gradle" => scaffold_gradle(dish_abs, dish_name)?,
        other => {
            return Err(ScaffoldError::UnsupportedLanguage {
                lang: other.to_string(),
            }
            .into());
        }
    };
    Ok((
        lang.to_string(),
        ScaffoldMode::Scaffolded,
        files,
        next_steps,
    ))
}

/// Adopt an existing directory as a dish: write only `dish.toml` and
/// leave the user's sources alone. Language is explicit (`--lang`) or
/// auto-detected from the file tree.
fn adopt(
    dish_abs: &Path,
    dish_name: &str,
    language: Option<&str>,
    registry: &bento_core::AdapterRegistry,
) -> Result<(String, ScaffoldMode, Vec<PathBuf>, Vec<String>)> {
    let dish_toml_path = dish_abs.join("dish.toml");
    if dish_toml_path.exists() {
        return Err(ScaffoldError::DishAlreadyConfigured {
            path: dish_toml_path.display().to_string(),
        }
        .into());
    }

    let language_id = match language {
        Some(id) => {
            if registry.by_id(id).is_none() {
                return Err(ScaffoldError::UnsupportedLanguage {
                    lang: id.to_string(),
                }
                .into());
            }
            id.to_string()
        }
        None => match registry.detect(dish_abs) {
            Some(a) => a.id().to_string(),
            None => {
                return Err(ScaffoldError::LanguageUnknown {
                    path: dish_abs.display().to_string(),
                }
                .into());
            }
        },
    };

    // Detect tasks from project metadata (e.g. package.json scripts) so
    // adopting an existing project gets a fully populated dish.toml — not
    // just the standard build/test/lint aliases. CI flows want every
    // script mirrored.
    let detected_tasks = registry
        .by_id(&language_id)
        .and_then(|a| a.detected_tasks(dish_abs));
    let dish_toml =
        crate::init::render_dish_toml(dish_name, &language_id, detected_tasks.as_deref());
    let files = vec![write_file(&dish_toml_path, &dish_toml)?];

    let next_steps = vec!["bento plan".to_string(), format!("bento build {dish_name}")];

    Ok((language_id, ScaffoldMode::Adopted, files, next_steps))
}

// ── bento selection + validation ───────────────────────────────────

fn pick_bento(ws: &Workspace, requested: Option<&str>) -> Result<String> {
    if let Some(name) = requested {
        if !ws.bentos.contains_key(name) {
            return Err(ScaffoldError::UnknownBento {
                name: name.to_string(),
                available: format_known_bentos(ws),
            }
            .into());
        }
        return Ok(name.to_string());
    }
    match ws.bentos.len() {
        0 => Err(ScaffoldError::NoBentos.into()),
        1 => Ok(ws.bentos.keys().next().unwrap().clone()),
        _ => Err(ScaffoldError::MultipleBentos {
            available: format_known_bentos(ws),
        }
        .into()),
    }
}

fn format_known_bentos(ws: &Workspace) -> String {
    let names: Vec<&str> = ws.bentos.keys().map(String::as_str).collect();
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

fn derive_dish_name(rel: &Path, workspace_root: &Path) -> Result<String> {
    // `.` / empty → use the workspace root's basename so `bento dish add .`
    // in a single-repo project names the dish after the repo dir.
    let rel_os = rel.as_os_str();
    let name_source: std::path::PathBuf = if rel_os == "." || rel_os.is_empty() {
        workspace_root.to_path_buf()
    } else {
        workspace_root.join(rel)
    };

    let name = name_source
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| ScaffoldError::InvalidDishPath {
            path: rel.display().to_string(),
            reason: "path does not end in a valid dish name".into(),
        })?;
    if name.is_empty() || name == "." || name == ".." {
        return Err(ScaffoldError::InvalidDishPath {
            path: rel.display().to_string(),
            reason: format!("invalid terminal component '{name}'"),
        }
        .into());
    }
    if name.contains('/') || name.contains(std::path::MAIN_SEPARATOR) {
        return Err(ScaffoldError::InvalidDishPath {
            path: rel.display().to_string(),
            reason: "dish name must not contain path separators".into(),
        }
        .into());
    }
    Ok(name.to_string())
}

fn normalize_rel(path: &Path) -> String {
    // dish refs in bento.toml always use forward slashes, regardless of host.
    path.to_string_lossy().replace('\\', "/")
}

fn relativize(path: &Path, root: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

// ── Go scaffold ────────────────────────────────────────────────────

fn scaffold_go(dish_abs: &Path, dish_name: &str) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let go_version = detect_go_version();
    let module_path = format!("example.com/{dish_name}");

    let go_mod = format!(
        "module {module_path}\n\
         \n\
         go {go_version}\n"
    );

    let cmd_dir = dish_abs.join("cmd").join(dish_name);
    let main_go = format!(
        "package main\n\
         \n\
         import \"fmt\"\n\
         \n\
         func main() {{\n\
         \tfmt.Println(\"hello from {dish_name}\")\n\
         }}\n"
    );

    let dish_toml = format!(
        "name = \"{dish_name}\"\n\
         language = \"go\"\n\
         \n\
         [tasks.build]\n\
         run = \"go build -o bin/{dish_name} ./cmd/{dish_name}\"\n\
         \n\
         [tasks.test]\n\
         run = \"go test ./...\"\n\
         \n\
         [tasks.lint]\n\
         run = \"go vet ./...\"\n"
    );

    let files = vec![
        write_file(&dish_abs.join("go.mod"), &go_mod)?,
        write_file(&cmd_dir.join("main.go"), &main_go)?,
        write_file(&dish_abs.join("dish.toml"), &dish_toml)?,
    ];

    let rel = dish_abs.display();
    let next_steps = vec![
        format!("cd {rel}"),
        format!("go run ./cmd/{dish_name}"),
        format!("bento build {dish_name}"),
    ];

    Ok((files, next_steps))
}

fn detect_go_version() -> String {
    run_version_command("go", &["version"])
        .and_then(|out| parse_go_version_output(&out))
        .unwrap_or_else(|| "1.22".to_string())
}

fn parse_go_version_output(out: &str) -> Option<String> {
    // "go version go1.22.3 linux/amd64" → "1.22.3"
    // Also handles dev builds like "go1.26.1-X:nodwarf5" by trimming at the
    // first non-`[0-9.]` character.
    let token = out.split_whitespace().find_map(|tok| {
        tok.strip_prefix("go")
            .filter(|s| s.starts_with(|c: char| c.is_ascii_digit()))
    })?;
    let version: String = token
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if version.is_empty() {
        None
    } else {
        Some(version)
    }
}

// ── Node family (npm / pnpm / yarn) scaffold ───────────────────────

/// Which package manager the JS/TS scaffold should target. Drives the
/// `language` id written to `dish.toml`, the `[tasks.*]` commands, and
/// the "next step" install hint.
#[derive(Debug, Clone, Copy)]
enum JsTool {
    Npm,
    Pnpm,
    Yarn,
    Bun,
}

impl JsTool {
    fn language_id(self) -> &'static str {
        match self {
            JsTool::Npm => "node-npm",
            JsTool::Pnpm => "node-pnpm",
            JsTool::Yarn => "node-yarn",
            JsTool::Bun => "bun",
        }
    }
    fn build_cmd(self) -> &'static str {
        match self {
            JsTool::Npm => "npm run build",
            JsTool::Pnpm => "pnpm run build",
            JsTool::Yarn => "yarn build",
            JsTool::Bun => "bun run build",
        }
    }
    fn test_cmd(self) -> &'static str {
        match self {
            JsTool::Npm => "npm test",
            JsTool::Pnpm => "pnpm test",
            JsTool::Yarn => "yarn test",
            // Bun has a built-in test runner — prefer it over the script.
            JsTool::Bun => "bun test",
        }
    }
    fn lint_cmd(self) -> &'static str {
        match self {
            JsTool::Npm => "npm run lint",
            JsTool::Pnpm => "pnpm run lint",
            JsTool::Yarn => "yarn lint",
            JsTool::Bun => "bun run lint",
        }
    }
    fn install_hint(self) -> &'static str {
        match self {
            JsTool::Npm => "npm install  # creates package-lock.json",
            JsTool::Pnpm => "pnpm install  # creates pnpm-lock.yaml",
            JsTool::Yarn => "yarn install  # creates yarn.lock",
            JsTool::Bun => "bun install  # creates bun.lock",
        }
    }
}

fn scaffold_js_family(
    dish_abs: &Path,
    dish_name: &str,
    tool: JsTool,
) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let node_version = detect_node_version();

    let package_json = format!(
        "{{\n  \"name\": \"{dish_name}\",\n  \
         \"version\": \"0.0.1\",\n  \
         \"private\": true,\n  \
         \"type\": \"module\",\n  \
         \"scripts\": {{\n    \
             \"build\": \"tsc\",\n    \
             \"test\": \"node --test\",\n    \
             \"lint\": \"eslint src\"\n  \
         }},\n  \
         \"devDependencies\": {{\n    \
             \"typescript\": \"^5.4.0\"\n  \
         }}\n\
         }}\n"
    );

    let tsconfig = "{\n  \
        \"compilerOptions\": {\n    \
            \"target\": \"ES2022\",\n    \
            \"module\": \"ES2022\",\n    \
            \"moduleResolution\": \"bundler\",\n    \
            \"strict\": true,\n    \
            \"esModuleInterop\": true,\n    \
            \"skipLibCheck\": true,\n    \
            \"outDir\": \"dist\",\n    \
            \"rootDir\": \"src\"\n  \
        },\n  \
        \"include\": [\"src/**/*\"]\n\
        }\n"
    .to_string();

    let index_ts = format!(
        "export function main(): void {{\n  console.log(\"hello from {dish_name}\");\n}}\n\nmain();\n"
    );

    let nvmrc = format!("{node_version}\n");

    let dish_toml = format!(
        "name = \"{dish_name}\"\n\
         language = \"{lang}\"\n\
         \n\
         [tasks.build]\n\
         run = \"{build}\"\n\
         \n\
         [tasks.test]\n\
         run = \"{test}\"\n\
         \n\
         [tasks.lint]\n\
         run = \"{lint}\"\n",
        lang = tool.language_id(),
        build = tool.build_cmd(),
        test = tool.test_cmd(),
        lint = tool.lint_cmd(),
    );

    let files = vec![
        write_file(&dish_abs.join("package.json"), &package_json)?,
        write_file(&dish_abs.join("tsconfig.json"), &tsconfig)?,
        write_file(&dish_abs.join("src/index.ts"), &index_ts)?,
        write_file(&dish_abs.join(".nvmrc"), &nvmrc)?,
        write_file(&dish_abs.join("dish.toml"), &dish_toml)?,
    ];

    let rel = dish_abs.display();
    let next_steps = vec![
        format!("cd {rel}"),
        tool.install_hint().to_string(),
        format!("bento build {dish_name}"),
    ];

    Ok((files, next_steps))
}

// ── Python scaffold ────────────────────────────────────────────────

fn scaffold_python(dish_abs: &Path, dish_name: &str) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let python_version = detect_python_version();
    let module_name = dish_name.replace('-', "_");

    let pyproject = format!(
        "[project]\n\
         name = \"{dish_name}\"\n\
         version = \"0.0.1\"\n\
         requires-python = \">={python_version}\"\n\
         dependencies = []\n\
         \n\
         [build-system]\n\
         requires = [\"setuptools>=61\"]\n\
         build-backend = \"setuptools.build_meta\"\n\
         \n\
         [tool.setuptools.packages.find]\n\
         where = [\"src\"]\n"
    );

    let init_py = format!(
        "def main() -> None:\n    print(\"hello from {module_name}\")\n\n\nif __name__ == \"__main__\":\n    main()\n"
    );

    let python_version_file = format!("{python_version}\n");

    let dish_toml = format!(
        "name = \"{dish_name}\"\n\
         language = \"python\"\n\
         inputs = [\"src/**/*.py\", \"pyproject.toml\"]\n\
         \n\
         [tasks.build]\n\
         run = \"python -m build\"\n\
         \n\
         [tasks.test]\n\
         run = \"pytest\"\n\
         \n\
         [tasks.lint]\n\
         run = \"ruff check .\"\n"
    );

    let files = vec![
        write_file(&dish_abs.join("pyproject.toml"), &pyproject)?,
        write_file(
            &dish_abs.join(format!("src/{module_name}/__init__.py")),
            &init_py,
        )?,
        write_file(&dish_abs.join(".python-version"), &python_version_file)?,
        write_file(&dish_abs.join("dish.toml"), &dish_toml)?,
    ];

    let rel = dish_abs.display();
    let next_steps = vec![
        format!("cd {rel}"),
        "python -m venv .venv && . .venv/bin/activate".to_string(),
        "pip install -e '.[dev]'  # or: pip install -e .".to_string(),
        format!("bento build {dish_name}"),
    ];

    Ok((files, next_steps))
}

fn detect_python_version() -> String {
    // `python --version` → "Python 3.12.1"
    run_version_command("python", &["--version"])
        .or_else(|| run_version_command("python3", &["--version"]))
        .and_then(|out| parse_python_version_output(&out))
        .unwrap_or_else(|| "3.11".to_string())
}

fn parse_python_version_output(out: &str) -> Option<String> {
    let mut tokens = out.split_whitespace();
    if tokens.next()? != "Python" {
        return None;
    }
    let ver = tokens.next()?;
    let trimmed: String = ver
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

// ── Cargo (Rust) scaffold ──────────────────────────────────────────

fn scaffold_cargo(dish_abs: &Path, dish_name: &str) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let rust_version = detect_rust_version();
    let crate_name = dish_name.replace('-', "_");

    let cargo_toml = format!(
        "[package]\n\
         name = \"{dish_name}\"\n\
         version = \"0.1.0\"\n\
         edition = \"2021\"\n\
         \n\
         [dependencies]\n\
         "
    );

    let main_rs = format!("fn main() {{\n    println!(\"hello from {crate_name}\");\n}}\n");

    let toolchain_toml = format!("[toolchain]\nchannel = \"{rust_version}\"\n");

    let dish_toml = format!(
        "name = \"{dish_name}\"\n\
         language = \"cargo\"\n\
         \n\
         [tasks.build]\n\
         run = \"cargo build --locked\"\n\
         \n\
         [tasks.test]\n\
         run = \"cargo test --locked\"\n\
         \n\
         [tasks.lint]\n\
         run = \"cargo clippy --locked --all-targets -- -D warnings\"\n"
    );

    let files = vec![
        write_file(&dish_abs.join("Cargo.toml"), &cargo_toml)?,
        write_file(&dish_abs.join("src/main.rs"), &main_rs)?,
        write_file(&dish_abs.join("rust-toolchain.toml"), &toolchain_toml)?,
        write_file(&dish_abs.join("dish.toml"), &dish_toml)?,
    ];

    let rel = dish_abs.display();
    let next_steps = vec![
        format!("cd {rel}"),
        "cargo run".to_string(),
        format!("bento build {dish_name}"),
    ];

    Ok((files, next_steps))
}

fn detect_rust_version() -> String {
    // `rustc --version` → "rustc 1.82.0 (f6e511eec 2024-10-15)"
    run_version_command("rustc", &["--version"])
        .and_then(|out| parse_rust_version_output(&out))
        .unwrap_or_else(|| "stable".to_string())
}

fn parse_rust_version_output(out: &str) -> Option<String> {
    let mut tokens = out.split_whitespace();
    if tokens.next()? != "rustc" {
        return None;
    }
    let ver = tokens.next()?;
    // Keep only digits + dots; strips any `-nightly` or `-beta` suffixes.
    let trimmed: String = ver
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

// ── Deno scaffold ──────────────────────────────────────────────────

fn scaffold_deno(dish_abs: &Path, dish_name: &str) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let deno_json = format!(
        "{{\n  \"name\": \"@dishes/{dish_name}\",\n  \
         \"version\": \"0.0.1\",\n  \
         \"exports\": \"./src/mod.ts\",\n  \
         \"tasks\": {{\n    \
             \"build\": \"deno check src/mod.ts\",\n    \
             \"start\": \"deno run --allow-net src/mod.ts\"\n  \
         }}\n\
         }}\n"
    );

    let mod_ts = format!(
        "export function main(): void {{\n  console.log(\"hello from {dish_name}\");\n}}\n\nif (import.meta.main) {{\n  main();\n}}\n"
    );

    let dish_toml = format!(
        "name = \"{dish_name}\"\n\
         language = \"deno\"\n\
         \n\
         [tasks.build]\n\
         run = \"deno task build\"\n\
         \n\
         [tasks.test]\n\
         run = \"deno test --allow-read\"\n\
         \n\
         [tasks.lint]\n\
         run = \"deno lint\"\n\
         \n\
         [serve]\n\
         run = \"deno task start\"\n"
    );

    let files = vec![
        write_file(&dish_abs.join("deno.json"), &deno_json)?,
        write_file(&dish_abs.join("src/mod.ts"), &mod_ts)?,
        write_file(&dish_abs.join("dish.toml"), &dish_toml)?,
    ];

    let rel = dish_abs.display();
    let next_steps = vec![
        format!("cd {rel}"),
        "deno task build".to_string(),
        format!("bento build {dish_name}"),
    ];

    Ok((files, next_steps))
}

fn detect_node_version() -> String {
    run_version_command("node", &["-v"])
        .and_then(|out| parse_node_version_output(&out))
        .unwrap_or_else(|| "22".to_string())
}

fn parse_node_version_output(out: &str) -> Option<String> {
    let line = out.lines().next()?.trim();
    let stripped = line.strip_prefix('v').unwrap_or(line);
    if stripped.is_empty() {
        return None;
    }
    Some(stripped.to_string())
}

fn run_version_command(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

// ── Ruby scaffold ──────────────────────────────────────────────────

fn scaffold_ruby(dish_abs: &Path, dish_name: &str) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let ruby_version = detect_ruby_version();
    let module_name = dish_name.replace('-', "_");

    let gemfile = format!(
        "source \"https://rubygems.org\"\n\
         \n\
         ruby \"{ruby_version}\"\n"
    );
    let lib_rb = format!(
        "module {module}\n  \
             GREETING = \"hello from {dish_name}\".freeze\n\
         end\n\
         \n\
         puts {module}::GREETING if __FILE__ == $PROGRAM_NAME\n",
        module = capitalised(&module_name)
    );
    let version_file = format!("{ruby_version}\n");
    let dish_toml = format!(
        "name = \"{dish_name}\"\n\
         language = \"ruby\"\n\
         \n\
         # The ruby adapter's defaults assume rspec + rubocop. This\n\
         # scaffold ships with neither, so we override here. Add\n\
         # `gem \"rspec\"` / `gem \"rubocop\"` to your Gemfile and\n\
         # delete the overrides to inherit the adapter defaults.\n\
         [tasks.build]\n\
         run = \"bundle install\"\n\
         \n\
         [tasks.test]\n\
         run = \"ruby -Ilib -e 'require \\\"{module_name}\\\"'\"\n\
         \n\
         [tasks.lint]\n\
         run = \"ruby -wc lib/{module_name}.rb\"\n"
    );

    let files = vec![
        write_file(&dish_abs.join("Gemfile"), &gemfile)?,
        write_file(&dish_abs.join(format!("lib/{module_name}.rb")), &lib_rb)?,
        write_file(&dish_abs.join(".ruby-version"), &version_file)?,
        write_file(&dish_abs.join("dish.toml"), &dish_toml)?,
    ];

    let rel = dish_abs.display();
    let next_steps = vec![
        format!("cd {rel}"),
        "bundle install".to_string(),
        format!("bento build {dish_name}"),
    ];

    Ok((files, next_steps))
}

fn detect_ruby_version() -> String {
    run_version_command("ruby", &["--version"])
        .and_then(|out| parse_ruby_version_output(&out))
        .unwrap_or_else(|| "3.2".to_string())
}

fn parse_ruby_version_output(out: &str) -> Option<String> {
    // "ruby 3.2.2 (2023-...) [...]" → "3.2.2"
    let mut tokens = out.split_whitespace();
    if tokens.next()? != "ruby" {
        return None;
    }
    let raw = tokens.next()?;
    let trimmed: String = raw
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

// ── PHP scaffold ───────────────────────────────────────────────────

fn scaffold_php(dish_abs: &Path, dish_name: &str) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let php_version = detect_php_version();
    let class_name = capitalised(&dish_name.replace('-', ""));

    let composer_json = format!(
        "{{\n  \"name\": \"acme/{dish_name}\",\n  \
         \"description\": \"{dish_name} dish\",\n  \
         \"type\": \"project\",\n  \
         \"require\": {{\n    \"php\": \">={php_version}\"\n  }},\n  \
         \"autoload\": {{\n    \"psr-4\": {{\n      \"App\\\\\": \"src/\"\n    }}\n  }}\n\
         }}\n"
    );
    let hello_php = format!(
        "<?php\n\
         declare(strict_types=1);\n\
         \n\
         namespace App;\n\
         \n\
         final class {class_name}\n\
         {{\n    \
             public function hello(): string\n    \
             {{\n        return 'hello from {dish_name}';\n    }}\n\
         }}\n"
    );
    let version_file = format!("{php_version}\n");
    let dish_toml = format!(
        "name = \"{dish_name}\"\n\
         language = \"php\"\n\
         \n\
         # The php adapter's defaults assume phpunit + phpstan. This\n\
         # scaffold ships with neither, so we override here. Add the\n\
         # tools to composer.json's require-dev and delete the overrides\n\
         # to inherit the adapter defaults.\n\
         [tasks.build]\n\
         run = \"composer install\"\n\
         \n\
         [tasks.test]\n\
         run = \"php -r 'require \\\"vendor/autoload.php\\\"; new App\\\\{class_name}();'\"\n\
         \n\
         [tasks.lint]\n\
         run = \"php -l src/{class_name}.php\"\n"
    );

    let files = vec![
        write_file(&dish_abs.join("composer.json"), &composer_json)?,
        write_file(&dish_abs.join(format!("src/{class_name}.php")), &hello_php)?,
        write_file(&dish_abs.join(".php-version"), &version_file)?,
        write_file(&dish_abs.join("dish.toml"), &dish_toml)?,
    ];

    let rel = dish_abs.display();
    let next_steps = vec![
        format!("cd {rel}"),
        "composer install".to_string(),
        format!("bento build {dish_name}"),
    ];

    Ok((files, next_steps))
}

fn detect_php_version() -> String {
    run_version_command("php", &["--version"])
        .and_then(|out| parse_php_version_output(&out))
        .unwrap_or_else(|| "8.2".to_string())
}

fn parse_php_version_output(out: &str) -> Option<String> {
    // "PHP 8.2.10 (cli) (built: ...)" → "8.2.10"
    let line = out.lines().next()?;
    let mut tokens = line.split_whitespace();
    if tokens.next()? != "PHP" {
        return None;
    }
    let raw = tokens.next()?;
    let trimmed: String = raw
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

// ── Maven scaffold ─────────────────────────────────────────────────

fn scaffold_maven(dish_abs: &Path, dish_name: &str) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let java_version = detect_java_version();
    let pkg_segment = java_package_segment(dish_name);
    let pkg = format!("com.example.{pkg_segment}");
    let pkg_path = pkg.replace('.', "/");

    let pom_xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <project xmlns=\"http://maven.apache.org/POM/4.0.0\"\n\
         \x20        xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"\n\
         \x20        xsi:schemaLocation=\"http://maven.apache.org/POM/4.0.0 http://maven.apache.org/xsd/maven-4.0.0.xsd\">\n\
         \x20 <modelVersion>4.0.0</modelVersion>\n\
         \x20 <groupId>com.example</groupId>\n\
         \x20 <artifactId>{dish_name}</artifactId>\n\
         \x20 <version>0.1.0-SNAPSHOT</version>\n\
         \x20 <packaging>jar</packaging>\n\
         \x20 <properties>\n\
         \x20   <maven.compiler.release>{java_version}</maven.compiler.release>\n\
         \x20   <project.build.sourceEncoding>UTF-8</project.build.sourceEncoding>\n\
         \x20 </properties>\n\
         </project>\n"
    );
    let hello_java = format!(
        "package {pkg};\n\
         \n\
         public final class Hello {{\n    \
             public static void main(String[] args) {{\n        \
                 System.out.println(\"hello from {dish_name}\");\n    \
             }}\n\
         }}\n"
    );
    let version_file = format!("{java_version}\n");
    // dish.toml is empty of [tasks.*] — the maven adapter's defaults
    // (mvn package -DskipTests / mvn test / mvn verify -DskipTests) are
    // exactly right for a fresh project.
    let dish_toml = format!(
        "name = \"{dish_name}\"\n\
         language = \"maven\"\n\
         \n\
         # Adapter defaults for maven cover build / test / lint.\n\
         # Override them by adding [tasks.<name>] blocks here.\n"
    );

    let files = vec![
        write_file(&dish_abs.join("pom.xml"), &pom_xml)?,
        write_file(
            &dish_abs.join(format!("src/main/java/{pkg_path}/Hello.java")),
            &hello_java,
        )?,
        write_file(&dish_abs.join(".java-version"), &version_file)?,
        write_file(&dish_abs.join("dish.toml"), &dish_toml)?,
    ];

    let rel = dish_abs.display();
    let next_steps = vec![
        format!("cd {rel}"),
        "mvn package".to_string(),
        format!("bento build {dish_name}"),
    ];

    Ok((files, next_steps))
}

// ── Gradle scaffold ────────────────────────────────────────────────

fn scaffold_gradle(dish_abs: &Path, dish_name: &str) -> Result<(Vec<PathBuf>, Vec<String>)> {
    let java_version = detect_java_version();
    let pkg_segment = java_package_segment(dish_name);
    let pkg = format!("com.example.{pkg_segment}");
    let pkg_path = pkg.replace('.', "/");

    let settings_kts = format!("rootProject.name = \"{dish_name}\"\n");
    let build_kts = format!(
        "plugins {{\n    \
             application\n    \
             id(\"java\")\n\
         }}\n\
         \n\
         java {{\n    \
             toolchain {{\n        \
                 languageVersion = JavaLanguageVersion.of({java_version})\n    \
             }}\n\
         }}\n\
         \n\
         repositories {{\n    \
             mavenCentral()\n\
         }}\n\
         \n\
         application {{\n    \
             mainClass.set(\"{pkg}.Hello\")\n\
         }}\n"
    );
    let hello_java = format!(
        "package {pkg};\n\
         \n\
         public final class Hello {{\n    \
             public static void main(String[] args) {{\n        \
                 System.out.println(\"hello from {dish_name}\");\n    \
             }}\n\
         }}\n"
    );
    let version_file = format!("{java_version}\n");
    // The gradle adapter's defaults use ./gradlew, but we don't generate
    // the wrapper in scaffold mode (would need to embed the binary jar).
    // Override to use system `gradle` until the user runs `gradle wrapper`.
    let dish_toml = format!(
        "name = \"{dish_name}\"\n\
         language = \"gradle\"\n\
         \n\
         # Scaffold uses system `gradle` — once you've run `gradle wrapper`\n\
         # to materialise gradlew + gradle-wrapper.{{jar,properties}}, delete\n\
         # these [tasks.*] overrides to inherit the adapter defaults\n\
         # (./gradlew build -x test, ./gradlew test, ./gradlew check -x test).\n\
         [tasks.build]\n\
         run = \"gradle build -x test\"\n\
         \n\
         [tasks.test]\n\
         run = \"gradle test\"\n\
         \n\
         [tasks.lint]\n\
         run = \"gradle check -x test\"\n"
    );

    let files = vec![
        write_file(&dish_abs.join("settings.gradle.kts"), &settings_kts)?,
        write_file(&dish_abs.join("build.gradle.kts"), &build_kts)?,
        write_file(
            &dish_abs.join(format!("src/main/java/{pkg_path}/Hello.java")),
            &hello_java,
        )?,
        write_file(&dish_abs.join(".java-version"), &version_file)?,
        write_file(&dish_abs.join("dish.toml"), &dish_toml)?,
    ];

    let rel = dish_abs.display();
    let next_steps = vec![
        format!("cd {rel}"),
        "gradle wrapper  # generate ./gradlew, then drop the dish.toml overrides".to_string(),
        "gradle run".to_string(),
        format!("bento build {dish_name}"),
    ];

    Ok((files, next_steps))
}

// ── Java version helper (shared between Maven + Gradle scaffolds) ──

fn detect_java_version() -> String {
    // `java -version` writes to stderr.
    let output = Command::new("java").arg("-version").output().ok();
    if let Some(o) = output {
        if o.status.success() {
            let mut text = String::from_utf8_lossy(&o.stderr).to_string();
            if text.trim().is_empty() {
                text = String::from_utf8_lossy(&o.stdout).to_string();
            }
            if let Some(v) = parse_java_version_output(&text) {
                return v;
            }
        }
    }
    "21".to_string()
}

fn parse_java_version_output(out: &str) -> Option<String> {
    // Common shapes:
    //   openjdk version "21.0.2" 2024-01-16
    //   openjdk version "1.8.0_412"
    //   java version "17.0.10" 2024-01-16 LTS
    let line = out.lines().next()?;
    let after_quote = line.split_once('"')?.1;
    let raw = after_quote.split_once('"')?.0;
    // Pull the first major.minor (or major). Examples:
    //   "21.0.2"  → "21"
    //   "17.0.10" → "17"
    //   "1.8.0_412" → "1.8"  (legacy 1.x prefix)
    let major: String = raw.chars().take_while(|c| c.is_ascii_digit()).collect();
    if major.is_empty() {
        return None;
    }
    // For the 1.x family (Java 8 and earlier), keep "1.8" rather than "1".
    if major == "1" {
        let rest = &raw[major.len()..];
        if let Some(stripped) = rest.strip_prefix('.') {
            let minor: String = stripped
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if !minor.is_empty() {
                return Some(format!("1.{minor}"));
            }
        }
    }
    Some(major)
}

/// Convert a dish name (potentially with dashes) into a valid Java
/// package segment — lowercase + dashes stripped + leading-digit guard.
fn java_package_segment(dish_name: &str) -> String {
    let stripped: String = dish_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    if stripped.is_empty() {
        "app".to_string()
    } else if stripped
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        format!("a{stripped}")
    } else {
        stripped
    }
}

fn capitalised(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut next_upper = true;
    for c in s.chars() {
        if c == '_' || c == '-' {
            next_upper = true;
        } else if next_upper {
            out.extend(c.to_uppercase());
            next_upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

// ── shared IO helpers ──────────────────────────────────────────────

fn write_file(path: &Path, contents: &str) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    Ok(path.to_path_buf())
}

/// Append `dish_rel` to a bento TOML's `dishes` array, preserving comments
/// and existing formatting. If already present, this is a no-op.
fn wire_into_bento(bento_toml: &Path, dish_rel: &str) -> Result<()> {
    let raw = std::fs::read_to_string(bento_toml)
        .with_context(|| format!("reading {}", bento_toml.display()))?;
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .with_context(|| format!("parsing {}", bento_toml.display()))?;

    let dishes = doc
        .get_mut("dishes")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| ScaffoldError::BentoConfigShape {
            path: bento_toml.display().to_string(),
        })?;

    let already_present = dishes
        .iter()
        .any(|v| v.as_str().map(|s| s == dish_rel).unwrap_or(false));
    if already_present {
        return Ok(());
    }

    dishes.push(dish_rel);

    std::fs::write(bento_toml, doc.to_string())
        .with_context(|| format!("writing {}", bento_toml.display()))?;
    Ok(())
}

// ── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
    }

    fn fixture_with_bento(bento_toml_body: &str) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("bentos")).unwrap();
        std::fs::write(root.join("bentos/prod.toml"), bento_toml_body).unwrap();
        // Pre-existing dish so the bento is valid on load.
        let seed = root.join("apps/seed");
        std::fs::create_dir_all(&seed).unwrap();
        std::fs::write(seed.join("dish.toml"), r#"name = "seed""#).unwrap();
        Fixture { _tmp: tmp, root }
    }

    fn default_fixture() -> Fixture {
        fixture_with_bento("name = \"prod\"\n# preserve me\ndishes = [\"apps/seed\"]\n")
    }

    fn load(root: &Path) -> Workspace {
        Workspace::load(root).unwrap()
    }

    #[test]
    fn go_scaffold_creates_expected_files() {
        let fx = default_fixture();
        let ws = load(&fx.root);
        let res = run(
            ScaffoldRequest {
                workspace_root: &fx.root,
                dish_rel: Path::new("apps/api"),
                language: Some("go"),
                bento: None,
            },
            &ws,
        )
        .unwrap();

        assert_eq!(res.dish_name, "api");
        assert_eq!(res.bento_name, "prod");
        assert!(fx.root.join("apps/api/dish.toml").exists());
        assert!(fx.root.join("apps/api/go.mod").exists());
        assert!(fx.root.join("apps/api/cmd/api/main.go").exists());

        let gomod = std::fs::read_to_string(fx.root.join("apps/api/go.mod")).unwrap();
        assert!(gomod.contains("module example.com/api"));
        assert!(gomod.contains("go "));

        let dish = std::fs::read_to_string(fx.root.join("apps/api/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "go""#));
        assert!(dish.contains("go build -o bin/api ./cmd/api"));
    }

    #[test]
    fn node_npm_scaffold_creates_expected_files() {
        let fx = default_fixture();
        let ws = load(&fx.root);
        run(
            ScaffoldRequest {
                workspace_root: &fx.root,
                dish_rel: Path::new("apps/web"),
                language: Some("node-npm"),
                bento: None,
            },
            &ws,
        )
        .unwrap();

        assert!(fx.root.join("apps/web/package.json").exists());
        assert!(fx.root.join("apps/web/tsconfig.json").exists());
        assert!(fx.root.join("apps/web/src/index.ts").exists());
        assert!(fx.root.join("apps/web/.nvmrc").exists());

        let pkg = std::fs::read_to_string(fx.root.join("apps/web/package.json")).unwrap();
        assert!(pkg.contains("\"name\": \"web\""));
        let dish = std::fs::read_to_string(fx.root.join("apps/web/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "node-npm""#));
        assert!(dish.contains("npm run build"));
    }

    #[test]
    fn pnpm_scaffold_uses_pnpm_commands() {
        let fx = default_fixture();
        let ws = load(&fx.root);
        run(
            ScaffoldRequest {
                workspace_root: &fx.root,
                dish_rel: Path::new("apps/web"),
                language: Some("node-pnpm"),
                bento: None,
            },
            &ws,
        )
        .unwrap();

        let dish = std::fs::read_to_string(fx.root.join("apps/web/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "node-pnpm""#), "{dish}");
        assert!(dish.contains("pnpm run build"));
        assert!(dish.contains("pnpm test"));
    }

    #[test]
    fn bun_scaffold_uses_bun_runtime() {
        let fx = default_fixture();
        let ws = load(&fx.root);
        run(
            ScaffoldRequest {
                workspace_root: &fx.root,
                dish_rel: Path::new("apps/web"),
                language: Some("bun"),
                bento: None,
            },
            &ws,
        )
        .unwrap();

        let dish = std::fs::read_to_string(fx.root.join("apps/web/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "bun""#));
        assert!(dish.contains("bun test"));
        assert!(dish.contains("bun run build"));
    }

    #[test]
    fn cargo_scaffold_creates_cargo_project() {
        let fx = default_fixture();
        let ws = load(&fx.root);
        run(
            ScaffoldRequest {
                workspace_root: &fx.root,
                dish_rel: Path::new("apps/api"),
                language: Some("cargo"),
                bento: None,
            },
            &ws,
        )
        .unwrap();

        assert!(fx.root.join("apps/api/Cargo.toml").exists());
        assert!(fx.root.join("apps/api/src/main.rs").exists());
        assert!(fx.root.join("apps/api/rust-toolchain.toml").exists());

        let cargo = std::fs::read_to_string(fx.root.join("apps/api/Cargo.toml")).unwrap();
        assert!(cargo.contains("name = \"api\""));
        assert!(cargo.contains("edition = \"2021\""));
        let dish = std::fs::read_to_string(fx.root.join("apps/api/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "cargo""#));
        assert!(dish.contains("cargo clippy"));
    }

    #[test]
    fn python_scaffold_creates_package_layout() {
        let fx = default_fixture();
        let ws = load(&fx.root);
        run(
            ScaffoldRequest {
                workspace_root: &fx.root,
                dish_rel: Path::new("apps/svc"),
                language: Some("python"),
                bento: None,
            },
            &ws,
        )
        .unwrap();

        assert!(fx.root.join("apps/svc/pyproject.toml").exists());
        assert!(fx.root.join("apps/svc/.python-version").exists());
        assert!(fx.root.join("apps/svc/src/svc/__init__.py").exists());

        let pyproj = std::fs::read_to_string(fx.root.join("apps/svc/pyproject.toml")).unwrap();
        assert!(pyproj.contains("name = \"svc\""));
        assert!(pyproj.contains("requires-python"));
        let dish = std::fs::read_to_string(fx.root.join("apps/svc/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "python""#));
        assert!(dish.contains("pytest"));
    }

    #[test]
    fn parse_python_version_handles_python3() {
        assert_eq!(
            parse_python_version_output("Python 3.12.1\n").as_deref(),
            Some("3.12.1")
        );
        assert_eq!(
            parse_python_version_output("Python 3.11\n").as_deref(),
            Some("3.11")
        );
        assert!(parse_python_version_output("").is_none());
    }

    #[test]
    fn parse_rust_version_strips_channel_suffix() {
        assert_eq!(
            parse_rust_version_output("rustc 1.82.0 (f6e511eec 2024-10-15)").as_deref(),
            Some("1.82.0"),
        );
        assert_eq!(
            parse_rust_version_output("rustc 1.83.0-nightly (abc 2024-12-10)").as_deref(),
            Some("1.83.0"),
        );
    }

    #[test]
    fn deno_scaffold_uses_deno_json() {
        let fx = default_fixture();
        let ws = load(&fx.root);
        run(
            ScaffoldRequest {
                workspace_root: &fx.root,
                dish_rel: Path::new("apps/web"),
                language: Some("deno"),
                bento: None,
            },
            &ws,
        )
        .unwrap();

        assert!(fx.root.join("apps/web/deno.json").exists());
        assert!(fx.root.join("apps/web/src/mod.ts").exists());

        let dish = std::fs::read_to_string(fx.root.join("apps/web/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "deno""#));
        assert!(dish.contains("deno test"));
        assert!(dish.contains("deno lint"));
    }

    #[test]
    fn yarn_scaffold_uses_yarn_commands() {
        let fx = default_fixture();
        let ws = load(&fx.root);
        run(
            ScaffoldRequest {
                workspace_root: &fx.root,
                dish_rel: Path::new("apps/web"),
                language: Some("node-yarn"),
                bento: None,
            },
            &ws,
        )
        .unwrap();

        let dish = std::fs::read_to_string(fx.root.join("apps/web/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "node-yarn""#));
        assert!(dish.contains("yarn build"));
        assert!(
            !dish.contains("yarn run build"),
            "yarn scaffold must not prefix run"
        );
    }

    #[test]
    fn ruby_scaffold_creates_expected_files() {
        let fx = default_fixture();
        let ws = load(&fx.root);
        run(
            ScaffoldRequest {
                workspace_root: &fx.root,
                dish_rel: Path::new("apps/notifier"),
                language: Some("ruby"),
                bento: None,
            },
            &ws,
        )
        .unwrap();

        assert!(fx.root.join("apps/notifier/Gemfile").exists());
        assert!(fx.root.join("apps/notifier/.ruby-version").exists());
        assert!(fx.root.join("apps/notifier/lib/notifier.rb").exists());

        let gemfile = std::fs::read_to_string(fx.root.join("apps/notifier/Gemfile")).unwrap();
        assert!(gemfile.contains("source \"https://rubygems.org\""));
        assert!(gemfile.contains("ruby \""));

        let lib = std::fs::read_to_string(fx.root.join("apps/notifier/lib/notifier.rb")).unwrap();
        assert!(lib.contains("module Notifier"));
        assert!(lib.contains("hello from notifier"));

        let dish = std::fs::read_to_string(fx.root.join("apps/notifier/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "ruby""#));
        assert!(dish.contains("bundle install"));
    }

    #[test]
    fn php_scaffold_creates_expected_files() {
        let fx = default_fixture();
        let ws = load(&fx.root);
        run(
            ScaffoldRequest {
                workspace_root: &fx.root,
                dish_rel: Path::new("apps/billing"),
                language: Some("php"),
                bento: None,
            },
            &ws,
        )
        .unwrap();

        assert!(fx.root.join("apps/billing/composer.json").exists());
        assert!(fx.root.join("apps/billing/.php-version").exists());
        assert!(fx.root.join("apps/billing/src/Billing.php").exists());

        let composer = std::fs::read_to_string(fx.root.join("apps/billing/composer.json")).unwrap();
        assert!(composer.contains("\"name\": \"acme/billing\""));
        assert!(composer.contains("\"php\":"));

        let php = std::fs::read_to_string(fx.root.join("apps/billing/src/Billing.php")).unwrap();
        assert!(php.contains("namespace App;"));
        assert!(php.contains("final class Billing"));

        let dish = std::fs::read_to_string(fx.root.join("apps/billing/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "php""#));
        assert!(dish.contains("composer install"));
    }

    #[test]
    fn maven_scaffold_creates_expected_files() {
        let fx = default_fixture();
        let ws = load(&fx.root);
        run(
            ScaffoldRequest {
                workspace_root: &fx.root,
                dish_rel: Path::new("apps/scoring"),
                language: Some("maven"),
                bento: None,
            },
            &ws,
        )
        .unwrap();

        assert!(fx.root.join("apps/scoring/pom.xml").exists());
        assert!(fx.root.join("apps/scoring/.java-version").exists());
        assert!(fx
            .root
            .join("apps/scoring/src/main/java/com/example/scoring/Hello.java")
            .exists());

        let pom = std::fs::read_to_string(fx.root.join("apps/scoring/pom.xml")).unwrap();
        assert!(pom.contains("<artifactId>scoring</artifactId>"));
        assert!(pom.contains("<maven.compiler.release>"));

        let java = std::fs::read_to_string(
            fx.root
                .join("apps/scoring/src/main/java/com/example/scoring/Hello.java"),
        )
        .unwrap();
        assert!(java.contains("package com.example.scoring;"));

        let dish = std::fs::read_to_string(fx.root.join("apps/scoring/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "maven""#));
        // Adapter defaults — no [tasks.*] section header expected. (The
        // string "[tasks." appears in the generated comment text.)
        assert!(!dish.contains("\n[tasks."));
    }

    #[test]
    fn gradle_scaffold_creates_expected_files() {
        let fx = default_fixture();
        let ws = load(&fx.root);
        run(
            ScaffoldRequest {
                workspace_root: &fx.root,
                dish_rel: Path::new("apps/router"),
                language: Some("gradle"),
                bento: None,
            },
            &ws,
        )
        .unwrap();

        assert!(fx.root.join("apps/router/settings.gradle.kts").exists());
        assert!(fx.root.join("apps/router/build.gradle.kts").exists());
        assert!(fx.root.join("apps/router/.java-version").exists());
        assert!(fx
            .root
            .join("apps/router/src/main/java/com/example/router/Hello.java")
            .exists());

        let build = std::fs::read_to_string(fx.root.join("apps/router/build.gradle.kts")).unwrap();
        assert!(build.contains("JavaLanguageVersion.of("));
        assert!(build.contains("mainClass.set(\"com.example.router.Hello\")"));

        let dish = std::fs::read_to_string(fx.root.join("apps/router/dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "gradle""#));
        // Scaffold uses system `gradle` not `./gradlew` (no wrapper generated).
        assert!(dish.contains("run = \"gradle build -x test\""));
    }

    #[test]
    fn parse_ruby_version_extracts_dotted_version() {
        assert_eq!(
            parse_ruby_version_output(
                "ruby 3.2.2 (2023-03-30 revision e51014f9c0) [aarch64-linux]"
            ),
            Some("3.2.2".into())
        );
        assert_eq!(parse_ruby_version_output(""), None);
        assert_eq!(parse_ruby_version_output("python 3.12"), None);
    }

    #[test]
    fn parse_php_version_extracts_dotted_version() {
        assert_eq!(
            parse_php_version_output(
                "PHP 8.2.10 (cli) (built: Aug 28 2023 10:30:55) (NTS)\nCopyright (c) The PHP Group"
            ),
            Some("8.2.10".into())
        );
        assert_eq!(parse_php_version_output("ruby 3.2"), None);
    }

    #[test]
    fn parse_java_version_handles_modern_and_legacy() {
        assert_eq!(
            parse_java_version_output("openjdk version \"21.0.2\" 2024-01-16\n"),
            Some("21".into())
        );
        assert_eq!(
            parse_java_version_output("java version \"17.0.10\" 2024-01-16 LTS\n"),
            Some("17".into())
        );
        // Java 8 and earlier kept the leading "1.x" form.
        assert_eq!(
            parse_java_version_output("openjdk version \"1.8.0_412\"\n"),
            Some("1.8".into())
        );
    }

    #[test]
    fn java_package_segment_strips_dashes_and_lowercases() {
        assert_eq!(java_package_segment("my-api"), "myapi");
        assert_eq!(java_package_segment("BillingService"), "billingservice");
    }

    #[test]
    fn java_package_segment_prefixes_leading_digit() {
        // Java identifiers can't start with a digit.
        assert_eq!(java_package_segment("3rdparty"), "a3rdparty");
    }

    #[test]
    fn java_package_segment_falls_back_to_app_when_empty() {
        assert_eq!(java_package_segment("---"), "app");
    }

    #[test]
    fn capitalised_handles_kebab_and_snake() {
        assert_eq!(capitalised("my-api"), "MyApi");
        assert_eq!(capitalised("notifier_service"), "NotifierService");
        assert_eq!(capitalised("simple"), "Simple");
    }

    #[test]
    fn wires_dish_into_bento_and_preserves_comment() {
        let fx = default_fixture();
        let ws = load(&fx.root);
        run(
            ScaffoldRequest {
                workspace_root: &fx.root,
                dish_rel: Path::new("apps/api"),
                language: Some("go"),
                bento: None,
            },
            &ws,
        )
        .unwrap();

        let bento = std::fs::read_to_string(fx.root.join("bentos/prod.toml")).unwrap();
        assert!(bento.contains("apps/api"));
        assert!(bento.contains("apps/seed"));
        assert!(
            bento.contains("# preserve me"),
            "comment must survive: {bento}"
        );

        // Re-loading the workspace with the new dish must succeed.
        let ws2 = Workspace::load(&fx.root).unwrap();
        assert!(ws2.dishes_by_name.contains_key("api"));
    }

    #[test]
    fn errors_when_language_unknown() {
        let fx = default_fixture();
        let ws = load(&fx.root);
        let err = run(
            ScaffoldRequest {
                workspace_root: &fx.root,
                dish_rel: Path::new("apps/rusty"),
                language: Some("rust"),
                bento: None,
            },
            &ws,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unsupported language"));
    }

    #[test]
    fn non_empty_dir_without_adapter_errors_with_language_unknown() {
        // Adoption mode: a non-empty dir with no recognisable manifest
        // and no --lang should produce a friendly LanguageUnknown error.
        let fx = default_fixture();
        std::fs::create_dir_all(fx.root.join("apps/api")).unwrap();
        std::fs::write(fx.root.join("apps/api/stray.txt"), "hi").unwrap();
        let ws = load(&fx.root);
        let err = run(
            ScaffoldRequest {
                workspace_root: &fx.root,
                dish_rel: Path::new("apps/api"),
                language: None,
                bento: None,
            },
            &ws,
        )
        .unwrap_err();
        assert!(err.to_string().contains("auto-detect"), "got: {err}");
    }

    #[test]
    fn non_empty_dir_with_go_manifest_is_adopted() {
        let fx = default_fixture();
        let api = fx.root.join("apps/api");
        std::fs::create_dir_all(&api).unwrap();
        std::fs::write(api.join("go.mod"), "module example.com/api\n\ngo 1.22\n").unwrap();
        std::fs::write(api.join("main.go"), "package main\nfunc main() {}\n").unwrap();
        let ws = load(&fx.root);

        let result = run(
            ScaffoldRequest {
                workspace_root: &fx.root,
                dish_rel: Path::new("apps/api"),
                language: None,
                bento: None,
            },
            &ws,
        )
        .unwrap();

        assert!(matches!(result.mode, ScaffoldMode::Adopted));
        assert_eq!(result.language, "go");
        // Source files must be untouched; only dish.toml is new.
        assert_eq!(
            std::fs::read_to_string(api.join("main.go"))
                .unwrap()
                .lines()
                .count(),
            2
        );
        let dish = std::fs::read_to_string(api.join("dish.toml")).unwrap();
        assert!(dish.contains(r#"language = "go""#), "got: {dish}");
    }

    #[test]
    fn adoption_refuses_when_dish_toml_already_exists() {
        let fx = default_fixture();
        let api = fx.root.join("apps/api");
        std::fs::create_dir_all(&api).unwrap();
        std::fs::write(api.join("go.mod"), "module x\n").unwrap();
        std::fs::write(api.join("dish.toml"), r#"name = "api""#).unwrap();
        let ws = load(&fx.root);
        let err = run(
            ScaffoldRequest {
                workspace_root: &fx.root,
                dish_rel: Path::new("apps/api"),
                language: None,
                bento: None,
            },
            &ws,
        )
        .unwrap_err();
        assert!(err.to_string().contains("already exists"), "got: {err}");
    }

    #[test]
    fn dot_path_adopts_as_workspace_root_basename() {
        // Single-repo case: `bento dish add .` adopts the workspace root.
        let tmp = tempfile::tempdir().unwrap();
        // Name the workspace dir "myapp" so we can assert on the dish name.
        let root = tmp.path().join("myapp");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir(root.join("bentos")).unwrap();
        std::fs::write(
            root.join("bentos/prod.toml"),
            "name = \"prod\"\ndishes = []\n",
        )
        .unwrap();
        std::fs::write(root.join("go.mod"), "module example.com/myapp\ngo 1.22\n").unwrap();
        std::fs::write(root.join("main.go"), "package main\nfunc main() {}\n").unwrap();

        let ws = Workspace::load(&root).unwrap();
        let result = run(
            ScaffoldRequest {
                workspace_root: &root,
                dish_rel: Path::new("."),
                language: None,
                bento: None,
            },
            &ws,
        )
        .unwrap();

        assert_eq!(result.dish_name, "myapp");
        assert!(matches!(result.mode, ScaffoldMode::Adopted));
        assert_eq!(result.language, "go");

        // Re-loading the workspace finds the dish at `.`.
        let ws2 = Workspace::load(&root).unwrap();
        assert!(ws2.dishes_by_name.contains_key("myapp"));
    }

    #[test]
    fn errors_when_dish_name_collides() {
        let fx = default_fixture();
        let ws = load(&fx.root);
        let err = run(
            ScaffoldRequest {
                workspace_root: &fx.root,
                dish_rel: Path::new("apps/seed"),
                language: Some("go"),
                bento: None,
            },
            &ws,
        )
        .unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn requires_bento_when_multiple_defined() {
        let fx = fixture_with_bento("name = \"prod\"\ndishes = [\"apps/seed\"]\n");
        std::fs::write(
            fx.root.join("bentos/staging.toml"),
            "name = \"staging\"\ndishes = [\"apps/seed\"]\n",
        )
        .unwrap();

        let ws = load(&fx.root);
        let err = run(
            ScaffoldRequest {
                workspace_root: &fx.root,
                dish_rel: Path::new("apps/api"),
                language: Some("go"),
                bento: None,
            },
            &ws,
        )
        .unwrap_err();
        assert!(err.to_string().contains("multiple bentos"));
    }

    #[test]
    fn errors_when_no_bentos_defined() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Workspace::load(tmp.path()).unwrap();
        let err = run(
            ScaffoldRequest {
                workspace_root: tmp.path(),
                dish_rel: Path::new("apps/api"),
                language: Some("go"),
                bento: None,
            },
            &ws,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no bentos defined"));
    }

    #[test]
    fn errors_for_unknown_bento_name() {
        let fx = default_fixture();
        let ws = load(&fx.root);
        let err = run(
            ScaffoldRequest {
                workspace_root: &fx.root,
                dish_rel: Path::new("apps/api"),
                language: Some("go"),
                bento: Some("nope"),
            },
            &ws,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no bento named 'nope'"));
    }

    #[test]
    fn parse_go_version_output_extracts_patch_version() {
        let v = parse_go_version_output("go version go1.22.3 linux/amd64").unwrap();
        assert_eq!(v, "1.22.3");
    }

    #[test]
    fn parse_go_version_output_trims_dev_suffix() {
        // Dev/custom builds embed a suffix after the semver.
        let v = parse_go_version_output("go version go1.26.1-X:nodwarf5 linux/amd64").unwrap();
        assert_eq!(v, "1.26.1");
    }

    #[test]
    fn parse_go_version_output_none_on_empty() {
        assert!(parse_go_version_output("").is_none());
    }

    #[test]
    fn parse_node_version_output_strips_v() {
        let v = parse_node_version_output("v22.1.0\n").unwrap();
        assert_eq!(v, "22.1.0");
    }
}
