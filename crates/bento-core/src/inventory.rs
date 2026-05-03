//! Workspace inventory data for `bento dish list` + `bento box list`.
//!
//! Pure read: `Workspace` in, inventory out. Shared by the CLI's
//! list verbs and by the `bento-mcp` tools — both call the same
//! functions, serialise to the same JSON shape.

use schemars::JsonSchema;
use serde::Serialize;

use bento_config::Workspace;

use crate::scan_orphan_dishes;

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DishListOutput {
    pub dishes: Vec<DishListItem>,
    /// Workspace-relative paths of `dish.toml` files on disk that
    /// aren't wired into any bento. Agents should surface these so
    /// the user can `bento dish add <path>` — or they'll be invisible
    /// to `bento plan`.
    pub orphans: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DishListItem {
    /// Dish name from dish.toml.
    pub name: String,
    /// Workspace-relative path to the dish directory.
    pub path: String,
    /// Language id (e.g. "bun", "cargo"), if declared.
    pub language: Option<String>,
    /// Names of bentos that include this dish.
    pub bentos: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BoxListOutput {
    pub bentos: Vec<BoxListItem>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BoxListItem {
    pub name: String,
    /// Workspace-relative path to the bento source file
    /// (e.g. `bentos/prod.toml`).
    pub source: String,
    /// Dish paths this bento includes, verbatim from `dishes = [...]`.
    pub dishes: Vec<String>,
}

/// Build the dish list for a loaded workspace.
pub fn dish_list(workspace: &Workspace) -> DishListOutput {
    let mut dishes: Vec<DishListItem> = workspace
        .dishes_by_name
        .values()
        .map(|d| {
            let rel = d.rel.to_string_lossy().to_string();
            let bentos = workspace
                .bentos
                .values()
                .filter(|b| b.config.dishes.iter().any(|dp| dp == &rel))
                .map(|b| b.config.name.clone())
                .collect();
            DishListItem {
                name: d.config.name.clone(),
                path: rel,
                language: d.config.language.clone(),
                bentos,
            }
        })
        .collect();
    dishes.sort_by(|a, b| a.name.cmp(&b.name));

    let orphans = scan_orphan_dishes(workspace)
        .into_iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();

    DishListOutput { dishes, orphans }
}

/// Build the box (bento) list for a loaded workspace.
pub fn box_list(workspace: &Workspace) -> BoxListOutput {
    let mut bentos: Vec<BoxListItem> = workspace
        .bentos
        .values()
        .map(|b| BoxListItem {
            name: b.config.name.clone(),
            source: b
                .source
                .strip_prefix(&workspace.root)
                .unwrap_or(&b.source)
                .to_string_lossy()
                .to_string(),
            dishes: b.config.dishes.clone(),
        })
        .collect();
    bentos.sort_by(|a, b| a.name.cmp(&b.name));

    BoxListOutput { bentos }
}
