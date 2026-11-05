//! The bridge between `dbdelve mcp` and the window.
//!
//! One socket connection is one call: closing it is the cancel, so there is no
//! cancel frame. The app side is std threads and a channel, and the window
//! reaches them only through [`Inbound`] arriving on the pump in
//! `workspace::agent`. [`Status`], [`Reply`] and [`Inbound`] are the
//! vocabulary both halves speak, and they build on every OS; the socket itself
//! is Unix only.
//!
//! `wire`, `tools`, `ipc` and the shim never name gpui, `crate::db` or the
//! keychain: they are the part that can be read, and run, without a window and
//! without a database.
#![cfg_attr(
    not(unix),
    allow(
        dead_code,
        reason = "the bridge only listens on Unix, so on Windows the workspace names these types and nothing builds them"
    )
)]

pub(crate) mod tools;

#[cfg(unix)]
mod ipc;
#[cfg(unix)]
mod server;
#[cfg(unix)]
mod wire;

use serde::{Deserialize, Serialize};

#[cfg(unix)]
pub(crate) use ipc::socket_path;
#[cfg(unix)]
pub(crate) use server::{Listener, listen};
use tools::Tool;

/// What the agent is told: `executed` is the app's, for a call it carried out;
/// `refused_by_gate` is the app's, for a name that resolves to nothing visible
/// or a bridge that is full; `expired` is the app's, for a ping nobody answered;
/// `cancelled` is the app's, logged only, because the caller has already gone;
/// `rejected` is the human's no, from the dialog or the kill switch;
/// `app_not_running` is the shim's, for a connection that failed or closed
/// without an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Executed,
    Rejected,
    RefusedByGate,
    Cancelled,
    Expired,
    AppNotRunning,
}

/// The one answer frame a call gets: a status and one sentence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reply {
    pub(crate) status: Status,
    pub(crate) text: String,
}

/// One accepted connection, from the moment it is classified until it ends.
/// What a presence is held by; a call has its own identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConnId(pub(crate) u64);

/// What a connection thread tells the window.
pub(crate) enum Inbound {
    /// A call, with the one-slot channel its answer goes back on. Dropping the
    /// sender unsent closes the connection without writing, which is how a
    /// cancelled call ends.
    Call {
        tool: Tool,
        reply: std::sync::mpsc::SyncSender<Reply>,
    },
    /// A shim opened its presence connection: an agent is attached, though it
    /// may have asked nothing yet.
    PresenceOpened { conn: ConnId },
    /// A presence connection ended, however it did.
    PresenceClosed { conn: ConnId },
}

/// Why the socket was not bound.
pub(crate) enum Declined {
    /// A live socket is already at the path: another window is serving agents,
    /// and its socket is left as it is.
    ServedElsewhere,
    /// Something went wrong, in words that say what.
    Failed(String),
}

/// Said wherever the bridge is asked for on an OS it does not run on.
#[cfg(not(unix))]
pub(crate) const UNAVAILABLE: &str = "The agent integration is not available on Windows.";

/// The socket is Unix only, so there is nothing to keep alive here.
#[cfg(not(unix))]
pub(crate) enum Listener {}

/// There is no socket path.
#[cfg(not(unix))]
pub(crate) fn socket_path() -> Result<std::path::PathBuf, String> {
    Err(UNAVAILABLE.to_string())
}

/// There is no socket to listen on.
#[cfg(not(unix))]
pub(crate) fn listen(
    _path: &std::path::Path,
    _inbound: tokio::sync::mpsc::UnboundedSender<Inbound>,
) -> Result<Listener, Declined> {
    Err(Declined::Failed(UNAVAILABLE.to_string()))
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    /// A socket path short enough to bind on every platform. Not under
    /// `temp_dir()`: macOS's is already about 49 bytes, and `sun_path` holds
    /// 104 there.
    pub(crate) fn short_socket_path() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        PathBuf::from(format!(
            "/tmp/dbd-{}-{}/s",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }
}
