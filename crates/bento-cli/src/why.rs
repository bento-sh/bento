//! Human-readable formatter for `bento why <target>`.
//!
//! Data layer lives in `bento_core::why` so `bento-mcp` can reuse it.
//! This module keeps only the CLI-side terminal rendering.

use bento_core::why::Explanation;

pub fn print_human(prefix: &str, results: &[Explanation]) {
    if results.is_empty() {
        println!("no cache entry matches '{prefix}'");
        return;
    }

    if results.len() > 1 {
        println!(
            "prefix '{prefix}' matches {} cache entries — showing all",
            results.len()
        );
        println!();
    }

    for (i, result) in results.iter().enumerate() {
        if i > 0 {
            println!();
        }
        print_one(result);
    }
}

fn print_one(result: &Explanation) {
    println!("key: {}", result.key);

    let Some(m) = &result.manifest else {
        println!("  (no manifest — entry was cached before input-tracking was added)");
        return;
    };

    println!("  dish:           {}", m.dish);
    println!("  task:           {}", m.task_name);
    println!("  command:        {}", m.run);
    println!("  bento version:  {}", m.bento_version);
    if let Some(a) = &m.adapter {
        println!("  adapter:        {a}");
    }
    if let Some(t) = &m.toolchain {
        println!("  toolchain:      {t}");
    }
    if !m.env_vars.is_empty() {
        println!("  env vars (values hashed, names only below):");
        for name in &m.env_vars {
            println!("    - {name}");
        }
    }
    println!("  hashed files ({}):", m.files.len());
    let name_width = m
        .files
        .iter()
        .map(|f| f.path.display().to_string().len())
        .max()
        .unwrap_or(4);
    for file in &m.files {
        let short = file.blake3.chars().take(12).collect::<String>();
        println!(
            "    {path:<width$}  {short:<12}  {size:>9} bytes",
            path = file.path.display(),
            width = name_width,
            short = short,
            size = file.size_bytes,
        );
    }
}
