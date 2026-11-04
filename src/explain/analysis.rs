//! What a parsed plan means: one derived model per plan, read by everything that
//! shows or serializes it.
//!
//! Two invariants hold throughout. An [`Analysis`] is derived once, where the
//! plan lands or is restored, and then only read -- the plan view draws from it
//! and the compact form prints from the same field, so the two cannot disagree
//! about which node is hot. And nothing in it is NaN or infinite: a total that
//! cannot be divided by is `None` and every share under it is `None`, because a
//! snapshot compares whole `Explained` values and `NaN != NaN`.
//!
//! Every figure is scoped to the plan it was built from. Nothing here is shared
//! between plans, so no share is ever a fraction of another plan's total.

use super::{Plan, PlanNode};
use crate::db::Engine;

/// A plan's reading: what its numbers are, how heavy each node is, and what is
/// worth looking at first.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Analysis {
    pub(crate) engine: Engine,
    pub(crate) basis: Basis,
    /// The engine and what its numbers are, as the header prints it:
    /// `Postgres · measured (ms)`.
    pub(crate) label: String,
    /// What every share is a share of, on both bases. `None` when there is
    /// nothing to divide by: a plan without numbers, or one that sums to zero.
    pub(crate) total: Option<f64>,
    /// One per plan node, in the plan's order.
    pub(crate) nodes: Vec<NodeAnalysis>,
}

/// Where a plan's numbers come from, decided by what the plan carries and not
/// by what was asked of the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Basis {
    /// At least one node reports how long it took.
    Measured,
    /// No timings, but nodes carry the planner's cost.
    PlannerCost,
    /// A shape and nothing to weigh it by.
    Structure,
}

/// One node's weight in its plan.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NodeAnalysis {
    /// Milliseconds of its own on a measured plan, its own cost on a cost plan.
    pub(crate) value: Option<f64>,
    /// `value` over the plan's total, in `0.0..=1.0`.
    pub(crate) share: Option<f64>,
    pub(crate) tier: Option<Tier>,
}

/// How loudly a figure is drawn. The same four names serve a node's share of
/// its plan and a misestimate's order of magnitude.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    Quiet,
    Warm,
    Hot,
    Danger,
}

impl Tier {
    /// Quiet below 10 %, Warm to below 50 %, Hot through 90 % inclusive, Danger
    /// above. A lower bound belongs to the tier it starts, and 90 % to the one
    /// below it, so that a figure printed to one decimal and a colour drawn from
    /// the share fall on the same side.
    pub(crate) fn of_share(share: f64) -> Self {
        if share < 0.10 {
            Self::Quiet
        } else if share < 0.50 {
            Self::Warm
        } else if share <= 0.90 {
            Self::Hot
        } else {
            Self::Danger
        }
    }
}

impl Analysis {
    /// Read `plan`, as an `engine` printed it.
    pub(crate) fn new(plan: &Plan, engine: Engine) -> Self {
        let basis = if plan.nodes.iter().any(|node| node.actual.is_some()) {
            Basis::Measured
        } else if plan.nodes.iter().any(|node| node.estimated.is_some()) {
            Basis::PlannerCost
        } else {
            Basis::Structure
        };

        // A figure that is not finite is a figure the server's text got wrong,
        // and is left out rather than carried into a division.
        let values: Vec<Option<f64>> = match basis {
            Basis::Measured => plan.nodes.iter().map(|node| node.self_ms).collect(),
            Basis::PlannerCost => own_costs(&plan.nodes),
            Basis::Structure => vec![None; plan.nodes.len()],
        }
        .into_iter()
        .map(|value| value.filter(|value| value.is_finite()))
        .collect();
        let total = match basis {
            // A measured plan's clock time is its top nodes' inclusive times,
            // which `fill_self_ms` makes the sum of every node's own time.
            Basis::Measured => Some(
                plan.nodes
                    .iter()
                    .filter(|node| node.depth == 0)
                    .filter_map(|node| Some(node.actual?.inclusive_ms()))
                    .sum::<f64>(),
            ),
            // The sum of every node's own cost, so a cost plan's shares add to
            // 100 %. Without a `Limit` that is the top nodes' own estimate; with
            // one it is more, because Postgres prices a `Limit` as a fraction of
            // its child and that node's own cost floors at zero.
            Basis::PlannerCost => Some(values.iter().flatten().sum::<f64>()),
            Basis::Structure => None,
        }
        .filter(|total| total.is_finite() && *total > 0.0);

        let nodes = values
            .into_iter()
            .map(|value| {
                // Bounded so malformed server text cannot draw past the track.
                let share = value
                    .zip(total)
                    .map(|(value, total)| (value / total).clamp(0.0, 1.0));
                NodeAnalysis {
                    value,
                    share,
                    tier: share.map(Tier::of_share),
                }
            })
            .collect();

        Self {
            engine,
            basis,
            label: label(engine, basis),
            total,
            nodes,
        }
    }
}

/// Each node's own planner cost: its `total_cost` less its direct children's,
/// never below zero. A child with no estimate costs its parent nothing here.
fn own_costs(nodes: &[PlanNode]) -> Vec<Option<f64>> {
    // The node each one hangs from is the nearest one above it that is
    // shallower.
    let mut path: Vec<usize> = Vec::new();
    let mut children = vec![0.0; nodes.len()];
    for (index, node) in nodes.iter().enumerate() {
        while path
            .last()
            .is_some_and(|last| nodes[*last].depth >= node.depth)
        {
            path.pop();
        }
        if let (Some(parent), Some(estimated)) = (path.last(), node.estimated) {
            children[*parent] += estimated.total_cost;
        }
        path.push(index);
    }
    nodes
        .iter()
        .zip(children)
        .map(|(node, children)| Some((node.estimated?.total_cost - children).max(0.0)))
        .collect()
}

/// A share as the percentage the header and the compact form print, so that the
/// figure and the tier drawn from the same share fall on the same side of every
/// tier boundary.
///
/// Floored to a tenth of a percent, not rounded: a share just under a boundary
/// must not print as the boundary. The one band flooring cannot fix is just
/// above 90 %, which is Danger and would print `90.0%`, a figure that is Hot, so
/// there the next tenth is printed. Works in whole tenths so that a `{:.1}`
/// format never rounds a floored value again.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "drawn by the plan view from plan 02-04")
)]
pub(crate) fn percent_text(share: f64) -> String {
    let mut tenths = (share.clamp(0.0, 1.0) * 1000.0).floor() as u32;
    if tenths == 900 && Tier::of_share(share) == Tier::Danger {
        tenths = 901;
    }
    format!("{}.{}%", tenths / 10, tenths % 10)
}

/// The engine and what its numbers are: `Postgres · measured (ms)`.
fn label(engine: Engine, basis: Basis) -> String {
    let words = engine.plan_basis();
    let name = engine.label();
    match basis {
        Basis::Measured => format!("{name} · {}", words.measured.unwrap_or("measured")),
        Basis::PlannerCost => format!(
            "{name} · {} · no measurements",
            words.cost.unwrap_or("planner cost")
        ),
        Basis::Structure => format!("{name} · structure only"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        db::{Row, rows},
        explain,
    };

    fn one_column(text: &str) -> Plan {
        explain::parse(
            &["QUERY PLAN".to_string()],
            &rows(vec![vec![Some(text.to_string())]]),
        )
    }

    /// One row per line, as ClickHouse answers.
    fn line_per_row(column: &str, text: &str) -> Plan {
        explain::parse(
            &[column.to_string()],
            &rows(
                text.lines()
                    .map(|line| vec![Some(line.to_string())])
                    .collect(),
            ),
        )
    }

    /// What the driver hands back for a MongoDB `explain`.
    fn explain_reply(json: &str) -> Plan {
        let reply: mongodb::bson::Document = serde_json::from_str(json).unwrap();
        let columns: Vec<String> = reply.keys().cloned().collect();
        let row = Row::new(reply.values().map(|value| match value {
            mongodb::bson::Bson::String(text) => Some(text.clone()),
            other => serde_json::to_string(other).ok(),
        }))
        .unwrap();
        explain::parse(&columns, &[row])
    }

    fn tabular(columns: &[&str], cells: &[&str]) -> Plan {
        explain::parse(
            &columns
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>(),
            &rows(vec![
                cells.iter().map(|cell| Some(cell.to_string())).collect(),
            ]),
        )
    }

    const HASH_JOIN: &str = "Hash Join  (cost=0.00..100.00 rows=10 width=8)
  ->  Seq Scan on a  (cost=0.00..30.00 rows=10 width=8)
  ->  Seq Scan on b  (cost=0.00..50.00 rows=10 width=8)";

    /// No number anywhere in the analysis may be NaN or infinite.
    #[track_caller]
    fn assert_finite(analysis: &Analysis) {
        let figures = analysis.total.into_iter().chain(
            analysis
                .nodes
                .iter()
                .flat_map(|node| [node.value, node.share].into_iter().flatten()),
        );
        for figure in figures {
            assert!(figure.is_finite(), "{analysis:?}");
        }
    }

    #[test]
    fn a_measured_plan_gives_each_node_its_share_of_the_root() {
        let plan = one_column(include_str!("../testdata/plan-postgres-parallel.txt"));
        let analysis = Analysis::new(&plan, Engine::Postgres);

        assert_eq!(analysis.basis, Basis::Measured);
        assert_eq!(analysis.label, "Postgres · measured (ms)");
        assert_eq!(analysis.nodes.len(), plan.nodes.len());
        let shares: Vec<f64> = analysis
            .nodes
            .iter()
            .map(|node| node.share.expect("a measured node has a share"))
            .collect();
        assert!(shares.iter().all(|share| (0.0..=1.0).contains(share)));
        let sum: f64 = shares.iter().sum();
        assert!((sum - 1.0).abs() < 0.01, "shares add up to {sum}");
    }

    #[test]
    fn share_tiers_change_exactly_at_ten_fifty_and_ninety_percent() {
        for (share, tier) in [
            (0.0, Tier::Quiet),
            (0.0999, Tier::Quiet),
            (0.10, Tier::Warm),
            (0.4999, Tier::Warm),
            (0.50, Tier::Hot),
            (0.90, Tier::Hot),
            (0.9001, Tier::Danger),
            (1.0, Tier::Danger),
        ] {
            assert_eq!(Tier::of_share(share), tier, "{share}");
        }
        // Equal shares are equally loud.
        assert_eq!(Tier::of_share(0.3), Tier::of_share(0.3));
    }

    #[test]
    fn a_plan_with_no_timings_falls_back_to_each_nodes_own_planner_cost() {
        let values = |analysis: &Analysis| -> Vec<f64> {
            analysis
                .nodes
                .iter()
                .map(|node| node.value.unwrap())
                .collect()
        };
        let shares = |analysis: &Analysis| -> Vec<f64> {
            analysis
                .nodes
                .iter()
                .map(|node| node.share.unwrap())
                .collect()
        };

        let analysis = Analysis::new(&one_column(HASH_JOIN), Engine::Postgres);
        assert_eq!(analysis.basis, Basis::PlannerCost);
        assert_eq!(values(&analysis), [20.0, 30.0, 50.0]);
        assert_eq!(shares(&analysis), [0.2, 0.3, 0.5]);
        assert_eq!(analysis.total, Some(100.0));

        // MySQL's `FORMAT=TREE` prints one cost per node.
        let mysql = Analysis::new(
            &one_column(
                "-> Nested loop inner join  (cost=3.10 rows=6)
    -> Table scan on a  (cost=0.85 rows=6)
    -> Single-row index lookup on b using PRIMARY (id=a.id)  (cost=0.38 rows=1)",
            ),
            Engine::MySql,
        );
        assert_eq!(mysql.basis, Basis::PlannerCost);
        let sum: f64 = shares(&mysql).iter().sum();
        assert!((sum - 1.0).abs() < 1e-9, "{sum}");
        assert!((mysql.nodes[0].value.unwrap() - (3.10 - 0.85 - 0.38)).abs() < 1e-9);

        // Postgres prices a `Limit` as a fraction of its child, so its own cost
        // (0.35 less 100.00) floors at zero and the shares are still of 100.
        let limited = one_column(&format!(
            "Limit  (cost=0.00..0.35 rows=1 width=8)\n  ->  {}",
            HASH_JOIN.replace('\n', "\n    ")
        ));
        let analysis = Analysis::new(&limited, Engine::Postgres);
        assert_eq!(values(&analysis), [0.0, 20.0, 30.0, 50.0]);
        assert_eq!(analysis.total, Some(100.0));
        let sum: f64 = shares(&analysis).iter().sum();
        assert!((sum - 1.0).abs() < 0.01, "{sum}");
        assert!(shares(&analysis).iter().all(|share| *share < 1.0));
        let tiers: Vec<Tier> = analysis
            .nodes
            .iter()
            .map(|node| node.tier.unwrap())
            .collect();
        assert_eq!(tiers, [Tier::Quiet, Tier::Warm, Tier::Warm, Tier::Hot]);
        assert_eq!(analysis.nodes[0].share, Some(0.0));
    }

    #[test]
    fn a_plan_with_neither_timings_nor_costs_draws_no_bars() {
        let sqlite = explain::parse(
            &["id", "parent", "notused", "detail"].map(str::to_string),
            &rows(vec![
                ["2", "0", "0", "SCAN accounts"]
                    .map(|cell| Some(cell.to_string()))
                    .to_vec(),
                ["6", "2", "0", "SEARCH orders USING INDEX orders_account"]
                    .map(|cell| Some(cell.to_string()))
                    .to_vec(),
            ]),
        );
        let mysql = tabular(
            &[
                "id",
                "select_type",
                "table",
                "partitions",
                "type",
                "possible_keys",
                "key",
                "key_len",
                "ref",
                "rows",
                "filtered",
                "Extra",
            ],
            &[
                "1",
                "SIMPLE",
                "accounts",
                "NULL",
                "ALL",
                "NULL",
                "NULL",
                "NULL",
                "NULL",
                "12",
                "100.00",
                "Using where",
            ],
        );
        let mariadb = tabular(
            &[
                "id",
                "select_type",
                "table",
                "type",
                "possible_keys",
                "key",
                "key_len",
                "ref",
                "rows",
                "r_rows",
                "filtered",
                "r_filtered",
                "Extra",
            ],
            &[
                "1",
                "SIMPLE",
                "accounts",
                "ALL",
                "NULL",
                "NULL",
                "NULL",
                "NULL",
                "12",
                "12.00",
                "100.00",
                "100.00",
                "Using where",
            ],
        );
        let clickhouse = line_per_row(
            "explain",
            "Expression ((Project names + Projection))\n  Limit (preliminary LIMIT)\n│  Limit 3\n│  Offset 0\n└──ReadFromSystemNumbers",
        );
        let mongodb = explain_reply(
            r#"{"explainVersion":"1","queryPlanner":{"winningPlan":{"stage":"LIMIT","limitAmount":2,"inputStage":{"stage":"FETCH","inputStage":{"stage":"IXSCAN","indexName":"_id_"}}}},"ok":1}"#,
        );

        for (plan, engine) in [
            (sqlite, Engine::Sqlite),
            (mysql, Engine::MySql),
            (mariadb, Engine::MariaDb),
            (clickhouse, Engine::ClickHouse),
            (mongodb, Engine::MongoDb),
        ] {
            let analysis = Analysis::new(&plan, engine);
            assert_eq!(analysis.basis, Basis::Structure, "{}", engine.label());
            assert_eq!(analysis.total, None, "{}", engine.label());
            assert!(
                analysis
                    .nodes
                    .iter()
                    .all(|node| node.share.is_none() && node.value.is_none()),
                "{}",
                engine.label()
            );
        }
    }

    #[test]
    fn the_header_names_the_engine_and_what_its_numbers_are() {
        let label = |plan: &Plan, engine| Analysis::new(plan, engine).label;

        let measured = one_column(include_str!("../testdata/plan-postgres-parallel.txt"));
        assert_eq!(
            label(&measured, Engine::Postgres),
            "Postgres · measured (ms)"
        );
        assert_eq!(
            label(&one_column(HASH_JOIN), Engine::Postgres),
            "Postgres · planner cost, arbitrary units · no measurements"
        );

        let tree = "-> Limit: 10 row(s)  (cost=1.25 rows=10) (actual time=0.021..0.030 rows=10 loops=1)\n    -> Table scan on accounts  (cost=2.50 rows=12) (actual time=0.019..0.025 rows=12 loops=1)";
        assert_eq!(
            label(&one_column(tree), Engine::MySql),
            "MySQL · measured (ms)"
        );
        let costs = "-> Limit: 10 row(s)  (cost=1.25 rows=10)\n    -> Table scan on accounts  (cost=2.50 rows=12)";
        assert_eq!(
            label(&one_column(costs), Engine::MySql),
            "MySQL · planner cost, single figure · no measurements"
        );
        let tabular_row = tabular(
            &["id", "select_type", "table"],
            &["1", "SIMPLE", "accounts"],
        );
        assert_eq!(label(&tabular_row, Engine::MySql), "MySQL · structure only");
        assert_eq!(
            label(&tabular_row, Engine::MariaDb),
            "MariaDB · structure only"
        );
        assert_eq!(
            label(&tabular_row, Engine::Sqlite),
            "SQLite · structure only"
        );
        assert_eq!(
            label(&tabular_row, Engine::ClickHouse),
            "ClickHouse · structure only"
        );

        let stats = explain_reply(
            r#"{"queryPlanner":{"winningPlan":{"stage":"LIMIT","inputStage":{"stage":"IXSCAN","indexName":"_id_"}}},"executionStats":{"nReturned":3,"executionStages":{"stage":"LIMIT","nReturned":3,"executionTimeMillisEstimate":4,"inputStage":{"stage":"IXSCAN","nReturned":3,"executionTimeMillisEstimate":1,"indexName":"_id_"}}},"ok":1}"#,
        );
        assert_eq!(
            label(&stats, Engine::MongoDb),
            "MongoDB · integer-millisecond estimates"
        );
        let planner =
            explain_reply(r#"{"queryPlanner":{"winningPlan":{"stage":"COLLSCAN"}},"ok":1}"#);
        assert_eq!(label(&planner, Engine::MongoDb), "MongoDB · structure only");

        // A server that said nothing a tree could be read from still gets a
        // header that names the engine.
        assert_eq!(
            label(&Plan::default(), Engine::Postgres),
            "Postgres · structure only"
        );
    }

    #[test]
    fn a_zero_or_empty_plan_has_no_share_and_no_nan() {
        for text in [
            "",
            "Seq Scan on a  (cost=0.00..0.00 rows=1 width=1)",
            "Seq Scan on a  (actual time=0.000..0.000 rows=1 loops=1)",
            "Seq Scan on a  (cost=0.00..1e999 rows=1 width=1)",
            "Seq Scan on a  (cost=NaN..NaN rows=NaN width=1)",
            "Seq Scan on a  (actual time=NaN..NaN rows=1 loops=1)",
            "Seq Scan on a  (cost=0.00..1.00 rows=1e999 width=1) (actual time=0.000..0.000 rows=1e999 loops=1)",
        ] {
            let analysis = Analysis::new(&one_column(text), Engine::Postgres);
            assert_eq!(analysis.total, None, "{text}");
            assert!(
                analysis.nodes.iter().all(|node| node.share.is_none()),
                "{text}"
            );
            assert_finite(&analysis);
        }
        let empty = Analysis::new(&Plan::default(), Engine::Postgres);
        assert!(empty.nodes.is_empty());

        let single = Analysis::new(
            &one_column("Seq Scan on a  (actual time=0.000..2.000 rows=1 loops=1)"),
            Engine::Postgres,
        );
        assert_eq!(single.nodes[0].share, Some(1.0));
        assert_eq!(single.nodes[0].tier, Some(Tier::Danger));
        assert_finite(&single);
    }

    #[test]
    fn a_printed_share_never_crosses_its_tier_boundary() {
        for (share, text) in [
            (0.0996, "9.9%"),
            (0.1, "10.0%"),
            (0.5, "50.0%"),
            (0.9, "90.0%"),
            (0.9001, "90.1%"),
            (0.9009, "90.1%"),
            (f64::from_bits(0.9f64.to_bits() + 1), "90.1%"),
            (0.901, "90.1%"),
            (1.0, "100.0%"),
        ] {
            assert_eq!(percent_text(share), text, "{share}");
        }
        assert_eq!(Tier::of_share(0.9), Tier::Hot);
        assert_eq!(Tier::of_share(0.9001), Tier::Danger);

        for step in 0..=10_000 {
            let share = step as f64 / 10_000.0;
            let printed: f64 = percent_text(share).trim_end_matches('%').parse().unwrap();
            assert_eq!(
                Tier::of_share(printed / 100.0),
                Tier::of_share(share),
                "{share} prints as {}",
                percent_text(share)
            );
        }
    }

    #[test]
    fn no_state_is_shared_between_plans() {
        // The same plan read twice, around a very different one, reads the same.
        let first = Analysis::new(&one_column(HASH_JOIN), Engine::Postgres);
        let big = Analysis::new(
            &one_column("Seq Scan on a  (cost=0.00..1000000.00 rows=1 width=1)"),
            Engine::Postgres,
        );
        let second = Analysis::new(&one_column(HASH_JOIN), Engine::Postgres);
        assert_eq!(first, second);
        assert_eq!(big.nodes[0].share, Some(1.0));
    }
}
