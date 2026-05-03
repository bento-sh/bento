//! Human-readable formatter for [`bento_core::Plan`].
//!
//! TTY-aware colours via [`crate::style`]; structure matches `run_print`
//! so humans reading a `plan → ci` flow see a consistent layout.

use bento_core::{Plan, PlannedDish, PlannedTask, TaskStatus};

use crate::style;

pub fn print_human(plan: &Plan) {
    if plan.bentos.is_empty() {
        println!("no bentos to plan");
        return;
    }

    for (i, bento) in plan.bentos.iter().enumerate() {
        if i > 0 {
            println!();
        }
        println!(
            "{}: {} bento ({} dish{})",
            style::bold("plan"),
            style::cyan(&bento.name),
            bento.dishes.len(),
            if bento.dishes.len() == 1 { "" } else { "es" }
        );

        for dish in &bento.dishes {
            println!();
            print_dish(dish);
        }
    }

    println!();
    let s = &plan.summary;
    println!(
        "summary: {} dish{} · {} task{} · {} miss · {} hit{}",
        s.dishes,
        if s.dishes == 1 { "" } else { "es" },
        s.tasks,
        if s.tasks == 1 { "" } else { "s" },
        s.misses,
        s.hits,
        if s.skipped > 0 {
            format!(" · {} skipped (diff-clean)", s.skipped)
        } else {
            String::new()
        },
    );
}

fn print_dish(dish: &PlannedDish) {
    let language = dish
        .language
        .as_deref()
        .map(|l| format!("({l})"))
        .unwrap_or_else(|| "(no adapter)".to_string());

    println!(
        "  {name}  {lang}",
        name = style::bold(&dish.name),
        lang = style::dim(&language),
    );

    if dish.skipped_by_diff {
        println!("    diff-clean — no tasks to run");
        return;
    }

    if dish.tasks.is_empty() {
        println!("    (no tasks)");
        return;
    }

    let name_width = dish.tasks.iter().map(|t| t.name.len()).max().unwrap_or(4);

    for task in &dish.tasks {
        print_task(task, name_width);
    }
}

fn print_task(task: &PlannedTask, name_width: usize) {
    let status = match task.status {
        TaskStatus::CacheHit => style::dim("cache hit "),
        TaskStatus::CacheMiss => style::yellow("cache miss"),
        TaskStatus::NoAdapter => style::dim("no adapter"),
        TaskStatus::SkippedDiffClean => style::dim("skipped   "),
    };
    let short = if task.key.is_empty() {
        String::new()
    } else {
        task.key.chars().take(12).collect::<String>()
    };
    println!(
        "    {name:<width$}  [{status}]  {short}",
        name = task.name,
        width = name_width,
        status = status,
        short = style::dim(&short),
    );
}
