//! `bento schema [target]` — emit JSON Schema for bento's agent outputs.
//!
//! The schemas are the stable integration contract. Field names and shapes
//! do not change within a major version. See `bento schema` (no target)
//! for the list of schemas and a short description of each.

use anyhow::Result;
use schemars::{schema_for, JsonSchema};
use serde::Serialize;

use bento_core::{Diagnostic, DoctorReport, ExecutionReport, GarnishPayload, InputManifest, Plan};

use bento_core::prime::Output as PrimeOutput;
use bento_core::why::Explanation;

use crate::cli::SchemaTarget;
use crate::errors::BentoError;
use crate::scaffold::ScaffoldResult;

#[derive(Debug, Serialize)]
struct SchemaListing {
    name: &'static str,
    command: &'static str,
    description: &'static str,
}

const LISTINGS: &[SchemaListing] = &[
    SchemaListing {
        name: "plan",
        command: "bento plan --json",
        description: "Cache-aware task plan: which tasks will hit, miss, or skip.",
    },
    SchemaListing {
        name: "report",
        command: "bento ci --json  |  bento build --json  |  ...",
        description: "Execution outcome for each task, with duration and cache state.",
    },
    SchemaListing {
        name: "why",
        command: "bento why <hash> --json",
        description: "Full input manifest(s) for a cache key, including every hashed file.",
    },
    SchemaListing {
        name: "scaffold",
        command: "bento dish add <path> --lang <lang> --json",
        description: "Result of scaffolding a new dish: files written, next steps.",
    },
    SchemaListing {
        name: "manifest",
        command: "(sidecar file written alongside each cache entry)",
        description: "Stand-alone InputManifest schema — subset of `why`.",
    },
    SchemaListing {
        name: "doctor",
        command: "bento doctor --json",
        description: "Structured health-check report with per-check status.",
    },
    SchemaListing {
        name: "error",
        command: "<any command> --json  (when the command fails)",
        description: "Structured error envelope emitted on any failure with --json.",
    },
    SchemaListing {
        name: "diagnostics",
        command: "(field on each failed task in `bento ci --json`)",
        description: "Compiler/linter records normalised to file/line/severity/message.",
    },
    SchemaListing {
        name: "garnish-payload",
        command: "(stdin to Notify-kind integration tasks during `bento deploy`)",
        description: "JSON payload chained to notify/garnish tasks after each deploy.",
    },
    SchemaListing {
        name: "prime",
        command: "bento prime --json",
        description: "Agent orientation snapshot: inventory, cache state, plan preview, next verb.",
    },
];

pub fn run(as_json: bool, target: Option<SchemaTarget>) -> Result<i32> {
    let Some(target) = target else {
        if as_json {
            let json = serde_json::to_string_pretty(LISTINGS)?;
            println!("{json}");
        } else {
            print_listing();
        }
        return Ok(0);
    };

    let schema_json = render(target)?;
    println!("{schema_json}");
    Ok(0)
}

fn render(target: SchemaTarget) -> Result<String> {
    let schema = match target {
        SchemaTarget::Plan => render_one::<Plan>(),
        SchemaTarget::Report => render_one::<ExecutionReport>(),
        SchemaTarget::Why => render_one::<Vec<Explanation>>(),
        SchemaTarget::Scaffold => render_one::<ScaffoldResult>(),
        SchemaTarget::Manifest => render_one::<InputManifest>(),
        SchemaTarget::Doctor => render_one::<DoctorReport>(),
        SchemaTarget::Error => render_one::<BentoError>(),
        SchemaTarget::Diagnostics => render_one::<Diagnostic>(),
        SchemaTarget::GarnishPayload => render_one::<GarnishPayload>(),
        SchemaTarget::Prime => render_one::<PrimeOutput>(),
    };
    Ok(serde_json::to_string_pretty(&schema)?)
}

fn render_one<T: JsonSchema>() -> schemars::Schema {
    schema_for!(T)
}

fn print_listing() {
    println!("bento schema — JSON Schema for agent-consumable outputs\n");
    println!("usage: bento schema <name>\n");
    let name_width = LISTINGS.iter().map(|l| l.name.len()).max().unwrap_or(0);
    for l in LISTINGS {
        println!(
            "  {:name_w$}  {}",
            l.name,
            l.description,
            name_w = name_width
        );
    }
    println!();
    println!("stable: field names and shapes do not change within a major version.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_targets_produce_parsable_schemas() {
        for t in [
            SchemaTarget::Plan,
            SchemaTarget::Report,
            SchemaTarget::Why,
            SchemaTarget::Scaffold,
            SchemaTarget::Manifest,
            SchemaTarget::Doctor,
            SchemaTarget::Error,
            SchemaTarget::Diagnostics,
            SchemaTarget::GarnishPayload,
            SchemaTarget::Prime,
        ] {
            let out = render(t).unwrap_or_else(|e| panic!("{t:?}: {e}"));
            let parsed: serde_json::Value =
                serde_json::from_str(&out).unwrap_or_else(|e| panic!("{t:?}: {e}"));
            // JSON Schema drafts always nest under "$schema" and/or "title"/"properties".
            assert!(
                parsed.get("$schema").is_some()
                    || parsed.get("properties").is_some()
                    || parsed.get("definitions").is_some()
                    || parsed.get("oneOf").is_some()
                    || parsed.get("anyOf").is_some()
                    || parsed.get("items").is_some(),
                "{t:?} produced unexpected top-level shape: {parsed}"
            );
        }
    }

    #[test]
    fn plan_schema_has_top_level_bentos_and_summary() {
        let out = render(SchemaTarget::Plan).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let props = v
            .get("properties")
            .expect("plan must have top-level properties");
        assert!(props.get("bentos").is_some());
        assert!(props.get("summary").is_some());
    }

    #[test]
    fn error_schema_uses_where_not_locator() {
        // The Rust field is renamed; agents see `where`, not `locator`.
        let out = render(SchemaTarget::Error).unwrap();
        assert!(
            out.contains("\"where\""),
            "error schema must expose 'where': {out}"
        );
        assert!(
            !out.contains("\"locator\""),
            "error schema must not leak internal field name 'locator': {out}"
        );
    }

    #[test]
    fn listings_cover_every_schema_target() {
        // If someone adds a SchemaTarget variant they must also add a listing.
        // Listing names must match clap's kebab-case ValueEnum form so
        // `bento schema <name>` resolves to the same string the listing
        // advertises.
        use clap::ValueEnum;
        let schemas = [
            SchemaTarget::Plan,
            SchemaTarget::Report,
            SchemaTarget::Why,
            SchemaTarget::Scaffold,
            SchemaTarget::Manifest,
            SchemaTarget::Doctor,
            SchemaTarget::Error,
            SchemaTarget::Diagnostics,
            SchemaTarget::GarnishPayload,
            SchemaTarget::Prime,
        ];
        for t in schemas {
            let key = t.to_possible_value().unwrap().get_name().to_string();
            assert!(
                LISTINGS.iter().any(|l| l.name == key),
                "no listing entry for {t:?} (expected name={key:?})"
            );
        }
        assert_eq!(LISTINGS.len(), schemas.len());
    }

    #[test]
    fn diagnostics_schema_has_required_fields() {
        let out = render(SchemaTarget::Diagnostics).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let required = v
            .get("required")
            .expect("Diagnostic schema must have required[]")
            .as_array()
            .unwrap();
        let names: Vec<&str> = required.iter().map(|x| x.as_str().unwrap()).collect();
        for must in ["file", "line", "severity", "message", "source"] {
            assert!(names.contains(&must), "{must} missing from required");
        }
    }

    #[test]
    fn report_schema_includes_diagnostics_field_on_executed_task() {
        // ExecutedTask.diagnostics must appear in the schema so agents
        // can switch on its presence even before the executor populates it.
        let out = render(SchemaTarget::Report).unwrap();
        assert!(
            out.contains("\"diagnostics\""),
            "report schema missing diagnostics field: {out}"
        );
    }
}
