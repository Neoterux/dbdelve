//! The one place a profile's agent visibility changes.
//!
//! One door, because the sweep of the agent calls still pending against a
//! profile has to run exactly when the profile is hidden, and a second writer
//! would be one more path to tell about it. Granting is the asymmetric half:
//! it needs the owner's confirmation, so it is refused unless the visibility
//! dialog is open for that very profile.
//!
//! The other half is the bridge: what an agent's call becomes once the socket
//! thread has handed it over. Everything a call does happens here, on the
//! foreground, reached only through [`Workspace::on_agent_message`].

use std::{collections::VecDeque, sync::mpsc::SyncSender};

use super::agent_read::{self, NEEDS_APPROVAL};
use super::*;
use crate::{
    agent::{CallId, EXPIRY, Inbound, Reply, Status, tools::Tool},
    sql::{AgentVerdict, agent_gate},
};

/// What the agent bridge holds while it is running.
#[derive(Default)]
pub(crate) struct AgentState {
    /// The socket and the task that drains it, or nothing when no bridge runs.
    /// Dropping it is what stops listening.
    pub(crate) bridge: Option<Bridge>,
    /// The presence connections open now: agents attached, whether or not they
    /// have asked anything.
    pub(crate) presence: Vec<crate::agent::ConnId>,
    /// The calls waiting on an answer, found by their id and never by their
    /// place in this list.
    pub(crate) calls: Vec<PendingCall>,
    /// What this window's own access action decided and no save has written
    /// yet: `true` from turning access on, `false` from the kill switch. Set
    /// by those two actions only, and taken by the next save, so it is the one
    /// way this window changes `agent_access`.
    pub(crate) access_choice: Option<bool>,
    /// The profiles, by id, whose `agent_visible` this window changed and no
    /// save has written yet. Every other profile's flag is the file's: see
    /// [`Workspace::adopt_saved_visibility`].
    pub(crate) visibility_choices: HashSet<String>,
    /// The calls that have ended, newest first, at most [`LOG_CAP`].
    pub(crate) log: VecDeque<LogEntry>,
    /// The agent's own connections, by profile id: never the user's.
    pub(crate) lanes: HashMap<String, agent_read::Lane>,
}

// ponytail: the log is this session's own and keeps the last twenty calls,
// because it is read by clicking the mark and a longer list is not read.
// Phase 5 (READ-10, APPR-12) turns it into the audit, which is kept.
pub(crate) const LOG_CAP: usize = 20;

/// One call that ended. Names, a code and a time, and nothing the call carried:
/// no row, no statement, no host, user or path. Held in memory and gone with
/// the window.
#[derive(Clone, Debug)]
pub(crate) struct LogEntry {
    /// Unix milliseconds when the call ended, as the palette's history keeps
    /// its own.
    pub(crate) at: u64,
    pub(crate) id: CallId,
    pub(crate) tool: Tool,
    /// The profile's own name, as it read when the call resolved to it.
    pub(crate) resolved: Option<String>,
    /// What the agent called a connection that resolved to nothing: cut to 64
    /// characters with control characters removed, and shown as the agent's
    /// words, never as DBDelve's.
    pub(crate) asked: Option<String>,
    pub(crate) status: Status,
    /// The answer could not be delivered, because its caller had already gone.
    pub(crate) requester_gone: bool,
}

/// How many characters of an agent's own text the log keeps.
const ASKED_CAP: usize = 64;

pub(crate) fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

impl AgentState {
    /// Write one ended call at the front of the log. Called once per call, from
    /// the places that answer it.
    pub(super) fn record(
        &mut self,
        id: CallId,
        tool: Tool,
        resolved: Option<String>,
        asked: Option<&str>,
        status: Status,
    ) {
        self.log.push_front(LogEntry {
            at: unix_millis(),
            id,
            tool,
            resolved,
            asked: asked.map(|text| {
                text.chars()
                    .filter(|c| !c.is_control())
                    .take(ASKED_CAP)
                    .collect()
            }),
            status,
            requester_gone: false,
        });
        self.log.truncate(LOG_CAP);
    }
}

/// A call that has been accepted and not yet finished. Dropping it without
/// replying closes the caller's connection with no answer.
pub(crate) struct PendingCall {
    pub(crate) id: CallId,
    /// The profile the call's connection name resolved to when it arrived. The
    /// dialog and the sweeps read the profile by this id, so a rename or a
    /// second profile of the same name cannot change which one a call is about.
    pub(crate) profile_id: String,
    pub(crate) tool: Tool,
    /// The profile's own name when the call resolved, kept so the log can name
    /// it after the profile is gone.
    profile_name: String,
    reply: std::sync::mpsc::SyncSender<Reply>,
    /// Ends the call as `expired` when it fires. Dropped with the call, which
    /// is what cancels it.
    _expiry: gpui::Task<()>,
}

/// What a pending call is told when the kill switch ends it.
const TURNED_OFF: &str = "Agent access was turned off in DBDelve.";

/// What the kill switch says in a window that is not serving while another
/// window of the variant is. Its sentence is what stays true: that window's
/// agents are not disconnected.
const STILL_SERVED_ELSEWHERE: &str = "Agent access was turned off. Another DBDelve window is still serving agents; they stay connected until that window closes or its own kill switch is used.";

/// What every call waiting on a profile is told when it is hidden or removed.
/// One sentence for both, so the agent cannot tell which happened.
const NO_LONGER_AVAILABLE: &str = "The connection is no longer available to agents.";

/// What an unanswered ping is told. The wait it names is [`EXPIRY`] itself.
fn expired_text() -> String {
    format!(
        "Nobody answered the ping in DBDelve within {} minutes.",
        EXPIRY.as_secs() / 60
    )
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

/// What the ping dialog says: the connection by its own name, that this is a
/// test, and that approving touches nothing. The name is the profile's, as its
/// owner spelt it, never text the agent sent.
pub(crate) fn ping_prompt(name: &str) -> String {
    format!(
        "An agent asks you to answer a test for {name}. Approving answers the agent; it runs \
         nothing on the database."
    )
}

/// What the visibility dialog tells the owner about this engine and mode, one
/// entry per sentence. A pure function of the two, so opening the dialog asks
/// no server anything: the hold is what the engine is built to do while the
/// profile is Read-only, not what a live role allows. The sentence about a call
/// to a function DBDelve does not list is what `agent_gate` answers such a call
/// in that mode, and is left out where the gate refuses it.
pub(crate) fn visibility_facts(engine: Engine, mode: Mode) -> Vec<String> {
    let mut facts = vec![
        format!("This connection's access level is {}.", mode.label()),
        engine.read_only_hold().to_string(),
        "Rows the agent reads leave this machine.".to_string(),
    ];
    // MongoDB and Redis have no function call to ask about, and Generic at
    // Read-only refuses one, so neither sentence is true there.
    if engine.syntax() == db::Syntax::Sql {
        if mode >= Mode::ReadWrite {
            facts.push(
                "Calls to functions DBDelve does not list wait for your approval.".to_string(),
            );
        } else if !engine.reads_only_listed_calls() {
            facts
                .push("Calls to functions DBDelve does not list run without approval.".to_string());
        }
    }
    facts
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
        self.agent.visibility_choices.insert(id.to_string());
        if !visible {
            self.reject_calls_for(id, cx);
        }
        self.remember_profiles(cx);
        cx.notify();
    }

    /// Answer every call waiting on this profile `rejected`, oldest first, each
    /// once. Calls waiting on any other profile are left as they are.
    pub(crate) fn reject_calls_for(&mut self, profile_id: &str, cx: &mut Context<Self>) {
        let waiting: Vec<CallId> = self
            .agent
            .calls
            .iter()
            .filter(|call| call.profile_id == profile_id)
            .map(|call| call.id)
            .collect();
        for id in waiting {
            self.finish_call(id, Status::Rejected, NO_LONGER_AVAILABLE.to_string(), cx);
        }
    }

    /// The call the ping dialog is about: the oldest one whose profile is still
    /// in the list. A call whose profile has gone is swept with it, so the
    /// filter only guards the moment between the two.
    pub(crate) fn front_ping(&self) -> Option<&PendingCall> {
        self.agent.calls.iter().find(|call| {
            self.profiles
                .iter()
                .any(|profile| profile.id == call.profile_id)
        })
    }

    /// The dialog's Approve. Answers the agent and nothing else: no connection
    /// is opened and nothing runs.
    pub(crate) fn approve_ping(&mut self, id: CallId, cx: &mut Context<Self>) {
        self.finish_call(
            id,
            Status::Executed,
            "The ping was approved in DBDelve.".to_string(),
            cx,
        );
    }

    /// The dialog's Deny, for a call named by id.
    pub(crate) fn deny_ping(&mut self, id: CallId, cx: &mut Context<Self>) {
        self.finish_call(
            id,
            Status::Rejected,
            "The ping was denied in DBDelve.".to_string(),
            cx,
        );
    }

    /// Deny whichever ping the dialog in front is about. Answers whether there
    /// was one, so `escape` is only spent on a dialog that was drawn.
    pub(crate) fn deny_front_ping(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.front_ping().map(|call| call.id) else {
            return false;
        };
        self.deny_ping(id, cx);
        true
    }

    /// Start answering agents on the socket at `path`. Answers `Ok` when a
    /// bridge already runs.
    ///
    /// The socket's threads hand each message to a channel, and a foreground
    /// task drains it into [`Workspace::on_agent_message`]. That task only ever
    /// awaits the channel: nothing here needs a reactor, which GPUI's executor
    /// does not have.
    pub(crate) fn start_bridge_at(
        &mut self,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.agent.bridge.is_some() {
            return Ok(());
        }
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let listener = crate::agent::listen(&path, sender).map_err(|declined| match declined {
            crate::agent::Declined::ServedElsewhere => {
                "Another DBDelve window is already serving agents.".to_string()
            }
            crate::agent::Declined::Failed(text) => text,
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

    /// Start answering agents on this build's own socket.
    pub(crate) fn listen_for_agents(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        self.start_bridge_at(crate::agent::socket_path()?, cx)
    }

    /// What `agent_access` holds in the file right now, `None` when it holds
    /// nothing or the file cannot be read, which both read as off.
    fn saved_agent_access(&self) -> Option<bool> {
        store::peek_profiles(&self.plugins)
            .and_then(|list| list.settings)
            .and_then(|settings| settings.agent_access)
    }

    /// The `agent_access` the next save writes: this window's own choice when
    /// one of its two access actions made one, else whatever the file holds at
    /// this moment. Settings are written whole by whichever window saves, so a
    /// value remembered from launch would put back `true` over another
    /// window's kill switch, or `false` over its turn-on.
    ///
    /// ponytail: the read and the write that follows are not one step across
    /// processes, so two windows saving within the same instant can still
    /// lose one of the two. The upgrade is a lock around the settings write.
    pub(crate) fn agent_access_to_save(&mut self) -> Option<bool> {
        let own = self.agent.access_choice.take();
        own.or_else(|| self.saved_agent_access())
    }

    /// Take on each profile's `agent_visible` as the file holds it now, except
    /// where this window made a choice no save has written yet. Profiles are
    /// written whole by whichever window saves, so a flag remembered from
    /// launch would put a hidden profile back in front of agents, with no
    /// dialog, over another window's hide, or drop another window's grant.
    /// And the window serving agents must answer with the hide made in
    /// another one. A profile hidden this way has its waiting calls rejected,
    /// as a hide here does.
    ///
    /// ponytail: the read and the write that follows are not one step across
    /// processes, as for `agent_access`; the upgrade is the same lock.
    pub(crate) fn adopt_saved_visibility(&mut self, cx: &mut Context<Self>) {
        let Some(saved) = store::peek_profiles(&self.plugins) else {
            return;
        };
        let mut hidden = Vec::new();
        for profile in &mut self.profiles {
            if self.agent.visibility_choices.contains(&profile.id) {
                continue;
            }
            // The first of a repeated id, as everywhere else.
            let Some(stored) = saved.profiles.iter().find(|stored| stored.id == profile.id) else {
                continue;
            };
            if profile.agent_visible && !stored.agent_visible {
                hidden.push(profile.id.clone());
            }
            profile.agent_visible = stored.agent_visible;
        }
        for id in hidden {
            self.reject_calls_for(&id, cx);
        }
    }

    /// Open the socket and, only once that has worked, save that access is on.
    /// A refusal is shown as it is and changes nothing saved, so a window that
    /// found another one serving can be asked again later.
    pub(crate) fn turn_on_agent_access(&mut self, cx: &mut Context<Self>) {
        if self.agent.bridge.is_some() {
            return;
        }
        match self.listen_for_agents(cx) {
            Ok(()) => {
                self.agent.access_choice = Some(true);
                self.remember_profiles(cx);
                cx.notify();
            }
            Err(text) => self.note(text, cx),
        }
    }

    /// Stop listening and forget who was attached. Dropping the bridge stops
    /// the listener, removes the socket file and shuts every presence
    /// connection; it leaves the saved choice alone, so quitting keeps it.
    pub(crate) fn drop_bridge(&mut self) {
        self.agent.bridge = None;
        self.agent.presence.clear();
    }

    /// The kill switch: nothing an agent started stays open, and nothing
    /// listens until the user turns access on again. In order:
    ///
    /// 1. Every pending call is answered `rejected`. This comes first and runs
    ///    with or without a bridge, because dropping the bridge never closes a
    ///    call's connection: each call's writer thread sends its frame and then
    ///    closes its own socket, so a waiting client reads a whole answer and
    ///    not a bare end of file.
    /// 2. The bridge is dropped, if there is one.
    /// 3. `agent_access = false` is saved when this window was serving or when
    ///    the file says `true`. It is the user's own action, so no later launch
    ///    of the variant listens, even if this window never did. With neither,
    ///    nothing is written.
    /// 4. In a window that was not serving, while another window of the
    ///    variant is, the notice says that window's agents stay connected. This
    ///    window cannot reach them, because Phase 4 has no frame that tells
    ///    another window to stop; what it can do is save the choice and say
    ///    that. The notice repeats on every press, since it stays true.
    pub(crate) fn disconnect_agents(&mut self, cx: &mut Context<Self>) {
        let pending: Vec<CallId> = self.agent.calls.iter().map(|call| call.id).collect();
        for id in pending {
            self.finish_call(id, Status::Rejected, TURNED_OFF.to_string(), cx);
        }
        let was_serving = self.agent.bridge.is_some();
        self.drop_bridge();
        if was_serving || self.saved_agent_access() == Some(true) {
            self.agent.access_choice = Some(false);
            self.remember_profiles(cx);
        }
        if !was_serving && crate::agent::socket_is_live() {
            self.note(STILL_SERVED_ELSEWHERE.to_string(), cx);
        }
        cx.notify();
    }

    /// What the titlebar's agent mark shows: nothing while no bridge runs, the
    /// dim mark while one listens alone, and the lit mark, with the count of
    /// calls waiting, while an agent is attached or a call waits.
    pub(crate) fn agent_mark_state(&self) -> Option<ui::AgentMark> {
        self.agent.bridge.as_ref()?;
        let attached = !self.agent.presence.is_empty() || !self.agent.calls.is_empty();
        Some(if attached {
            ui::AgentMark::Connected {
                waiting: self.agent.calls.len(),
            }
        } else {
            ui::AgentMark::Listening
        })
    }

    /// What an agent asked for. Opens no connection and starts no database
    /// work: the answer is read from the profile list as it stands.
    pub(crate) fn on_agent_message(&mut self, message: Inbound, cx: &mut Context<Self>) {
        match message {
            Inbound::PresenceOpened { conn } => {
                self.agent.presence.push(conn);
                cx.notify();
            }
            Inbound::PresenceClosed { conn } => {
                self.agent.presence.retain(|open| *open != conn);
                cx.notify();
            }
            Inbound::Call {
                tool: Tool::ListConnections,
                id,
                reply,
                ..
            } => {
                self.adopt_saved_visibility(cx);
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
                self.agent
                    .record(id, Tool::ListConnections, None, None, Status::Executed);
            }
            Inbound::Call {
                tool: Tool::ExecuteSql,
                id,
                connection,
                args,
                reply,
            } => {
                self.adopt_saved_visibility(cx);
                let found = match connection.as_deref() {
                    Some(name) => self.agent_profile(name).map(|profile| {
                        (
                            profile.id.clone(),
                            profile.name.clone(),
                            profile.config.engine(),
                            profile.mode,
                        )
                    }),
                    None => Err(AgentLookup::NotAvailable),
                };
                let (profile_id, profile_name, engine, mode) = match found {
                    Ok(found) => found,
                    Err(lookup) => {
                        self.agent_answered(
                            id,
                            None,
                            connection.as_deref(),
                            &reply,
                            Status::RefusedByGate,
                            lookup.to_string(),
                        );
                        return;
                    }
                };
                // The schema requires a statement, so only an injected call
                // has none; it is the empty statement.
                let sql = args.sql.unwrap_or_default();
                let refused = match agent_gate(engine, &sql, mode) {
                    AgentVerdict::Run => {
                        self.run_agent_sql(id, profile_id, sql, reply, cx);
                        return;
                    }
                    // The verdict's contents are what an approval will show,
                    // which agents do not get.
                    AgentVerdict::Approve(..) => NEEDS_APPROVAL.to_string(),
                    AgentVerdict::Refuse(reason) => reason.to_string(),
                };
                self.agent_answered(
                    id,
                    Some(profile_name),
                    None,
                    &reply,
                    Status::RefusedByGate,
                    refused,
                );
            }
            Inbound::Call {
                tool: Tool::Ping,
                id,
                connection,
                reply,
                ..
            } => {
                self.adopt_saved_visibility(cx);
                let found = match connection.as_deref() {
                    Some(name) => self
                        .agent_profile(name)
                        .map(|profile| (profile.id.clone(), profile.name.clone())),
                    None => Err(AgentLookup::NotAvailable),
                };
                let (profile_id, profile_name) = match found {
                    Ok(found) => found,
                    Err(lookup) => {
                        // The caller may have hung up, and then nobody reads this.
                        let _ = reply.send(Reply {
                            status: Status::RefusedByGate,
                            text: lookup.to_string(),
                        });
                        self.agent.record(
                            id,
                            Tool::Ping,
                            None,
                            connection.as_deref(),
                            Status::RefusedByGate,
                        );
                        return;
                    }
                };
                let expiry = cx.spawn(async move |workspace, cx| {
                    cx.background_executor().timer(EXPIRY).await;
                    _ = workspace.update(cx, |workspace, cx| {
                        workspace.finish_call(id, Status::Expired, expired_text(), cx)
                    });
                });
                self.agent.calls.push(PendingCall {
                    id,
                    profile_id,
                    tool: Tool::Ping,
                    profile_name,
                    reply,
                    _expiry: expiry,
                });
                cx.notify();
            }
            Inbound::Hangup { id } => {
                self.finish_call(id, Status::Cancelled, String::new(), cx);
            }
            Inbound::Undelivered { id } => {
                if let Some(entry) = self.agent.log.iter_mut().find(|entry| entry.id == id) {
                    entry.requester_gone = true;
                    cx.notify();
                }
            }
        }
    }

    /// Answer an `execute_sql` call at once and log it: the end of a call that
    /// never waits for anyone. `resolved` is the profile's own name, `asked` what the agent
    /// called a connection that resolved to nothing.
    pub(super) fn agent_answered(
        &mut self,
        id: CallId,
        resolved: Option<String>,
        asked: Option<&str>,
        reply: &SyncSender<Reply>,
        status: Status,
        text: String,
    ) {
        // The caller may have hung up, and then nobody reads this.
        let _ = reply.send(Reply { status, text });
        self.agent
            .record(id, Tool::ExecuteSql, resolved, asked, status);
    }

    /// The one door every call ends through: answered, expired or hung up.
    /// It removes the call by id before it replies, so two things ending the
    /// same call at once produce one outcome and at most one reply. Answers
    /// whether there was such a call.
    ///
    /// A `Cancelled` call is dropped without a reply, because its caller has
    /// gone and nobody would read one.
    pub(crate) fn finish_call(
        &mut self,
        id: CallId,
        status: Status,
        text: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(index) = self.agent.calls.iter().position(|call| call.id == id) else {
            return false;
        };
        let was_in_front = self.front_ping().is_some_and(|front| front.id == id);
        let call = self.agent.calls.remove(index);
        if status != Status::Cancelled {
            // The caller may have hung up, and then nobody reads this.
            let _ = call.reply.send(Reply { status, text });
        }
        // Past the early return above, so a hangup that arrives after the call
        // ended adds nothing: one entry per call, written here.
        self.agent
            .record(id, call.tool, Some(call.profile_name), None, status);
        if was_in_front {
            // The dialog took the keyboard when it went up and goes away with
            // the call, by whichever route the call ended.
            self.refocus_front();
        }
        cx.notify();
        true
    }

    /// The agent-visible profile called `name`.
    ///
    /// Filters to visible profiles before it compares names, so a hidden
    /// profile of the same name cannot change the answer, and names are not
    /// unique: the connection form accepts any non-empty one. The comparison is
    /// on bytes, with no case folding, trimming or normalisation, so what an
    /// agent was told is the only spelling that works. A profile this build
    /// cannot open is never in `self.profiles`, so it is never a candidate.
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

    #[test]
    fn the_dialog_says_whether_a_call_to_an_unlisted_function_waits() {
        use crate::sql::{AgentVerdict, agent_gate};

        const RUNS: &str = "Calls to functions DBDelve does not list run without approval.";
        const WAITS: &str = "Calls to functions DBDelve does not list wait for your approval.";
        for engine in Engine::ALL {
            for mode in Mode::ALL {
                // The sentence comes from what the gate answers, so the dialog
                // cannot state an answer the gate contradicts.
                let (stated, other) = match agent_gate(engine, "SELECT my_udf(a) FROM t", mode) {
                    AgentVerdict::Run => (Some(RUNS), WAITS),
                    AgentVerdict::Approve(..) => (Some(WAITS), RUNS),
                    AgentVerdict::Refuse(_) => (None, ""),
                };
                let facts = visibility_facts(engine, mode);
                let count = |sentence: &str| facts.iter().filter(|fact| *fact == sentence).count();
                match stated {
                    Some(stated) => {
                        assert_eq!(count(stated), 1, "{engine:?} {mode:?}: {facts:?}");
                        assert_eq!(count(other), 0, "{engine:?} {mode:?}: {facts:?}");
                    }
                    None => {
                        assert_eq!(
                            count(RUNS) + count(WAITS),
                            0,
                            "{engine:?} {mode:?}: {facts:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_ping_prompt_names_the_connection_and_runs_nothing() {
        let prompt = ping_prompt("Reports");
        assert!(prompt.contains("Reports"), "{prompt}");
        assert!(prompt.contains("runs nothing"), "{prompt}");
        for name in ["Orders", "a b", "x'y"] {
            let prompt = ping_prompt(name);
            assert!(prompt.contains(name), "{prompt}");
            for word in ["SQL", "try", "should", "Phase", "MCP"] {
                assert!(!prompt.contains(word), "{name}: {prompt}");
            }
        }
    }
}
