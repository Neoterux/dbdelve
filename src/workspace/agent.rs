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

use std::{
    collections::VecDeque,
    sync::mpsc::SyncSender,
    time::{Duration, Instant},
};

use super::agent_read::{self, AgentWork, NEEDS_APPROVAL, Resolved, explain_statement};
use super::agent_write;
use super::*;
use crate::{
    agent::{CallId, EXPIRY, Inbound, Reply, Status, result::Total, tools::Tool},
    db::ConnectionConfig,
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
    /// The calls waiting on an answer from a person, found by their id and
    /// never by their place in this list. Reads are not here: they wait for
    /// nobody, and live in their [`agent_read::Lane`].
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
    /// The calls that have ended, newest first, at most [`AUDIT_CAP`].
    pub(crate) log: VecDeque<LogEntry>,
    /// [`store::profiles_stamp`] as [`Workspace::adopt_saved_visibility`] last
    /// found it, taken before that read.
    pub(crate) visibility_stamp: Option<(std::time::SystemTime, u64)>,
    /// The agent's own connections, by profile id: never the user's.
    pub(crate) lanes: HashMap<String, agent_read::Lane>,
    /// What opens an agent's connection; `None` is the real open. Tests put
    /// one here to stand in for an open that never returns.
    pub(crate) opener: Option<agent_read::Opener>,
    /// The approved writes running now, found by their id. Each holds no
    /// connection and no way to cancel it: see [`agent_write`].
    pub(crate) writes: Vec<agent_write::Writing>,
    /// The call whose Approve is live: the approval in front, once
    /// [`ARM_DELAY`] has passed since it was shown. At most one.
    pub(crate) armed: Option<CallId>,
    /// The approval [`Workspace::reveal_approval`] last started the arm for.
    pub(crate) revealed: Option<CallId>,
    /// The one-shot timer that sets `armed`. Replaced, and so cancelled, each
    /// time the dialog in front is revealed.
    _arming: Option<gpui::Task<()>>,
    /// Unix milliseconds when this window's bridge started, or 0 when none
    /// has: with a call's id, what pairs the lines of the audit file. A call's
    /// id is the counter of its connection, which starts again each run.
    pub(crate) session: u64,
    /// The file approved statements are recorded in.
    #[cfg(unix)]
    audit: store::AuditLog,
}

// ponytail: the wait between an approval being shown and its Approve working,
// a decision and not a setting: long enough that a click or keystroke already
// in flight when the dialog appeared cannot land on an Approve that did not
// exist when it began. The upgrade is a setting.
pub(crate) const ARM_DELAY: Duration = Duration::from_millis(1500);

/// What an approval that was approved is told when the line that says it is
/// about to run could not be written. Nothing was opened.
pub(super) const NOT_RECORDED: &str =
    "DBDelve could not record the statement before running it, so it was not run.";

/// What an approval that was approved is told when the statement, the
/// connection or the profile's access level is not what it was when the person
/// was shown it. Nothing was opened.
pub(super) const CHANGED_SINCE_SHOWN: &str = "The statement, the connection or its access level changed after it was shown, so it was not run.";

// ponytail: the audit is this session's own and keeps the last five hundred
// calls, in memory only, because it is read in a dialog and a longer list is
// not read. Phase 6's persisted record of approvals (APPR-12) is the upgrade:
// it is what outlives the window.
pub(crate) const AUDIT_CAP: usize = 500;

/// How many characters of an agent's own statement an audit entry keeps.
pub(super) const AUDIT_SQL_CAP: usize = 4096;

/// What an entry's SQL ends with once it was cut at [`AUDIT_SQL_CAP`].
pub(super) const SQL_CLIPPED: &str = " [clipped]";

/// What was decided about a statement before it ran, as the audit says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AuditVerdict {
    /// The gate let it through and it ran on the agent's own connection.
    Run,
    /// The gate held it for a person: the lowest mode that lets it run, and the
    /// expiry text of a statement that schedules a removal.
    Approve { mode: Mode, expiry: Option<String> },
    /// The gate refused it, for this reason.
    Refuse(String),
}

impl AuditVerdict {
    /// The line the Agent activity dialog shows for it.
    pub(crate) fn line(&self) -> String {
        match self {
            Self::Run => "Ran".to_string(),
            Self::Approve {
                mode,
                expiry: Some(expiry),
            } => format!("Needs approval at {}. {expiry}", mode.label()),
            Self::Approve { mode, expiry: None } => {
                format!("Needs approval at {}", mode.label())
            }
            Self::Refuse(reason) => format!("Refused: {reason}"),
        }
    }

    /// What the audit says of a gate verdict that held or refused a statement;
    /// `None` for `Run`, which the read itself gives its verdict.
    fn held_by(verdict: AgentVerdict) -> Option<Self> {
        match verdict {
            AgentVerdict::Run => None,
            AgentVerdict::Approve(verdict, expiry) => Some(Self::Approve {
                mode: verdict.mode,
                expiry: expiry.map(|expiry| expiry.to_string()),
            }),
            AgentVerdict::Refuse(reason) => Some(Self::Refuse(reason.to_string())),
        }
    }
}

/// What a call knew about itself at its end, beyond who made it and how it
/// ended. A call that has none of these, such as a ping, takes the default.
#[derive(Default)]
pub(crate) struct CallFacts {
    pub(crate) verdict: Option<AuditVerdict>,
    /// The statement exactly as the agent sent it.
    pub(crate) sql: Option<String>,
    /// How many rows went back to the agent, and how many there were.
    pub(crate) shown: Option<usize>,
    pub(crate) total: Option<Total>,
    /// Whole milliseconds from the call's arrival to its end.
    pub(crate) elapsed_ms: u64,
    pub(crate) error: Option<String>,
    /// The caller hung up before the call ended.
    pub(crate) requester_gone: bool,
    /// What the person decided about a statement that waited for them. A call
    /// with one is written to the audit file when it ends.
    pub(crate) decision: Option<Decision>,
    /// The engine of the profile the call was about, for the audit file.
    pub(crate) engine: Option<Engine>,
}

/// What a person did with a statement that waited for them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Decision {
    Approved,
    Rejected,
    Expired,
}

/// One line of `agent-audit.jsonl`, the record of statements that waited for a
/// person. It names the statement, who decided and how the call ended, and
/// never a row value: what the database held does not belong in a file kept
/// for longer than a session. `v` leads so a later shape can be told apart.
/// JSON escapes every newline, so text an agent sent cannot start a line.
#[derive(serde::Serialize)]
pub(super) struct AuditLine<'a> {
    v: u8,
    session: u64,
    id: u64,
    at: u64,
    /// `started` before a write is opened, `outcome` once the call ended.
    phase: &'static str,
    tool: Tool,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    engine: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verdict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    decision: Option<Decision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<Status>,
    requester_gone: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    shown: Option<usize>,
    elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
    /// The statement whole, never cut at [`AUDIT_SQL_CAP`]: clipping the record
    /// of what a person approved would defeat it.
    #[serde(skip_serializing_if = "Option::is_none")]
    sql: Option<&'a str>,
}

/// One call that ended: its names, its code, its time and what the agent sent.
/// Never a row: the audit shows that a read happened and how much came back,
/// not what was in it. Held in memory and gone with the window.
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
    /// `None` for the tools that carry no statement.
    pub(crate) verdict: Option<AuditVerdict>,
    /// The agent's statement, cut at [`AUDIT_SQL_CAP`] characters.
    pub(crate) sql: Option<String>,
    pub(crate) shown: Option<usize>,
    pub(crate) total: Option<Total>,
    pub(crate) elapsed_ms: u64,
    /// The error of a call that failed, which the agent was not given, cut at
    /// [`AUDIT_SQL_CAP`] characters.
    pub(crate) error: Option<String>,
}

impl LogEntry {
    /// Whether the agent was given fewer rows than there were, or than the
    /// statement could have returned.
    pub(crate) fn capped(&self) -> bool {
        match (self.shown, self.total) {
            (_, Some(Total::AtLeast(_))) => true,
            (Some(shown), Some(Total::Exact(total))) => total > shown,
            _ => false,
        }
    }
}

/// How many characters of an agent's own text the log keeps.
const ASKED_CAP: usize = 64;

pub(crate) fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Whole milliseconds since `arrived`, truncated.
pub(crate) fn elapsed_ms(arrived: Instant) -> u64 {
    arrived.elapsed().as_millis() as u64
}

impl AgentState {
    /// Write one ended call at the front of the log. Called once per call, from
    /// the places that answer it. Entries are ordered by insertion and never by
    /// their time, so two calls that end in the same millisecond keep the order
    /// they ended in.
    pub(super) fn record(
        &mut self,
        id: CallId,
        tool: Tool,
        resolved: Option<String>,
        asked: Option<&str>,
        status: Status,
        facts: CallFacts,
    ) {
        if let Some(decision) = facts.decision {
            // The one place an outcome line is written, so the file and the
            // log say the same of every call that waited.
            let line = AuditLine {
                v: 1,
                session: self.session,
                id: id.0,
                at: unix_millis(),
                phase: "outcome",
                tool,
                profile: resolved.as_deref(),
                engine: facts.engine.map(Engine::as_str),
                verdict: facts.verdict.as_ref().map(AuditVerdict::line),
                decision: Some(decision),
                status: Some(status),
                requester_gone: facts.requester_gone,
                shown: facts.shown,
                elapsed_ms: facts.elapsed_ms,
                error: facts.error.as_deref(),
                sql: facts.sql.as_deref(),
            };
            if let Err(error) = self.persist(&line) {
                // The call has ended and nothing can un-run it, so the only
                // thing left to do is say that its record is missing. The
                // error names the file and never the statement.
                eprintln!("{error}");
            }
        }
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
            requester_gone: facts.requester_gone,
            verdict: facts.verdict,
            sql: facts.sql.map(clipped),
            shown: facts.shown,
            total: facts.total,
            elapsed_ms: facts.elapsed_ms,
            error: facts.error.map(clipped),
        });
        self.log.truncate(AUDIT_CAP);
    }
}

impl AgentState {
    /// Write one line to the audit file. The one door to it, so the writes of
    /// the two phases cannot disagree about the file.
    #[cfg(unix)]
    pub(super) fn persist(&mut self, line: &AuditLine) -> Result<(), String> {
        let json = serde_json::to_string(line)
            .map_err(|error| format!("Could not encode the audit line: {error}"))?;
        self.audit.append(&json)
    }

    /// No agent call reaches a window here: the bridge listens only on Unix.
    #[cfg(not(unix))]
    pub(super) fn persist(&mut self, _line: &AuditLine) -> Result<(), String> {
        Ok(())
    }
}

/// `text` cut at [`AUDIT_SQL_CAP`] characters, on a character boundary: a
/// statement, or an error that may echo one. The dialog draws every entry on
/// every frame.
fn clipped(mut text: String) -> String {
    if let Some((cut, _)) = text.char_indices().nth(AUDIT_SQL_CAP) {
        text.truncate(cut);
        text.push_str(SQL_CLIPPED);
    }
    text
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
    /// When the call reached [`Workspace::on_agent_message`].
    arrived: Instant,
    /// What the person is asked to approve, for a statement the gate held. A
    /// ping asks for nothing and has none.
    pub(crate) approval: Option<Approval>,
    /// Ends the call as `expired` when it fires. Dropped with the call, which
    /// is what cancels it.
    _expiry: gpui::Task<()>,
}

/// The statement a held call asks to run, with what the gate and the profile
/// said of it when it arrived. `sql` is exactly what the agent sent: what the
/// dialog draws and what runs, never a copy that was cleaned for the screen.
pub(crate) struct Approval {
    pub(crate) sql: String,
    /// The gate's answer at arrival, which the re-gate at Approve must
    /// reproduce.
    pub(crate) verdict: AgentVerdict,
    /// The profile's config at arrival, compared with the current one at
    /// Approve.
    pub(crate) config: ConnectionConfig,
}

/// What a pending call is told when the kill switch ends it.
const TURNED_OFF: &str = "Agent access was turned off in DBDelve.";

/// What the kill switch says in a window that is not serving while another
/// window of the variant is. Its sentence is what stays true: that window's
/// agents are not disconnected.
const STILL_SERVED_ELSEWHERE: &str = "Agent access was turned off. Another DBDelve window is still serving agents; they stay connected until that window closes or its own kill switch is used.";

/// What every call waiting on a profile is told when it is hidden or removed.
/// One sentence for both, so the agent cannot tell which happened.
pub(super) const NO_LONGER_AVAILABLE: &str = "The connection is no longer available to agents.";

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
/// in that mode, and is left out where the gate refuses it. `read_can_block_commits`
/// is [`db::ConnectionConfig::agent_read_can_block_commits`], read by the caller
/// so this stays pure.
pub(crate) fn visibility_facts(
    engine: Engine,
    mode: Mode,
    read_can_block_commits: bool,
) -> Vec<String> {
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
    if read_can_block_commits {
        facts.push(
            "An agent's read in progress can make your commit on this file wait, and then fail \
             with \"database is locked\"."
                .to_string(),
        );
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
        self.agent_visibility_blocks_commits = profile.config.agent_read_can_block_commits();
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

    /// Put up the `Agent activity` dialog. Changes nothing but what is drawn.
    pub(crate) fn show_agent_activity(&mut self, cx: &mut Context<Self>) {
        self.agent_audit_open = true;
        cx.notify();
    }

    /// Close the dialog and hand the keyboard back. Answers whether it was
    /// open, so `escape` is only spent on a dialog that was drawn.
    pub(crate) fn close_agent_audit(&mut self, cx: &mut Context<Self>) -> bool {
        if !std::mem::take(&mut self.agent_audit_open) {
            return false;
        }
        self.refocus_front();
        cx.notify();
        true
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
    /// once, and end its reads, running and waiting, and drop its agent
    /// connection. Calls on any other profile are left as they are.
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
        self.end_lane(profile_id, Status::Rejected, NO_LONGER_AVAILABLE, cx);
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

    /// The call the dialog in front is about, when it asks for a statement to
    /// be run and not for a ping to be answered.
    pub(crate) fn front_approval_id(&self) -> Option<CallId> {
        self.front_ping()
            .filter(|call| call.approval.is_some())
            .map(|call| call.id)
    }

    /// The timer that ends a call nobody answered as `expired`. Dropping it
    /// with the call is what cancels it.
    fn expiry_of(&self, id: CallId, cx: &mut Context<Self>) -> gpui::Task<()> {
        cx.spawn(async move |workspace, cx| {
            cx.background_executor().timer(EXPIRY).await;
            _ = workspace.update(cx, |workspace, cx| {
                workspace.finish_call(id, Status::Expired, expired_text(), cx)
            });
        })
    }

    /// The approval in front has just been drawn, as the dialog in front: its
    /// Approve is dead until [`ARM_DELAY`] has passed from now. Called one frame
    /// after the dialog is raised or the front call changes, so it counts from
    /// what the person can see and never from the call's arrival.
    pub(crate) fn reveal_approval(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.front_approval_id() else {
            return;
        };
        self.agent.revealed = Some(id);
        self.agent.armed = None;
        // Replacing the old timer cancels it.
        self.agent._arming = Some(cx.spawn(async move |workspace, cx| {
            cx.background_executor().timer(ARM_DELAY).await;
            _ = workspace.update(cx, |workspace, cx| {
                // Only while the approval is what the person is looking at: a
                // dialog over it that they are reading is not the approval
                // being shown, and it is revealed again when that closes.
                if workspace.front_approval_id() == Some(id)
                    && workspace.dialog_in_front.get() == Some("agent-ping")
                {
                    workspace.agent.armed = Some(id);
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    /// The dialog's Approve for a held statement. Does nothing until the call is
    /// armed. Then the call is taken out of the waiting list, and the statement
    /// is judged again as the gate sees it now: it must come out as it did when
    /// it arrived, on the config it arrived with. Only then does the write
    /// start, on a connection of its own, with the very `String` the dialog drew.
    pub(crate) fn approve_call(&mut self, id: CallId, cx: &mut Context<Self>) {
        if self.agent.armed != Some(id) {
            return;
        }
        // A hide saved by another window since this call arrived: the answer
        // is this call's own rejection, found below as no call at all.
        self.adopt_saved_visibility_for_read(cx);
        let Some(index) = self
            .agent
            .calls
            .iter()
            .position(|call| call.id == id && call.approval.is_some())
        else {
            return;
        };
        let was_in_front = self.front_ping().is_some_and(|front| front.id == id);
        // Dropping the call's expiry with the rest of it.
        let call = self.agent.calls.remove(index);
        let PendingCall {
            profile_id,
            profile_name,
            reply,
            arrived,
            approval: Some(approval),
            ..
        } = call
        else {
            return;
        };
        let engine = approval.config.engine();
        let profile = self
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id && profile.agent_visible);
        let held = AuditVerdict::held_by(approval.verdict.clone());
        let (status, text, error) = match profile {
            None => (Status::Rejected, NO_LONGER_AVAILABLE, None),
            Some(profile)
                if agent_gate(profile.config.engine(), &approval.sql, profile.mode)
                    != approval.verdict
                    || profile.config != approval.config =>
            {
                (Status::RefusedByGate, CHANGED_SINCE_SHOWN, None)
            }
            Some(_) => {
                // Before anything is opened: a write that cannot be recorded
                // is not run, so the file never lacks a statement the server
                // may have received.
                let started = AuditLine {
                    v: 1,
                    session: self.agent.session,
                    id: id.0,
                    at: unix_millis(),
                    phase: "started",
                    tool: Tool::ExecuteSql,
                    profile: Some(&profile_name),
                    engine: Some(engine.as_str()),
                    verdict: held.as_ref().map(AuditVerdict::line),
                    decision: Some(Decision::Approved),
                    status: None,
                    requester_gone: false,
                    shown: None,
                    elapsed_ms: elapsed_ms(arrived),
                    error: None,
                    sql: Some(&approval.sql),
                };
                match self.agent.persist(&started) {
                    // No decision on the failure: its outcome line would meet
                    // the same file.
                    Err(error) => (Status::Failed, NOT_RECORDED, Some(error)),
                    Ok(()) => {
                        self.start_write(
                            id,
                            profile_id,
                            profile_name,
                            approval.config,
                            approval.sql,
                            held,
                            reply,
                            arrived,
                            cx,
                        );
                        self.finish_approval(was_in_front, cx);
                        return;
                    }
                }
            }
        };
        // The person did click Approve; what the answer says is that it did not
        // run. A failure to record has no decision (see above).
        let decision = (status != Status::Failed).then_some(Decision::Approved);
        self.agent_answered(
            id,
            Tool::ExecuteSql,
            Some(profile_name),
            None,
            &reply,
            Reply {
                status,
                text: text.to_string(),
            },
            CallFacts {
                verdict: held,
                sql: Some(approval.sql),
                elapsed_ms: elapsed_ms(arrived),
                error,
                decision,
                engine: Some(engine),
                ..CallFacts::default()
            },
        );
        self.finish_approval(was_in_front, cx);
    }

    /// What every end of an approval does after the call left the list: the
    /// arm is spent, and the keyboard goes back if the dialog that took it went.
    fn finish_approval(&mut self, was_in_front: bool, cx: &mut Context<Self>) {
        self.agent.armed = None;
        if was_in_front {
            self.refocus_front();
        }
        cx.notify();
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
        self.agent.session = unix_millis();
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
        // Taken before the read, so a save landing during it reads as a change.
        self.agent.visibility_stamp = store::profiles_stamp();
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

    /// [`Workspace::adopt_saved_visibility`] for a call that runs a statement
    /// or reads a catalog, which can come many at once: the file is read and
    /// parsed again only when its modified time or length moved, so a hide
    /// saved by another window is never missed and an unchanged file costs one
    /// `stat` on the window's thread.
    ///
    /// ponytail: a save that keeps the length within the file system's time
    /// resolution reads as unchanged; that resolution is nanoseconds on APFS
    /// and ext4 and 100 ns on NTFS, and a visibility flip changes the length.
    fn adopt_saved_visibility_for_read(&mut self, cx: &mut Context<Self>) {
        let stamp = store::profiles_stamp();
        if stamp.is_none() || stamp != self.agent.visibility_stamp {
            self.adopt_saved_visibility(cx);
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
    /// 1. Every pending call and every read, running or waiting, is answered
    ///    `rejected` and every agent connection is dropped. This comes first and runs
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
        let lanes: Vec<String> = self.agent.lanes.keys().cloned().collect();
        for profile_id in lanes {
            self.end_lane(&profile_id, Status::Rejected, TURNED_OFF, cx);
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
        // A read in flight is an agent attached, though nobody is asked.
        let attached = !self.agent.presence.is_empty()
            || !self.agent.calls.is_empty()
            || self.agent.lanes.values().any(|lane| lane.running.is_some());
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
        let arrived = Instant::now();
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
                // Names and engine spellings always serialize; an empty list
                // is what the impossible case would say.
                let text = serde_json::to_string(&visible).unwrap_or_else(|_| "[]".to_string());
                // The caller may have hung up, and then nobody reads this.
                let _ = reply.send(Reply {
                    status: Status::Executed,
                    text,
                });
                self.agent.record(
                    id,
                    Tool::ListConnections,
                    None,
                    None,
                    Status::Executed,
                    CallFacts {
                        elapsed_ms: elapsed_ms(arrived),
                        ..CallFacts::default()
                    },
                );
            }
            Inbound::Call {
                tool: Tool::ExecuteSql,
                id,
                connection,
                args,
                reply,
            } => {
                self.adopt_saved_visibility_for_read(cx);
                let found = match connection.as_deref() {
                    Some(name) => self
                        .agent_profile(name)
                        .map(|profile| (Resolved::of(profile), profile.mode)),
                    None => Err(AgentLookup::NotAvailable),
                };
                let (resolved, mode) = match found {
                    Ok(found) => found,
                    Err(lookup) => {
                        self.agent_answered(
                            id,
                            Tool::ExecuteSql,
                            None,
                            connection.as_deref(),
                            &reply,
                            Reply {
                                status: Status::RefusedByGate,
                                text: lookup.to_string(),
                            },
                            CallFacts {
                                elapsed_ms: elapsed_ms(arrived),
                                ..CallFacts::default()
                            },
                        );
                        return;
                    }
                };
                // The schema requires a statement, so only an injected call
                // has none; it is the empty statement.
                let sql = args.sql.unwrap_or_default();
                let engine = resolved.config.engine();
                let verdict = agent_gate(engine, &sql, mode);
                // The verdict's contents are what an approval will show,
                // which agents do not get.
                let (refused, audited) = match &verdict {
                    AgentVerdict::Run => {
                        self.run_agent_sql(id, resolved, sql, reply, arrived, cx);
                        return;
                    }
                    // A write is held for a person, unless nothing can run it
                    // for an agent on this engine: then it is refused here,
                    // before there is a pending call or a dialog to be shown.
                    AgentVerdict::Approve(held, _) if held.mode == Mode::ReadWrite => {
                        match resolved.config.agent_write_refusal() {
                            Some(refusal) => (
                                refusal.to_string(),
                                Some(AuditVerdict::Refuse(refusal.to_string())),
                            ),
                            None => {
                                let expiry = self.expiry_of(id, cx);
                                self.agent.calls.push(PendingCall {
                                    id,
                                    profile_id: resolved.id,
                                    tool: Tool::ExecuteSql,
                                    profile_name: resolved.name,
                                    reply,
                                    arrived,
                                    approval: Some(Approval {
                                        sql,
                                        verdict,
                                        config: resolved.config,
                                    }),
                                    _expiry: expiry,
                                });
                                cx.notify();
                                return;
                            }
                        }
                    }
                    AgentVerdict::Approve(..) => {
                        (NEEDS_APPROVAL.to_string(), AuditVerdict::held_by(verdict))
                    }
                    AgentVerdict::Refuse(reason) => {
                        (reason.to_string(), AuditVerdict::held_by(verdict))
                    }
                };
                self.agent_answered(
                    id,
                    Tool::ExecuteSql,
                    Some(resolved.name),
                    None,
                    &reply,
                    Reply {
                        status: Status::RefusedByGate,
                        text: refused,
                    },
                    CallFacts {
                        verdict: audited,
                        sql: Some(sql),
                        elapsed_ms: elapsed_ms(arrived),
                        ..CallFacts::default()
                    },
                );
            }
            Inbound::Call {
                tool: Tool::ExplainQuery,
                id,
                connection,
                args,
                reply,
            } => {
                self.adopt_saved_visibility_for_read(cx);
                let found = match connection.as_deref() {
                    Some(name) => self
                        .agent_profile(name)
                        .map(|profile| (Resolved::of(profile), profile.mode)),
                    None => Err(AgentLookup::NotAvailable),
                };
                let (resolved, mode) = match found {
                    Ok(found) => found,
                    Err(lookup) => {
                        self.agent_answered(
                            id,
                            Tool::ExplainQuery,
                            None,
                            connection.as_deref(),
                            &reply,
                            Reply {
                                status: Status::RefusedByGate,
                                text: lookup.to_string(),
                            },
                            CallFacts {
                                elapsed_ms: elapsed_ms(arrived),
                                ..CallFacts::default()
                            },
                        );
                        return;
                    }
                };
                // The schema requires a statement, so only an injected call
                // has none; it is the empty statement.
                let sql = args.sql.unwrap_or_default();
                let engine = resolved.config.engine();
                match explain_statement(engine, &sql, args.analyze.unwrap_or(false), mode) {
                    Ok(statement) => self.run_agent_work(
                        id,
                        resolved,
                        AgentWork::Explain { statement },
                        reply,
                        arrived,
                        cx,
                    ),
                    Err(declined) => self.agent_answered(
                        id,
                        Tool::ExplainQuery,
                        Some(resolved.name),
                        None,
                        &reply,
                        Reply {
                            status: declined.status,
                            text: declined.text,
                        },
                        CallFacts {
                            verdict: declined.verdict.and_then(AuditVerdict::held_by),
                            sql: declined.sql,
                            elapsed_ms: elapsed_ms(arrived),
                            ..CallFacts::default()
                        },
                    ),
                }
            }
            Inbound::Call {
                tool: tool @ (Tool::ListSchemas | Tool::ListTables | Tool::DescribeTable),
                id,
                connection,
                args,
                reply,
            } => {
                self.adopt_saved_visibility_for_read(cx);
                let resolved = match connection.as_deref() {
                    Some(name) => self.agent_profile(name).map(Resolved::of),
                    None => Err(AgentLookup::NotAvailable),
                };
                let resolved = match resolved {
                    Ok(resolved) => resolved,
                    Err(lookup) => {
                        self.agent_answered(
                            id,
                            tool,
                            None,
                            connection.as_deref(),
                            &reply,
                            Reply {
                                status: Status::RefusedByGate,
                                text: lookup.to_string(),
                            },
                            CallFacts {
                                elapsed_ms: elapsed_ms(arrived),
                                ..CallFacts::default()
                            },
                        );
                        return;
                    }
                };
                // The names are required by the tools that take them, so only
                // an injected call lacks one; it names nothing the catalog holds.
                let work = match tool {
                    Tool::DescribeTable => AgentWork::Structure {
                        schema: args.schema.unwrap_or_default(),
                        table: args.table.unwrap_or_default(),
                    },
                    Tool::ListTables => AgentWork::Catalog {
                        schema: Some(args.schema.unwrap_or_default()),
                    },
                    _ => AgentWork::Catalog { schema: None },
                };
                self.run_agent_work(id, resolved, work, reply, arrived, cx);
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
                            CallFacts::default(),
                        );
                        return;
                    }
                };
                let expiry = self.expiry_of(id, cx);
                self.agent.calls.push(PendingCall {
                    id,
                    profile_id,
                    tool: Tool::Ping,
                    profile_name,
                    reply,
                    arrived,
                    approval: None,
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

    /// Answer a read call at once and log it: the end of a call that never
    /// waits for anyone. `resolved` is the profile's own name, `asked` what the
    /// agent called a connection that resolved to nothing.
    #[allow(
        clippy::too_many_arguments,
        reason = "the call's identity, its two names, both halves of the answer and its facts are each one thing; a struct of them would be built once per call to be taken apart at once"
    )]
    pub(super) fn agent_answered(
        &mut self,
        id: CallId,
        tool: Tool,
        resolved: Option<String>,
        asked: Option<&str>,
        reply: &SyncSender<Reply>,
        answer: Reply,
        facts: CallFacts,
    ) {
        let status = answer.status;
        // The caller may have hung up, and then nobody reads this.
        let _ = reply.send(answer);
        self.agent.record(id, tool, resolved, asked, status, facts);
    }

    /// The one door every call ends through: answered, expired or hung up.
    /// It removes the call by id before it replies, so two things ending the
    /// same call at once produce one outcome and at most one reply. It looks
    /// among the calls waiting on a person first (pings and held statements),
    /// then among the approved writes running, which a hangup is noted on and
    /// nothing else ends, and last among the reads of every lane
    /// ([`Workspace::finish_read`]), where a running read is cancelled on the
    /// server. Answers whether there was such a call.
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
            // A running write ends by its own result alone: a hangup is noted
            // for the audit and nothing else reaches it.
            if let Some(write) = self.agent.writes.iter_mut().find(|write| write.id == id) {
                write.requester_gone |= status == Status::Cancelled;
                return true;
            }
            return self.finish_read(id, status, text, true, Default::default(), cx);
        };
        let was_in_front = self.front_ping().is_some_and(|front| front.id == id);
        let call = self.agent.calls.remove(index);
        if status != Status::Cancelled {
            // The caller may have hung up, and then nobody reads this.
            let _ = call.reply.send(Reply { status, text });
        }
        // A held statement is audited with what it asked, so a denied, expired
        // or hung-up approval says which statement it was.
        let (verdict, sql, decision, engine) = match call.approval {
            Some(approval) => (
                AuditVerdict::held_by(approval.verdict),
                Some(approval.sql),
                // A hangup is no decision of the person's: its caller left.
                match status {
                    Status::Rejected => Some(Decision::Rejected),
                    Status::Expired => Some(Decision::Expired),
                    _ => None,
                },
                Some(approval.config.engine()),
            ),
            None => (None, None, None, None),
        };
        // Past the early return above, so a hangup that arrives after the call
        // ended adds nothing: one entry per call, written here.
        self.agent.record(
            id,
            call.tool,
            Some(call.profile_name),
            None,
            status,
            CallFacts {
                verdict,
                sql,
                elapsed_ms: elapsed_ms(call.arrived),
                decision,
                engine,
                ..CallFacts::default()
            },
        );
        if self.agent.armed == Some(id) {
            self.agent.armed = None;
        }
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
    fn an_audit_entry_keeps_a_cut_of_a_long_error_as_it_does_of_a_long_statement() {
        let mut state = AgentState::default();
        let long = "e".repeat(AUDIT_SQL_CAP * 3);
        state.record(
            CallId(1),
            Tool::ExecuteSql,
            None,
            None,
            Status::Failed,
            CallFacts {
                sql: Some(long.clone()),
                error: Some(long),
                ..CallFacts::default()
            },
        );
        let entry = state.log.front().expect("the call is logged");
        for kept in [entry.sql.as_deref(), entry.error.as_deref()] {
            let kept = kept.expect("kept");
            assert_eq!(kept.chars().count(), AUDIT_SQL_CAP + SQL_CLIPPED.len());
            assert!(kept.ends_with(SQL_CLIPPED));
        }
    }

    #[test]
    fn the_dialog_states_the_engines_hold_and_that_rows_leave_the_machine() {
        for engine in Engine::ALL {
            for mode in Mode::ALL {
                let facts = visibility_facts(engine, mode, false);
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
                let facts = visibility_facts(engine, mode, false);
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
    fn the_dialog_states_the_commit_wait_only_when_a_read_can_block_it() {
        const WAIT: &str = "An agent's read in progress can make your commit on this file wait, \
                            and then fail with \"database is locked\".";
        for engine in Engine::ALL {
            for mode in Mode::ALL {
                let count = |facts: &[String]| facts.iter().filter(|fact| *fact == WAIT).count();
                let blocked = visibility_facts(engine, mode, true);
                assert_eq!(count(&blocked), 1, "{engine:?} {mode:?}: {blocked:?}");
                let free = visibility_facts(engine, mode, false);
                assert_eq!(count(&free), 0, "{engine:?} {mode:?}: {free:?}");
                for fact in blocked.iter().chain(&free) {
                    for word in ["yet", "Phase", "MCP", "try", "should", "upgrade"] {
                        assert!(!fact.contains(word), "{engine:?} {mode:?}: {fact}");
                    }
                }
            }
        }
    }

    #[test]
    fn an_approval_with_an_expiry_is_audited_with_its_text() {
        use crate::sql::{Expiry, Verdict};

        let verdict = Verdict {
            mode: Mode::ReadWrite,
            destructive: Vec::new(),
        };
        assert_eq!(
            AuditVerdict::held_by(AgentVerdict::Approve(verdict.clone(), Some(Expiry::Key))),
            Some(AuditVerdict::Approve {
                mode: Mode::ReadWrite,
                expiry: Some(Expiry::Key.to_string()),
            })
        );
        assert_eq!(
            AuditVerdict::held_by(AgentVerdict::Approve(verdict, None)),
            Some(AuditVerdict::Approve {
                mode: Mode::ReadWrite,
                expiry: None,
            })
        );
        assert_eq!(AuditVerdict::held_by(AgentVerdict::Run), None);
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
