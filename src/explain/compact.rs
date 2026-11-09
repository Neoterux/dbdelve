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

use std::cmp::Ordering;

use super::{
    Plan, PlanNode, WASTEFUL_MIN_REMOVED,
    analysis::{Analysis, Basis, Direction, Finding, percent_text},
    wasteful,
};

/// How many nodes a plan prints, ancestors counted.
pub(crate) const NODE_CAP: usize = 24;

/// How many findings are listed before the rest are counted.
pub(crate) const FINDINGS_CAP: usize = 10;

/// `plan` as compact text, from the `analysis` built over it. When the plan has
/// more than `max_nodes` nodes, the `max_nodes` heaviest are kept with every
/// ancestor of each, so the shape of the tree survives and the heaviest node is
/// never the one cut; the first line says so, and each kept node that lost
/// descendants says how many. A finding on a node that was cut is counted, not
/// listed.
pub(crate) fn compact(plan: &Plan, analysis: &Analysis, max_nodes: usize) -> String {
    let kept = kept(plan, analysis, max_nodes);
    let shown = kept.iter().filter(|kept| **kept).count();
    let mut lines = vec![
        truncation(plan, analysis, max_nodes, shown),
        header(plan, analysis),
    ];
    // A finding is only as good as the node it names, and that node has to be
    // in the tree below for the line to be checked against it. One on a node
    // that was cut is counted and left out, so the list is no longer than the
    // tree is.
    let (shown_findings, left_out): (Vec<Finding>, Vec<Finding>) = analysis
        .findings
        .iter()
        .partition(|finding| kept[node_of(**finding)]);
    if analysis.findings.is_empty() {
        lines.push("findings: none".to_string());
    } else {
        lines.push("findings:".to_string());
        lines.extend(
            shown_findings
                .iter()
                .map(|finding| finding_line(plan, analysis, *finding)),
        );
        if !left_out.is_empty() {
            let count = left_out.len();
            lines.push(format!(
                "  ... {count} finding{} on nodes not shown",
                if count == 1 { "" } else { "s" }
            ));
        }
    }
    if shown == 0 {
        lines.push("tree: none".to_string());
    } else {
        lines.push("tree:".to_string());
        // The kept nodes on the path down to the node being read, each with its
        // child indent and how many of its descendants were dropped. The first
        // entry stands for the plan itself, whose dropped roots are counted too.
        let mut open: Vec<(usize, usize)> = vec![(0, 0)];
        let not_shown = |(child_depth, dropped): (usize, usize)| {
            (dropped > 0).then(|| format!("{}... {dropped} nodes not shown", indent(child_depth)))
        };
        for (index, node) in plan.nodes.iter().enumerate() {
            while open.len() > 1 && open.last().is_some_and(|(child, _)| *child > node.depth) {
                lines.extend(open.pop().and_then(not_shown));
            }
            if kept[index] {
                open.push((node.depth + 1, 0));
                lines.push(node_line(plan, analysis, index));
                lines.extend(detail_lines(plan, index));
            } else if let Some((_, dropped)) = open.last_mut() {
                *dropped += 1;
            }
        }
        while let Some(entry) = open.pop() {
            lines.extend(not_shown(entry));
        }
    }
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// The first line: whether anything was cut and by what rule.
fn truncation(plan: &Plan, analysis: &Analysis, max_nodes: usize, shown: usize) -> String {
    let nodes = plan.nodes.len();
    if max_nodes >= nodes {
        return "truncated: none".to_string();
    }
    let rule = match analysis.basis {
        Basis::Measured => format!("top {max_nodes} by self time, with their ancestors"),
        Basis::PlannerCost => format!("top {max_nodes} by self cost, with their ancestors"),
        Basis::Structure => format!("first {max_nodes} in plan order"),
    };
    format!("truncated: kept {shown} of {nodes} nodes ({rule})")
}

/// Which nodes to print: the `max_nodes` with the highest value, ties in the
/// plan's order and nodes with no value last, and every ancestor of each. A plan
/// with no values ranks nothing, so it keeps the first `max_nodes` as printed.
fn kept(plan: &Plan, analysis: &Analysis, max_nodes: usize) -> Vec<bool> {
    let mut ranked: Vec<usize> = (0..plan.nodes.len()).collect();
    // A stable sort, so equal values stay in document order.
    ranked.sort_by(
        |a, b| match (analysis.nodes[*a].value, analysis.nodes[*b].value) {
            (Some(a), Some(b)) => b.partial_cmp(&a).unwrap_or(Ordering::Equal),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        },
    );
    let mut kept = vec![false; plan.nodes.len()];
    for index in ranked.into_iter().take(max_nodes) {
        // A node already kept as an ancestor has had its own ancestors kept
        // too, and so has the first kept ancestor the walk below meets.
        if std::mem::replace(&mut kept[index], true) {
            continue;
        }
        let mut depth = plan.nodes[index].depth;
        for above in (0..index).rev() {
            if depth == 0 {
                break;
            }
            if plan.nodes[above].depth < depth {
                if std::mem::replace(&mut kept[above], true) {
                    break;
                }
                depth = plan.nodes[above].depth;
            }
        }
    }
    kept
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

/// The node a finding is about.
fn node_of(finding: Finding) -> usize {
    match finding {
        Finding::Hot(index) | Finding::Misestimated(index) => index,
    }
}

/// One finding on one line: the node, why it is worth reading, and its label.
fn finding_line(plan: &Plan, analysis: &Analysis, finding: Finding) -> String {
    match finding {
        Finding::Hot(index) => {
            let reading = &analysis.nodes[index];
            let mut line = format!("  n{index} hot");
            if let Some(share) = reading.share {
                line.push_str(&format!(" {}", percent_text(share)));
            }
            if let Some(value) = reading.value {
                line.push_str(&format!(" {}", weight(analysis, value)));
            }
            line.push_str(&format!(" {}", one_line(&plan.nodes[index].label)));
            line
        }
        Finding::Misestimated(index) => {
            let node = &plan.nodes[index];
            let mut line = format!("  n{index} misestimate");
            if let Some(missed) = analysis.nodes[index].misestimate {
                line.push_str(&format!(
                    " {} x{}",
                    match missed.direction {
                        Direction::Under => "under",
                        Direction::Over => "over",
                    },
                    number(missed.factor.floor()),
                ));
            }
            let loops = loops(node);
            if let Some(estimated) = node.estimated {
                line.push_str(&format!(" est={}", number(estimated.rows * loops)));
            }
            if let Some(actual) = node.actual {
                line.push_str(&format!(" act={}", number(actual.rows * loops)));
            }
            line.push_str(&format!(" {}", one_line(&node.label)));
            line
        }
    }
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
        line.push_str(&format!(" {}", weight(analysis, value)));
    }
    if let Some(share) = reading.share {
        line.push_str(&format!(" {}", percent_text(share)));
    }
    let loops = loops(node);
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

/// A node's own weight as its key says what it is: milliseconds of its own on a
/// measured plan, its own planner cost, in the engine's units, otherwise.
fn weight(analysis: &Analysis, value: f64) -> String {
    match analysis.basis {
        Basis::Measured => format!("self={}ms", number(value)),
        _ => format!("cost={}", number(value)),
    }
}

/// How many times a node ran. Postgres prints both row counts per loop, so a
/// total is a count times this.
fn loops(node: &PlanNode) -> f64 {
    node.actual.map_or(1.0, |actual| actual.loops)
}

/// The lines a node printed under itself, verbatim. A `Rows Removed by` line
/// also gets its total over the loops, and the fact that it threw away more than
/// it kept when [`wasteful`] says so.
fn detail_lines(plan: &Plan, index: usize) -> Vec<String> {
    let node = &plan.nodes[index];
    node.detail
        .iter()
        .map(|detail| {
            let mut line = format!("{}  | {}", indent(node.depth), one_line(detail));
            let removed = detail
                .split_once(": ")
                .filter(|(key, _)| key.starts_with("Rows Removed by "))
                .and_then(|(_, value)| value.trim().parse::<f64>().ok());
            if let (Some(removed), Some(actual)) = (removed, node.actual) {
                line.push_str(&format!(
                    "  [total {} over {} loops]",
                    number(removed * actual.loops),
                    number(actual.loops)
                ));
            }
            if wasteful(detail, node.actual) {
                line.push_str(&format!(
                    "  [fact: removed >= {} and more than kept, per loop]",
                    number(WASTEFUL_MIN_REMOVED)
                ));
            }
            line
        })
        .collect()
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
    use super::super::{Actual, parse};
    use super::*;
    use crate::db::{CancelToken, Connection, ConnectionConfig, Engine, ExplainMode, Fetch, rows};
    use std::path::Path;

    const PARALLEL: &str = include_str!("../testdata/plan-postgres-parallel.txt");
    const CTE: &str = include_str!("../testdata/plan-postgres-cte-subplan.txt");
    const SPILL: &str = include_str!("../testdata/plan-postgres-spill.txt");

    /// The compact form of a Postgres plan, untruncated.
    fn whole(plan: &Plan) -> String {
        compact(plan, &Analysis::new(plan, Engine::Postgres), usize::MAX)
    }

    const LARGE: &str = include_str!("../testdata/plan-postgres-large.txt");

    /// The plan-order index of the node a tree line starts, when it starts one.
    fn node_index(line: &str) -> Option<usize> {
        let digits = line.trim_start().strip_prefix('n')?;
        let end = digits.find(|c: char| !c.is_ascii_digit())?;
        digits[..end].parse().ok()
    }

    /// The lines between a column-0 `start` line and the next column-0 line.
    fn block<'a>(text: &'a str, start: &str) -> Vec<&'a str> {
        text.lines()
            .skip_while(|line| !line.starts_with(start))
            .skip(1)
            .take_while(|line| line.starts_with(' '))
            .collect()
    }

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
        let at = |key: &str| lines.iter().position(|line| line.starts_with(key));
        assert!(at("findings:") < at("tree:"));
    }

    #[test]
    fn a_compact_plan_keeps_loops_rows_removed_analysis_and_cost_unit() {
        let text = whole(&one_column(PARALLEL));
        assert!(text.contains(" loops=3"));
        assert!(text.contains("(x 3 loops)"));
        assert!(text.contains("| Rows Removed by Filter: 33337  [total 100011 over 3 loops]"));
        assert!(text.contains(" analyzed=yes "));
        assert!(text.contains(" cost_unit=\"planner cost, arbitrary units\""));
        assert!(text.contains(" time_unit=\"measured (ms)\""));
    }

    #[test]
    fn findings_come_before_the_tree_and_name_hot_and_misestimated_nodes() {
        for text in [PARALLEL, CTE, SPILL] {
            let plan = one_column(text);
            let analysis = Analysis::new(&plan, Engine::Postgres);
            let out = compact(&plan, &analysis, usize::MAX);
            let at = |key: &str| out.lines().position(|line| line.starts_with(key));
            assert!(at("findings:") < at("tree:"));
            assert!(!analysis.findings.is_empty());
            let findings = block(&out, "findings:");
            assert_eq!(findings.len(), analysis.findings.len());
            for finding in &analysis.findings {
                let prefix = match finding {
                    Finding::Hot(index) => format!("  n{index} hot "),
                    Finding::Misestimated(index) => format!("  n{index} misestimate "),
                };
                assert_eq!(
                    findings
                        .iter()
                        .filter(|line| line.starts_with(&prefix))
                        .count(),
                    1,
                    "{prefix}"
                );
            }
        }

        // The scan under the inner `Limit` read one row of the 169400 it
        // expected, which is the `Limit` and not the planner.
        let plan = one_column(SPILL);
        let scan = plan
            .nodes
            .iter()
            .position(|node| node.estimated.is_some_and(|e| e.rows == 169_400.0))
            .expect("the spill fixture has the scan under its Limit");
        let out = whole(&plan);
        assert!(!out.contains(&format!("n{scan} misestimate")));
        assert!(!out.contains("misestimate"));
    }

    #[test]
    fn the_wasteful_rule_is_a_fact_with_its_threshold_and_never_a_finding() {
        let node = |removed: u32| {
            format!(
                "Seq Scan on t  (cost=0.00..10.00 rows=10 width=4) (actual time=0.010..1.000 rows=5 loops=2)\n  Filter: (a > 1)\n  Rows Removed by Filter: {removed}"
            )
        };
        let wasteful = whole(&one_column(&node(5000)));
        assert!(wasteful.contains(
            "| Rows Removed by Filter: 5000  [total 10000 over 2 loops]  [fact: removed >= 1000 and more than kept, per loop]"
        ));
        let findings = block(&wasteful, "findings:").join("\n");
        assert!(!findings.contains("fact:") && !findings.contains("wasteful"));

        let small = whole(&one_column(&node(3)));
        assert!(small.contains("| Rows Removed by Filter: 3  [total 6 over 2 loops]"));
        assert!(!small.contains("fact:"));

        // 33337 is far fewer than the 299997 rows that node kept.
        assert!(!whole(&one_column(PARALLEL)).contains("fact:"));
        // 948358 against 1632 kept is the other way.
        assert!(whole(&one_column(CTE)).contains("[fact: removed >= 1000 and more than kept"));
    }

    #[test]
    fn server_text_cannot_start_a_line_of_its_own() {
        let plan = Plan {
            nodes: vec![PlanNode {
                depth: 0,
                label: "Seq Scan on t\nfindings: n0 hot 100.0%".to_string(),
                detail: vec!["Filter: x\r\ntree:".to_string()],
                heading: Some("CTE c\n\u{2028}plan: x".to_string()),
                actual: Some(Actual {
                    startup_ms: 0.0,
                    total_ms: 1.0,
                    rows: 1.0,
                    loops: 1.0,
                }),
                self_ms: Some(1.0),
                ..PlanNode::default()
            }],
            summary: vec![
                (
                    "Execution Time".to_string(),
                    "1 ms\nfindings: n0 hot 100.0%".to_string(),
                ),
                (
                    "Planning Time".to_string(),
                    "2 ms\" analyzed=\"no".to_string(),
                ),
            ],
            text: String::new(),
        };
        let out = whole(&plan);
        let starting = |key: &str| out.lines().filter(|line| line.starts_with(key)).count();
        assert_eq!(starting("findings:"), 1);
        assert_eq!(starting("tree:"), 1);
        assert_eq!(starting("plan:"), 1);
        assert_eq!(starting("truncated:"), 1);
        let header = out.lines().nth(1).expect("a header line");
        assert!(header.starts_with("plan: "));
        assert!(header.contains("execution=\"1 ms findings: n0 hot 100.0%\""));
        assert!(header.contains("planning=\"2 ms\\\" analyzed=\\\"no\""));
        assert!(
            out.lines()
                .skip_while(|line| *line != "tree:")
                .skip(1)
                .all(|line| line.starts_with(' ') || line.starts_with('n'))
        );
        // A backslash is escaped too, so it cannot escape the closing quote.
        let mut plan = plan;
        plan.summary[1].1 = "2 ms\\".to_string();
        assert!(whole(&plan).contains("planning=\"2 ms\\\\\""));
    }

    #[test]
    fn conditions_are_kept_verbatim() {
        for text in [CTE, SPILL] {
            let plan = one_column(text);
            let out = whole(&plan);
            let conditions: Vec<&String> = plan
                .nodes
                .iter()
                .flat_map(|node| &node.detail)
                .filter(|line| {
                    ["Filter:", "Index Cond:", "Hash Cond:"]
                        .iter()
                        .any(|k| line.starts_with(k))
                })
                .collect();
            assert!(!conditions.is_empty());
            for condition in conditions {
                assert!(out.contains(&format!("| {condition}")), "{condition}");
            }
            assert!(out.contains(" conditions=verbatim"));
            let lower = out.to_lowercase();
            for word in ["redact", "sanitiz", "masked"] {
                assert!(!lower.contains(word));
            }
        }
    }

    #[test]
    fn a_planner_cost_or_structure_plan_says_so_in_its_header() {
        // The same plan as `EXPLAIN` without `ANALYZE` prints it.
        let mut plain = String::new();
        for line in PARALLEL.lines() {
            match line.find(" (actual ") {
                Some(at) => plain.push_str(&line[..at]),
                None => plain.push_str(line),
            }
            plain.push('\n');
        }
        let out = whole(&one_column(&plain));
        let header = out.lines().nth(1).expect("a header line");
        assert!(header.contains(" analyzed=no basis=planner-cost "));
        let total = header
            .split(' ')
            .find_map(|word| word.strip_prefix("total="))
            .expect("a total");
        assert!(total.parse::<f64>().is_ok(), "{total}");
        assert!(out.contains(" cost="));
        assert!(!out.contains(" self="));

        let sqlite = parse(
            &["id", "parent", "notused", "detail"].map(String::from),
            &rows(vec![
                vec![
                    Some("3".into()),
                    Some("0".into()),
                    Some("0".into()),
                    Some("SCAN accounts".into()),
                ],
                vec![
                    Some("9".into()),
                    Some("3".into()),
                    Some("0".into()),
                    Some("USE TEMP B-TREE FOR ORDER BY".into()),
                ],
            ]),
        );
        let out = compact(&sqlite, &Analysis::new(&sqlite, Engine::Sqlite), usize::MAX);
        let header = out.lines().nth(1).expect("a header line");
        assert!(header.contains(" basis=structure-only time_unit=none cost_unit=none total=none "));
        assert!(header.contains("analyzed=no"));
        assert!(out.contains("n0 SCAN accounts"));
        let tree = block(&out, "tree:");
        assert_eq!(tree.len(), 2);
        assert!(
            tree.iter()
                .all(|line| !line.contains("self=") && !line.contains("cost="))
        );
    }

    #[test]
    fn a_finding_on_a_node_the_truncation_cut_is_counted_and_not_named() {
        let plan = one_column(
            "Nested Loop  (cost=0.00..100.00 rows=10 width=4) (actual time=0.010..50.000 rows=10 loops=1)\n  ->  Seq Scan on heavy  (cost=0.00..90.00 rows=10 width=4) (actual time=0.010..49.000 rows=10 loops=1)\n  ->  Seq Scan on cold  (cost=0.00..1.00 rows=1 width=4) (actual time=0.001..0.002 rows=5000 loops=1)\n",
        );
        let analysis = Analysis::new(&plan, Engine::Postgres);
        let cold = 2;
        assert!(analysis.findings.contains(&Finding::Misestimated(cold)));

        // Uncut, the misestimated leaf is listed as before.
        let all = whole(&plan);
        assert!(
            block(&all, "findings:")
                .iter()
                .any(|line| line.starts_with(&format!("  n{cold} misestimate under ")))
        );

        let out = compact(&plan, &analysis, 1);
        let tree = block(&out, "tree:");
        let shown: Vec<usize> = tree.iter().filter_map(|line| node_index(line)).collect();
        assert!(!shown.contains(&cold));
        let findings = block(&out, "findings:");
        for line in &findings {
            if let Some(index) = node_index(line) {
                assert!(shown.contains(&index), "{line}");
            }
        }
        assert!(!out.contains(&format!("n{cold} misestimate")));
        assert_eq!(findings.last(), Some(&"  ... 1 finding on nodes not shown"));
    }

    #[test]
    fn every_fixture_serializes_to_its_golden_text() {
        for (text, golden) in [
            (
                PARALLEL,
                include_str!("../testdata/plan-postgres-parallel.compact.txt"),
            ),
            (
                CTE,
                include_str!("../testdata/plan-postgres-cte-subplan.compact.txt"),
            ),
            (
                SPILL,
                include_str!("../testdata/plan-postgres-spill.compact.txt"),
            ),
        ] {
            // Asserted first, so that the ASCII check below is about the serializer.
            assert!(text.is_ascii());
            let out = whole(&one_column(text));
            assert_eq!(out, golden);
            assert!(out.is_ascii());
        }
    }

    #[test]
    fn truncation_keeps_the_slowest_nodes_and_their_ancestors() {
        let plan = one_column(LARGE);
        let analysis = Analysis::new(&plan, Engine::Postgres);
        let out = compact(&plan, &analysis, 8);
        let nodes = plan.nodes.len();

        // The expectation is worked out here the long way: the heaviest node and
        // its whole chain, then the nodes of the first findings, then the rest by
        // value, each with the ancestors it lacks, while the total stays within 8.
        let value = |index: usize| analysis.nodes[index].value.unwrap_or(f64::MIN);
        let mut by_value: Vec<usize> = (0..nodes).collect();
        by_value.sort_by(|a, b| value(*b).total_cmp(&value(*a)).then(a.cmp(b)));
        let mut order = vec![by_value[0]];
        for finding in analysis.findings.iter().take(FINDINGS_CAP) {
            order.push(node_of(*finding));
        }
        order.extend(by_value.iter().copied());
        let mut expected = vec![false; nodes];
        for selected in order {
            let mut chain = Vec::new();
            let mut at = Some(selected);
            while let Some(index) = at {
                if expected[index] {
                    break;
                }
                chain.push(index);
                at = (0..index)
                    .rev()
                    .find(|above| plan.nodes[*above].depth < plan.nodes[index].depth);
            }
            let have = expected.iter().filter(|kept| **kept).count();
            if have == 0 || have + chain.len() <= 8 {
                for index in chain {
                    expected[index] = true;
                }
            }
        }
        let tree: Vec<&str> = out.lines().skip_while(|line| *line != "tree:").collect();
        let shown: Vec<usize> = tree.iter().filter_map(|line| node_index(line)).collect();
        let want: Vec<usize> = (0..nodes).filter(|index| expected[*index]).collect();
        assert_eq!(shown, want);
        assert!(shown.len() <= 8 && shown.len() < nodes);
        assert_eq!(
            out.lines().next(),
            Some(
                format!(
                    "truncated: kept {} of {nodes} nodes (heaviest first, then findings, then by self time; at most 8 with ancestors)",
                    shown.len()
                )
                .as_str()
            )
        );

        // Every node is either printed or counted by the line that says so.
        let not_shown: Vec<usize> = tree
            .iter()
            .filter_map(|line| {
                line.trim_start()
                    .strip_prefix("... ")?
                    .strip_suffix(" nodes not shown")?
                    .parse()
                    .ok()
            })
            .collect();
        assert!(!not_shown.is_empty());
        assert_eq!(shown.len() + not_shown.iter().sum::<usize>(), nodes);
        assert_eq!(
            out,
            include_str!("../testdata/plan-postgres-large.truncated.compact.txt")
        );

        // Equal values keep the earlier node, and a dropped root is counted.
        let node = |label: &str| PlanNode {
            label: label.to_string(),
            actual: Some(Actual {
                startup_ms: 0.0,
                total_ms: 2.0,
                rows: 1.0,
                loops: 1.0,
            }),
            self_ms: Some(2.0),
            ..PlanNode::default()
        };
        let tied = Plan {
            nodes: vec![node("first"), node("second"), node("third")],
            ..Plan::default()
        };
        let out = compact(&tied, &Analysis::new(&tied, Engine::Postgres), 2);
        assert_eq!(
            out.lines().next(),
            Some(
                "truncated: kept 2 of 3 nodes (heaviest first, then findings, then by self time; at most 2 with ancestors)"
            )
        );
        assert!(out.contains("n0 first") && out.contains("n1 second") && !out.contains("third"));
        assert!(out.ends_with("  ... 1 nodes not shown\n"));
    }

    #[test]
    fn the_large_fixture_serializes_to_a_pinned_byte_length() {
        let out = whole(&one_column(LARGE));
        // Plan 02-06's `02-TOKEN-BUDGET.md` measures this text. A change to this
        // number means that measured token count is stale.
        assert_eq!(out.len(), 15302);
        assert_eq!(
            out,
            include_str!("../testdata/plan-postgres-large.compact.txt")
        );
    }

    /// The printed node indexes and the count the `... N nodes not shown` lines
    /// add up to, of a compact plan.
    fn printed_and_counted(out: &str) -> (Vec<usize>, usize) {
        let tree = block(out, "tree:");
        let counted = tree
            .iter()
            .filter_map(|line| {
                line.trim_start()
                    .strip_prefix("... ")?
                    .strip_suffix(" nodes not shown")?
                    .parse::<usize>()
                    .ok()
            })
            .sum();
        (
            tree.iter().filter_map(|line| node_index(line)).collect(),
            counted,
        )
    }

    #[test]
    fn the_node_cap_counts_ancestors_and_keeps_the_heaviest_chain() {
        let plan = one_column(LARGE);
        let analysis = Analysis::new(&plan, Engine::Postgres);
        let nodes = plan.nodes.len();
        assert_eq!(nodes, 59);
        let value = |index: usize| analysis.nodes[index].value.unwrap_or(f64::MIN);
        let heaviest = (0..nodes).fold(0, |best, index| {
            if value(index) > value(best) {
                index
            } else {
                best
            }
        });
        let chain = plan.nodes[heaviest].depth + 1;
        for cap in 1..=nodes {
            let out = compact(&plan, &analysis, cap);
            let (shown, counted) = printed_and_counted(&out);
            assert!(
                shown.len() <= cap.max(chain),
                "cap {cap} printed {}",
                shown.len()
            );
            assert!(shown.contains(&heaviest), "cap {cap}");
            for line in block(&out, "findings:") {
                if let Some(index) = node_index(line) {
                    assert!(shown.contains(&index), "cap {cap}: {line}");
                }
            }
            assert_eq!(shown.len() + counted, nodes, "cap {cap}");
        }
        let (shown, _) = printed_and_counted(&compact(&plan, &analysis, 8));
        assert!(shown.len() <= 8, "it printed 38 before: {}", shown.len());
    }

    #[test]
    fn a_chain_deeper_than_the_cap_is_kept_whole() {
        // Five nested loops, the innermost the only one that does any work.
        let mut text = String::new();
        for level in 0..5 {
            let total = 50.0 - 0.1 * f64::from(level);
            let pad = " ".repeat(4 * level as usize);
            let arrow = if level == 0 { "" } else { "  ->  " };
            text.push_str(&format!(
                "{pad}{arrow}Nested Loop  (cost=0.00..100.00 rows=1 width=4) (actual time=0.010..{total:.3} rows=1 loops=1)\n"
            ));
        }
        let plan = one_column(&text);
        assert_eq!(plan.nodes.len(), 5);
        assert_eq!(plan.nodes[4].depth, 4);
        let analysis = Analysis::new(&plan, Engine::Postgres);
        let (shown, counted) = printed_and_counted(&compact(&plan, &analysis, 2));
        assert_eq!(shown, [0, 1, 2, 3, 4]);
        assert_eq!(counted, 0);
    }

    #[test]
    fn findings_are_listed_up_to_their_cap() {
        let mut text = "Append  (cost=0.00..15.00 rows=15000 width=4) (actual time=0.010..15.000 rows=15000 loops=1)\n".to_string();
        for leaf in 0..15 {
            text.push_str(&format!(
                "  ->  Seq Scan on t{leaf}  (cost=0.00..1.00 rows=1 width=4) (actual time=0.010..1.000 rows=1000 loops=1)\n"
            ));
        }
        let plan = one_column(&text);
        let analysis = Analysis::new(&plan, Engine::Postgres);
        assert_eq!(analysis.findings.len(), 15);
        let out = compact(&plan, &analysis, usize::MAX);
        let findings = block(&out, "findings:");
        assert_eq!(findings.len(), FINDINGS_CAP + 1);
        assert_eq!(
            findings.last(),
            Some(&"  ... 5 more findings past the first 10")
        );
    }

    #[test]
    fn the_large_fixture_at_the_node_cap_is_pinned() {
        let plan = one_column(LARGE);
        let out = compact(&plan, &Analysis::new(&plan, Engine::Postgres), NODE_CAP);
        // Plan 05-10 measures the tokens of this text and sets `NODE_CAP` from it;
        // a change to this number means that measurement is stale.
        assert_eq!(out.len(), 0);
        assert_eq!(
            out,
            include_str!("../testdata/plan-postgres-large.capped.compact.txt")
        );
    }

    #[test]
    fn an_empty_plan_or_a_zero_cap_serializes_without_a_panic() {
        let empty = Plan::default();
        let out = compact(&empty, &Analysis::new(&empty, Engine::Postgres), 0);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "truncated: none");
        assert!(lines[1].starts_with("plan: engine=postgres analyzed=no basis=structure-only "));
        assert_eq!(lines[2..], ["findings: none", "tree: none"]);
        assert!(out.is_ascii());

        let plan = one_column(LARGE);
        let nodes = plan.nodes.len();
        let analysis = Analysis::new(&plan, Engine::Postgres);
        let none = compact(&plan, &analysis, 0);
        assert!(none.starts_with(&format!("truncated: kept 0 of {nodes} nodes")));
        assert!(none.ends_with("tree: none\n"));

        let every = compact(&plan, &analysis, nodes);
        assert!(every.starts_with("truncated: none\n"));
        assert_eq!(every, compact(&plan, &analysis, usize::MAX));
        let one_short = compact(&plan, &analysis, nodes - 1);
        assert!(one_short.starts_with("truncated: kept "));
    }

    /// Not a check: serializes the plan in `DBDELVE_PLAN_FILE` the way a Postgres
    /// plan lands, untruncated or at `DBDELVE_COMPACT_CAP`, and writes it to
    /// `DBDELVE_COMPACT_OUT`. Plan 02-06 measures that text, and the golden files
    /// are written from it.
    #[test]
    #[ignore = "measurement aid: reads DBDELVE_PLAN_FILE and writes DBDELVE_COMPACT_OUT"]
    fn the_compact_form_of_a_plan_file_is_written_for_measurement() {
        // CI runs every ignored test, this one included; with no plan file named
        // there is nothing to measure.
        let Ok(plan_file) = std::env::var("DBDELVE_PLAN_FILE") else {
            return;
        };
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
        let text = std::fs::read_to_string(plan_file).expect("the plan file reads");
        let cap = std::env::var("DBDELVE_COMPACT_CAP")
            .ok()
            .map_or(usize::MAX, |cap| cap.parse().expect("the cap is a number"));
        let plan = one_column(&text);
        let analysis = Analysis::new(&plan, Engine::Postgres);
        let compact = compact(&plan, &analysis, cap);
        std::fs::write(var("DBDELVE_COMPACT_OUT"), &compact).expect("the output writes");
        let kept = compact
            .lines()
            .skip_while(|line| *line != "tree:")
            .filter(|line| node_index(line).is_some())
            .count();
        println!(
            "nodes={} kept={kept} bytes={}",
            plan.nodes.len(),
            compact.len()
        );
    }

    /// One plan a compose server answers in a form the Postgres parser does not
    /// read: which server, what is explained and how, and the fixture that holds
    /// the answer.
    struct Capture {
        variable: &'static str,
        fixture: &'static str,
        mode: ExplainMode,
        sql: &'static str,
    }

    /// What `Row`s are written as in a fixture: one tab-separated line per row,
    /// `\N` for NULL, and a tab or newline inside a value as `\t` or `\n`.
    // ponytail: a literal backslash is not escaped, so a value reading `\N` or
    // `\t` is ambiguous. Plans carry none; escape it when a fixture does.
    fn fixture_text(columns: &[String], rows: &[crate::db::Row]) -> String {
        let escape = |cell: &str| cell.replace('\t', "\\t").replace('\n', "\\n");
        let mut text = columns
            .iter()
            .map(|name| escape(name))
            .collect::<Vec<_>>()
            .join("\t");
        text.push('\n');
        for row in rows {
            let cells: Vec<String> = row
                .iter()
                .map(|cell| cell.map_or_else(|| "\\N".to_string(), escape))
                .collect();
            text.push_str(&cells.join("\t"));
            text.push('\n');
        }
        text
    }

    /// Each `Capture` is run on its compose server through `Engine::explain`,
    /// the call the plan tool makes. With `DBDELVE_PLAN_FIXTURES_OUT` set the
    /// answers are written there as the fixtures; without it the columns each
    /// server answers with must be the committed fixture's header, so a server
    /// that changes the shape of its plan fails here.
    fn check_captures(captures: &[Capture]) {
        let out = std::env::var("DBDELVE_PLAN_FIXTURES_OUT").ok();
        for capture in captures {
            let url = std::env::var(capture.variable)
                .unwrap_or_else(|_| panic!("{} is required", capture.variable));
            let config = ConnectionConfig::from_url(&url).expect("the URL should parse");
            let engine = config.engine();
            let connection = Connection::open(config).expect("connection should open");
            let statement = engine
                .explain(capture.mode, capture.sql)
                .expect("the engine plans a statement");
            let result = connection
                .query(&statement, &CancelToken::default(), Fetch::default())
                .unwrap_or_else(|error| panic!("{statement}: {}", error.message));
            assert!(
                !result.rows.is_empty(),
                "{} answered no rows",
                capture.fixture
            );
            let columns: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
            let text = fixture_text(&columns, &result.rows);
            match &out {
                Some(dir) => std::fs::write(Path::new(dir).join(capture.fixture), &text)
                    .expect("the fixture writes"),
                None => {
                    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("src/testdata")
                        .join(capture.fixture);
                    let committed = std::fs::read_to_string(&path).expect("the fixture reads");
                    let header = committed.lines().next().unwrap_or_default();
                    assert_eq!(
                        header,
                        text.lines().next().unwrap_or_default(),
                        "{} no longer has the columns the server answers with",
                        capture.fixture
                    );
                }
            }
        }
    }

    /// The join the MySQL and MariaDB fixtures plan, over `dev/mysql/init`.
    const MYSQL_JOIN: &str = "SELECT accounts.name, orders.number, orders.total \
        FROM accounts JOIN orders ON orders.account_id = accounts.id \
        WHERE orders.total > 100";

    /// Captures the plans of the engines whose answer is a table or flat text
    /// from the compose servers (`dbdelve_MYSQL_URL` and its siblings), the
    /// fixtures 05-03's fallback and 05-09's tool are tested against.
    #[test]
    #[ignore = "requires the compose databases"]
    fn live_the_plan_fixtures_match_the_compose_servers() {
        check_captures(&[
            Capture {
                variable: "dbdelve_MYSQL_URL",
                fixture: "plan-mysql-tabular.txt",
                mode: ExplainMode::Plan,
                sql: MYSQL_JOIN,
            },
            Capture {
                variable: "dbdelve_MARIADB_URL",
                fixture: "plan-mariadb-tabular.txt",
                mode: ExplainMode::Plan,
                sql: MYSQL_JOIN,
            },
            Capture {
                variable: "dbdelve_MARIADB_URL",
                fixture: "plan-mariadb-analyze.txt",
                mode: ExplainMode::Analyze,
                sql: MYSQL_JOIN,
            },
            Capture {
                variable: "dbdelve_CLICKHOUSE_URL",
                fixture: "plan-clickhouse.txt",
                mode: ExplainMode::Plan,
                sql: "SELECT id, name, plan, balance FROM accounts WHERE active",
            },
        ]);
    }
}
