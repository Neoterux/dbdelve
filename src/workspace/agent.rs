//! The one place a profile's agent visibility changes.
//!
//! One door, because a sweep of the agent calls still pending against a
//! profile (Phase 4) has to run exactly when the profile is hidden, and a
//! second writer would be one more path to tell about it. Granting is the
//! asymmetric half: it needs the owner's confirmation, so it is refused unless
//! the visibility dialog is open for that very profile.
//!
//! The other half is the bridge: what an agent's call becomes once the socket
//! thread has handed it over. Everything a call does happens here, on the
//! foreground, reached only through [`Workspace::on_agent_message`].

use super::*;
use crate::agent::{Inbound, Reply, Status, tools::Tool};

/// What the agent bridge holds while it is running.
#[derive(Default)]
pub(crate) struct AgentState {
    /// The socket and the task that drains it, or nothing when no bridge runs.
    /// Dropping it is what stops listening.
    pub(crate) bridge: Option<Bridge>,
}

/// A running bridge. Neither field is read: holding them is the point.
pub(crate) struct Bridge {
    _listener: crate::agent::Listener,
    _pump: gpui::Task<()>,
}

/// One entry of `list_connections`: a name and an engine, and nothing that
/// says where the database is or how to get in.
#[derive(serde::Serialize)]
struct Listed<'a> {
    name: &'a str,
    engine: &'static str,
}

/// Why a profile name did not resolve for an agent. Two answers only, and
/// neither echoes the name asked for: the text goes back to the agent as it is.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the agent tools (Phase 4) are the first caller")
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentLookup {
    /// No agent-visible profile has that name. A hidden profile, a missing one,
    /// one this build cannot open and a near-miss all come back as this, so the
    /// answer says nothing about which of them it was.
    NotAvailable,
    /// More than one agent-visible profile has that name. Counts visible
    /// profiles only, so it discloses nothing about hidden ones.
    Ambiguous,
}

impl std::fmt::Display for AgentLookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotAvailable => "no agent-visible connection has that name",
            Self::Ambiguous => "more than one agent-visible connection has that name",
        })
    }
}

/// What the visibility dialog tells the owner about this engine and mode, one
/// entry per sentence. A pure function of the two, so opening the dialog asks
/// no server anything: the hold is what the engine is built to do while the
/// profile is Read-only, not what a live role allows.
pub(crate) fn visibility_facts(engine: Engine, mode: Mode) -> Vec<String> {
    vec![
        format!("This connection's access level is {}.", mode.label()),
        engine.read_only_hold().to_string(),
        "Rows the agent reads leave this machine.".to_string(),
    ]
}

impl Workspace {
    /// Put up the dialog that makes a profile visible to agents. Changes no
    /// flag: only [`Workspace::confirm_agent_visibility`] does. The index is
    /// resolved to the profile's id here, once, because the dialog outlives
    /// whatever the list does meanwhile.
    pub(crate) fn open_agent_visibility(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.get(index) else {
            return;
        };
        self.agent_visibility_prompt = Some(profile.id.clone());
        cx.notify();
    }

    /// The dialog's `Make visible`. Closes the dialog whether or not the
    /// profile it was open for is still there; the grant itself is
    /// [`Workspace::set_agent_visible`]'s, which refuses a profile that is gone.
    pub(crate) fn confirm_agent_visibility(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.agent_visibility_prompt.clone() else {
            return;
        };
        self.set_agent_visible(&id, true, cx);
        self.agent_visibility_prompt = None;
        self.refocus_front();
        cx.notify();
    }

    /// Close the dialog without granting anything. Answers whether a dialog was
    /// drawn, so `escape` is only spent on one that was: a prompt left over a
    /// profile that has since been removed draws nothing.
    pub(crate) fn cancel_agent_visibility(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.agent_visibility_prompt.take() else {
            return false;
        };
        let drawn = self.profiles.iter().any(|profile| profile.id == id);
        if drawn {
            self.refocus_front();
            cx.notify();
        }
        drawn
    }

    /// Make a profile visible to agents or hide it. Keyed by id, never by
    /// index: ids can repeat in a hand-edited file, and the first one wins as
    /// everywhere else in `profiles.rs`.
    ///
    /// Asked for `true` this changes nothing unless
    /// [`Workspace::agent_visibility_prompt`] holds the profile's id, and takes
    /// the prompt when it grants. Asked for `false` it hides at once, because
    /// narrowing needs no confirmation.
    pub(crate) fn set_agent_visible(&mut self, id: &str, visible: bool, cx: &mut Context<Self>) {
        let Some(profile) = self.profiles.iter_mut().find(|profile| profile.id == id) else {
            return;
        };
        if visible {
            if self.agent_visibility_prompt.as_deref() != Some(id) {
                return;
            }
            self.agent_visibility_prompt = None;
        }
        profile.agent_visible = visible;
        self.remember_profiles(cx);
        cx.notify();
    }

    /// Start answering agents on the socket at `path`. Answers `Ok` when a
    /// bridge already runs.
    ///
    /// The socket's threads hand each message to a channel, and a foreground
    /// task drains it into [`Workspace::on_agent_message`]. That task only ever
    /// awaits the channel: nothing here needs a reactor, which GPUI's executor
    /// does not have.
    #[cfg_attr(
        not(all(test, unix)),
        expect(
            dead_code,
            reason = "the e2e socket tests are the only caller until listen_for_agents (task 2)"
        )
    )]
    pub(crate) fn start_bridge_at(
        &mut self,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.agent.bridge.is_some() {
            return Ok(());
        }
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let listener = crate::agent::listen(&path, sender).map_err(|declined| {
            let crate::agent::Declined::Failed(text) = declined;
            text
        })?;
        let pump = cx.spawn(async move |workspace, cx| {
            while let Some(message) = receiver.recv().await {
                if workspace
                    .update(cx, |workspace, cx| workspace.on_agent_message(message, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        self.agent.bridge = Some(Bridge {
            _listener: listener,
            _pump: pump,
        });
        Ok(())
    }

    /// What an agent asked for. Opens no connection and starts no database
    /// work: the answer is read from the profile list as it stands.
    pub(crate) fn on_agent_message(&mut self, message: Inbound, _cx: &mut Context<Self>) {
        match message {
            Inbound::Call {
                tool: Tool::ListConnections,
                reply,
            } => {
                let visible: Vec<Listed> = self
                    .profiles
                    .iter()
                    .filter(|profile| profile.agent_visible)
                    .map(|profile| Listed {
                        name: &profile.name,
                        engine: profile.config.engine().as_str(),
                    })
                    .collect();
                let text = serde_json::to_string(&visible)
                    .expect("names and engine spellings always serialize");
                // The caller may have hung up, and then nobody reads this.
                let _ = reply.send(Reply {
                    status: Status::Executed,
                    text,
                });
            }
        }
    }

    /// The agent-visible profile called `name`.
    ///
    /// Filters to visible profiles before it compares names, so a hidden
    /// profile of the same name cannot change the answer, and names are not
    /// unique: the connection form accepts any non-empty one. The comparison is
    /// on bytes, with no case folding, trimming or normalisation, so what an
    /// agent was told is the only spelling that works. A profile this build
    /// cannot open is never in `self.profiles`, so it is never a candidate.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the agent tools (Phase 4) are the first caller")
    )]
    pub(crate) fn agent_profile(&self, name: &str) -> Result<&Profile, AgentLookup> {
        let mut matches = self
            .profiles
            .iter()
            .filter(|profile| profile.agent_visible)
            .filter(|profile| profile.name == name);
        match (matches.next(), matches.next()) {
            (Some(profile), None) => Ok(profile),
            (Some(_), Some(_)) => Err(AgentLookup::Ambiguous),
            (None, _) => Err(AgentLookup::NotAvailable),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dialog_states_the_engines_hold_and_that_rows_leave_the_machine() {
        for engine in Engine::ALL {
            for mode in Mode::ALL {
                let facts = visibility_facts(engine, mode);
                assert!(facts.iter().any(|fact| fact == engine.read_only_hold()));
                assert!(
                    facts
                        .iter()
                        .any(|fact| fact == "Rows the agent reads leave this machine.")
                );
                assert!(facts.iter().any(|fact| fact.contains(mode.label())));
                for fact in &facts {
                    for word in ["yet", "Phase", "MCP", "try", "should", "upgrade"] {
                        assert!(!fact.contains(word), "{engine:?} {mode:?}: {fact}");
                    }
                }
            }
        }
    }
}
