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

use super::Plan;
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

        let values: Vec<Option<f64>> = match basis {
            Basis::Measured => plan.nodes.iter().map(|node| node.self_ms).collect(),
            Basis::PlannerCost | Basis::Structure => vec![None; plan.nodes.len()],
        };
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
            Basis::PlannerCost | Basis::Structure => None,
        }
        .filter(|total| total.is_finite() && *total > 0.0);

        let nodes = values
            .into_iter()
            .map(|value| {
                let value = value.filter(|value| value.is_finite());
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
    use crate::explain;

    fn one_column(text: &str) -> Plan {
        explain::parse(
            &["QUERY PLAN".to_string()],
            &crate::db::rows(vec![vec![Some(text.to_string())]]),
        )
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
}
