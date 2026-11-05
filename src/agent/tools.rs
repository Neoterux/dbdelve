//! The agent's tool list. Static: it never varies with what is visible or what
//! state the app is in, because a list that changed under the agent would be a
//! tool the agent had approved turning into another. Only results vary.

use serde::{Deserialize, Serialize};

/// A tool the agent can call, spelled on the wire as its MCP name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Tool {
    ListConnections,
}
