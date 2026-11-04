//! A plan as plain text an agent can read: what was cut, what the numbers are,
//! what to look at first, then the tree.
//!
//! It prints from the same [`Analysis`] the plan view draws, so the two cannot
//! disagree about which node is hot, and it computes no share, tier,
//! misestimate or basis of its own.
//!
//! The plan text comes from servers and from the tables and columns anyone
//! wrote, and an agent reads it next to the structure this module writes. So
//! structure is only ever written where this module puts it: `truncated:`,
//! `plan:`, `findings:` and `tree:` start a line at column 0 and nothing else
//! does. Every server string is put on one line and written after an
//! indentation and an `n<index>` or `| ` prefix, except the summary's two time
//! values, which sit inside quotes on the `plan:` line with backslash and quote
//! escaped. Everything this module writes is ASCII.
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the explain_query tool (Phase 5) is the first caller"
    )
)]

use super::{
    Plan,
    analysis::{Analysis, Basis, percent_text},
};

/// `plan` as compact text, from the `analysis` built over it. At most
/// `max_nodes` nodes are chosen; see the module's truncation line.
pub(crate) fn compact(plan: &Plan, analysis: &Analysis, _max_nodes: usize) -> String {
    let mut lines = vec!["truncated: none".to_string(), header(plan, analysis)];
    lines.push("findings:".to_string());
    if plan.nodes.is_empty() {
        lines.push("tree: none".to_string());
    } else {
        lines.push("tree:".to_string());
        lines.extend((0..plan.nodes.len()).map(|index| node_line(plan, analysis, index)));
    }
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// The `plan:` line: what the numbers are, in `key=value` pairs.
fn header(plan: &Plan, analysis: &Analysis) -> String {
    let words = analysis.engine.plan_basis();
    let measured = analysis.basis == Basis::Measured;
    let mut line = format!(
        "plan: engine={} analyzed={} basis={} time_unit={} cost_unit={} total={}",
        analysis.engine.as_str(),
        if measured { "yes" } else { "no" },
        match analysis.basis {
            Basis::Measured => "measured",
            Basis::PlannerCost => "planner-cost",
            Basis::Structure => "structure-only",
        },
        value(words.measured),
        value(words.cost),
        match (analysis.total, measured) {
            (Some(total), true) => format!("{}ms", number(total)),
            (Some(total), false) => number(total),
            (None, _) => "none".to_string(),
        },
    );
    for (key, label) in [
        ("execution", "Execution Time"),
        ("planning", "Planning Time"),
    ] {
        if let Some((_, text)) = plan
            .summary
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(label))
        {
            line.push_str(&format!(" {key}=\"{}\"", escaped(&one_line(text))));
        }
    }
    if let Some(outside) = analysis.outside_nodes_ms {
        line.push_str(&format!(" outside_nodes={}ms", number(outside)));
    }
    line.push_str(" conditions=verbatim");
    line
}

/// One node on one line, indented by its depth.
fn node_line(plan: &Plan, analysis: &Analysis, index: usize) -> String {
    let node = &plan.nodes[index];
    let reading = &analysis.nodes[index];
    let mut line = format!("{}n{index}", indent(node.depth));
    if let Some(heading) = &node.heading {
        line.push_str(&format!(" [{}]", one_line(heading)));
    }
    line.push(' ');
    line.push_str(&one_line(&node.label));
    if let Some(value) = reading.value {
        let measured = analysis.basis == Basis::Measured;
        line.push_str(&format!(
            " {}={}{}",
            if measured { "self" } else { "cost" },
            number(value),
            if measured { "ms" } else { "" },
        ));
    }
    if let Some(share) = reading.share {
        line.push_str(&format!(" {}", percent_text(share)));
    }
    let loops = node.actual.map_or(1.0, |actual| actual.loops);
    if let Some(estimated) = node.estimated {
        line.push_str(&format!(" est={}", number(estimated.rows * loops)));
    }
    if let Some(actual) = node.actual {
        line.push_str(&format!(" act={}", number(actual.rows * loops)));
        if loops > 1.0 {
            line.push_str(&format!(" (x {} loops)", number(loops)));
        }
        line.push_str(&format!(" loops={}", number(loops)));
    }
    if let Some(parallel) = reading.parallel {
        line.push_str(&format!(" parallel={}workers", parallel.workers));
    }
    line
}

/// The indentation of a tree line at `depth`: two spaces, so that no line of
/// the tree starts at column 0, and two more for each level.
fn indent(depth: usize) -> String {
    " ".repeat(2 + 2 * depth)
}

/// A value of the `plan:` line the serializer writes itself, quoted when it
/// holds a space or a comma.
fn value(text: Option<&str>) -> String {
    match text {
        Some(text) if text.contains([' ', ',']) => format!("\"{text}\""),
        Some(text) => text.to_string(),
        None => "none".to_string(),
    }
}

/// `text` on one line: every control character, and the two Unicode line
/// separators, becomes a space.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// `text` for inside double quotes: a backslash and a double quote each get a
/// backslash before them, so the text cannot close its quotes early.
fn escaped(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// A figure with up to three decimals and no trailing zeros. A figure that is
/// not a number, which a server's text can produce, prints as `n/a`.
fn number(figure: f64) -> String {
    if !figure.is_finite() {
        return "n/a".to_string();
    }
    let text = format!("{figure:.3}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    match text {
        "" | "-0" => "0".to_string(),
        text => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Engine, rows};

    fn one_column(text: &str) -> Plan {
        super::super::parse(
            &["QUERY PLAN".to_string()],
            &rows(vec![vec![Some(text.to_string())]]),
        )
    }

    #[test]
    fn the_parallel_fixture_serializes_to_its_golden_text() {
        let plan = one_column(include_str!("../testdata/plan-postgres-parallel.txt"));
        let analysis = Analysis::new(&plan, Engine::Postgres);
        let text = compact(&plan, &analysis, usize::MAX);
        let golden = include_str!("../testdata/plan-postgres-parallel.compact.txt");
        assert_eq!(text, golden);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "truncated: none");
        assert!(lines[1].starts_with("plan: engine=postgres analyzed=yes basis=measured"));
        let at = |key: &str| lines.iter().position(|line| *line == key);
        assert!(at("findings:") < at("tree:"));
    }

    /// Not a check: serializes the plan in `DBDELVE_PLAN_FILE` the way a Postgres
    /// plan lands, untruncated or at `DBDELVE_COMPACT_CAP`, and writes it to
    /// `DBDELVE_COMPACT_OUT`. Plan 02-06 measures that text, and the golden files
    /// are written from it.
    #[test]
    #[ignore = "measurement aid: reads DBDELVE_PLAN_FILE and writes DBDELVE_COMPACT_OUT"]
    fn the_compact_form_of_a_plan_file_is_written_for_measurement() {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
        let text = std::fs::read_to_string(var("DBDELVE_PLAN_FILE")).expect("the plan file reads");
        let cap = std::env::var("DBDELVE_COMPACT_CAP")
            .ok()
            .map_or(usize::MAX, |cap| cap.parse().expect("the cap is a number"));
        let plan = one_column(&text);
        let analysis = Analysis::new(&plan, Engine::Postgres);
        let compact = compact(&plan, &analysis, cap);
        std::fs::write(var("DBDELVE_COMPACT_OUT"), &compact).expect("the output writes");
        let kept = compact
            .lines()
            .filter(|line| line.trim_start().starts_with('n') && line.starts_with("  "))
            .filter(|line| {
                line.trim_start()
                    .as_bytes()
                    .get(1)
                    .is_some_and(u8::is_ascii_digit)
            })
            .count();
        println!(
            "nodes={} kept={kept} bytes={}",
            plan.nodes.len(),
            compact.len()
        );
    }
}
