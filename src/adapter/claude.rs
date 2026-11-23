//! The Claude Code adapter. See the design's "Claude Code" adapter section
//! and `docs/research/anthropic.md`.

use super::{
    Actor, Adapter, Reason, Sessions, Tracker, close, name_attr, open, open_session, request_key,
    session_path,
};
use crate::record::{HookRecord, claude_surface};
use crate::registry::{SessionState, Status};
use crate::tree::{
    Approvals, Attrs, Client, Event, Interrupts, Millis, Observes, Outcome, last_turn,
};
use std::collections::{HashMap, HashSet};

const CLIENT: Client = Client {
    name: "claude",
    observes: Observes {
        agents: true,
        approvals: Approvals::Actor,
        questions: true,
        mcp_input: true,
        interrupts: Interrupts::Inferred,
    },
};

/// The turn opened for a busy session whose turn no hook has reported yet:
/// Eywa started mid-turn, or the registry reported the turn before its
/// prompt hook did. The first hook that names the real turn renames it.
const UNREPORTED_TURN: &str = "running";

/// A turn the session registry ended before any hook said how it ended.
struct Unsettled {
    turn: String,
    ended_at: Millis,
}

#[derive(Default)]
pub struct ClaudeAdapter {
    sessions: Sessions,
    /// The session each Claude process is running.
    processes: HashMap<u32, String>,
    /// Each session's turn that has ended with its outcome still to come.
    unsettled: HashMap<String, Unsettled>,
}

impl ClaudeAdapter {
    pub fn new() -> ClaudeAdapter {
        ClaudeAdapter::default()
    }

    fn end(&mut self, session_id: &str) {
        self.sessions.end(session_id);
        self.processes.retain(|_, session| session != session_id);
        self.unsettled.remove(session_id);
    }

    /// A Claude process runs one session at a time, so a new session id
    /// from the same process (`/clear`, `/resume`, a fork) ends the old one.
    fn claim_process(&mut self, pid: u32, session_id: &str, events: &mut Vec<Event>) {
        if let Some(previous) = self.processes.insert(pid, session_id.to_string())
            && previous != session_id
        {
            events.push(close(session_path(CLIENT.name, &previous), Outcome::Ok));
            self.end(&previous);
        }
    }
}

impl Adapter for ClaudeAdapter {
    fn client(&self) -> Client {
        CLIENT
    }

    fn handle(&mut self, record: &HookRecord, at: Millis) -> Vec<Event> {
        if !self.sessions.admits(record, &record.session_id) {
            return Vec::new();
        }
        let mut events = Vec::new();
        if record.event == "SessionStart"
            && let Some(pid) = record.pid
        {
            self.claim_process(pid, &record.session_id, &mut events);
        }
        let session = session_path(CLIENT.name, &record.session_id);
        events.push(open_session(&session, record));
        if record.event == "SessionEnd" {
            events.push(close(session, Outcome::Ok));
            self.end(&record.session_id);
            return events;
        }
        if record.agent_id.is_none()
            && let Some(unsettled) = self.unsettled.get(&record.session_id)
        {
            let outcome = match record.event.as_str() {
                "Stop" => Some(Outcome::Ok),
                "StopFailure" => Some(Outcome::Error),
                "UserPromptSubmit" if record.turn_id.as_deref() != Some(&unsettled.turn) => {
                    Some(Outcome::Cancelled)
                }
                // The same turn again: it continues, so it has no outcome yet.
                "UserPromptSubmit" => None,
                // A late hook from the ended turn must not reopen it.
                _ => return events,
            };
            let ended_at = unsettled.ended_at;
            self.unsettled.remove(&record.session_id);
            if let Some(outcome) = outcome {
                events.push(record_outcome(&session, outcome, ended_at));
            }
        }
        let tracker = self.sessions.tracker(CLIENT.name, &record.session_id);
        handle_event(tracker, record, at, &mut events);
        events
    }

    /// No `Stop` hook followed the idle: the turn was interrupted.
    fn settle(&mut self, session_id: &str, _at: Millis) -> Vec<Event> {
        let session = session_path(CLIENT.name, session_id);
        self.unsettled
            .remove(session_id)
            .map(|unsettled| record_outcome(&session, Outcome::Cancelled, unsettled.ended_at))
            .into_iter()
            .collect()
    }

    fn forget_session(&mut self, session_id: &str) {
        self.end(session_id);
    }

    fn observe(&mut self, state: &SessionState, at: Millis) -> Vec<Event> {
        if self.sessions.is_ended(&state.session_id) {
            return Vec::new();
        }
        let mut events = Vec::new();
        self.claim_process(state.pid, &state.session_id, &mut events);
        let mut attrs = Attrs::new();
        attrs.insert("pid".into(), state.pid.into());
        if let Some(cwd) = &state.cwd {
            attrs.insert("cwd".into(), cwd.clone().into());
        }
        if let Some(entrypoint) = &state.entrypoint {
            attrs.insert("surface".into(), claude_surface(entrypoint).into());
        }
        events.push(open(session_path(CLIENT.name, &state.session_id), attrs));
        let tracker = self.sessions.tracker(CLIENT.name, &state.session_id);
        if state.status != Status::Idle
            && let Some(unsettled) = self.unsettled.remove(&state.session_id)
            && let Some(actor) = tracker.root(Some(&unsettled.turn))
        {
            // Running again: the idle didn't hold, so the turn it closed is
            // still the one running and has no outcome.
            tracker.enter(&actor, &mut events);
        }
        match &state.status {
            // Busy again after waiting: whatever it waited on was answered.
            Status::Busy => {
                running_turn(tracker, &mut events);
                tracker.close_all_requests(&mut events);
            }
            Status::Waiting(reason) => {
                let actor = running_turn(tracker, &mut events);
                if tracker.has_requests() {
                    tracker.mark_prompted(&mut events);
                } else if let Some(actor) = actor {
                    // The registry says what kind of input, never which tool.
                    let reason = waiting_reason(reason.as_deref());
                    let about = Attrs::new();
                    tracker.open_request(&actor, "waiting", reason, about, at, &mut events);
                }
            }
            // Idle with a turn still open: the turn was interrupted, an
            // approval was denied and the agent stopped, or the turn finished
            // and its `Stop` hook is still on its way. It ends now; `handle`
            // or `settle` records how.
            Status::Idle => {
                if let Some(turn) = tracker.end_turn_unsettled(&mut events) {
                    let unsettled = Unsettled { turn, ended_at: at };
                    self.unsettled.insert(state.session_id.clone(), unsettled);
                }
            }
        }
        events
    }
}

/// Records how a session's most recent turn ended.
fn record_outcome(session: &str, outcome: Outcome, ended_at: Millis) -> Event {
    let mut attrs = Attrs::new();
    attrs.insert("last_turn".into(), last_turn(outcome, ended_at));
    Event::Attr {
        id: session.to_string(),
        attrs,
    }
}

/// The open turn's main thread, opening the unreported turn if hooks haven't
/// reported one.
fn running_turn(tracker: &mut Tracker, events: &mut Vec<Event>) -> Option<Actor> {
    match tracker.root(None) {
        Some(actor) => {
            tracker.enter(&actor, events);
            Some(actor)
        }
        None => {
            tracker.begin_turn(Some(UNREPORTED_TURN), Attrs::new(), events);
            tracker.root(None)
        }
    }
}

fn waiting_reason(waiting_for: Option<&str>) -> Reason {
    match waiting_for {
        Some(reason) if reason.contains("question") => Reason::Question,
        Some(reason) if reason.contains("elicitation") || reason.contains("mcp") => {
            Reason::McpInput
        }
        _ => Reason::Approval,
    }
}

fn handle_event(tracker: &mut Tracker, record: &HookRecord, at: Millis, events: &mut Vec<Event>) {
    let turn = record.turn_id.as_deref();
    // The real turn has been reported: the placeholder was that turn all
    // along, so it takes the real id and nothing ends.
    if record.agent_id.is_none()
        && let Some(turn) = turn
        && tracker
            .root(None)
            .is_some_and(|actor| actor.turn == UNREPORTED_TURN)
    {
        tracker.rename_turn(turn, events);
    }
    let actor = match &record.agent_id {
        Some(agent) => tracker.agent(agent, turn),
        None => tracker.root(turn),
    };
    let tool_use_id = record.tool_use_id.as_deref();
    match record.event.as_str() {
        "UserPromptSubmit" => {
            // `source` tells a person's prompt from a turn the machine started,
            // such as a background task reporting back.
            let mut attrs = Attrs::new();
            if let Some(source) = &record.source {
                let origin = if matches!(source.as_str(), "user" | "sdk") {
                    "person"
                } else {
                    "machine"
                };
                attrs.insert("origin".into(), origin.into());
            }
            tracker.begin_turn(turn, attrs, events);
        }
        "Stop" => {
            tracker.end_turn(turn, Outcome::Ok, events);
            reconcile(tracker, record, events);
        }
        "StopFailure" => tracker.end_turn(turn, Outcome::Error, events),
        "Notification" => match record.notification_type.as_deref() {
            // Claude has no interrupt hook: waiting at the prompt means the
            // turn is over.
            Some("idle_prompt") => tracker.end_turn(None, Outcome::Cancelled, events),
            Some("permission_prompt") if tracker.has_requests() => tracker.mark_prompted(events),
            // A prompt no hook announced, such as a sandbox network approval.
            // The notification doesn't name a tool.
            Some("permission_prompt") => {
                if let Some(actor) = &actor {
                    let about = Attrs::new();
                    tracker.open_request(actor, "prompt", Reason::Approval, about, at, events);
                }
            }
            Some("elicitation_complete" | "elicitation_response") => {
                tracker.close_reason(None, Reason::McpInput, events);
            }
            _ => {}
        },
        // Only the start names the agent's own type: `SubagentStop` can name
        // the session's agent instead, for Claude's internal agents.
        "SubagentStart" => {
            if let Some(agent) = &record.agent_id {
                tracker.start_agent(agent, turn, record.agent_type.as_deref(), events);
            }
        }
        "SubagentStop" | "TeammateIdle" => {
            if let Some(agent) = &record.agent_id {
                tracker.stop_agent(agent, Outcome::Ok, events);
            }
            reconcile(tracker, record, events);
        }
        "PreToolUse" => {
            if let Some(actor) = &actor {
                tracker.answer_approvals(actor, at, events);
                tracker.remember_call(actor, record.digest.as_deref(), tool_use_id);
                match (record.tool_name.as_deref(), tool_use_id) {
                    (Some(tool @ "AskUserQuestion"), Some(id)) => {
                        let about = name_attr("tool_name", Some(tool));
                        tracker.open_request(actor, id, Reason::Question, about, at, events);
                    }
                    _ => tracker.enter(actor, events),
                }
            }
        }
        "PermissionRequest" => {
            if let Some(actor) = &actor {
                let id = approval_id(tracker, actor, record);
                let about = name_attr("tool_name", record.tool_name.as_deref());
                tracker.open_request(actor, &id, Reason::Approval, about, at, events);
            }
        }
        "PostToolUse" | "PostToolUseFailure" => {
            close_call(tracker, record, Outcome::Ok, events);
            if record
                .tool_name
                .as_deref()
                .is_some_and(|name| name.starts_with("mcp__"))
            {
                tracker.close_reason(actor.as_ref(), Reason::McpInput, events);
            }
            if let Some(actor) = &actor {
                tracker.enter(actor, events);
            }
        }
        // The calls in a batch are resolved: granted and finished, or denied.
        "PostToolBatch" => {
            for id in record.tool_use_ids.iter().flatten() {
                tracker.close_request(id, Outcome::Ok, events);
                tracker.forget_call(id);
            }
        }
        // Fires only for auto-mode denials; a person's denial sends nothing.
        "PermissionDenied" => close_call(tracker, record, Outcome::Cancelled, events),
        "Elicitation" => {
            if let Some(actor) = &actor {
                let id = elicitation_id(record);
                let about = name_attr("mcp_server", record.mcp_server.as_deref());
                tracker.open_request(actor, &id, Reason::McpInput, about, at, events);
            }
        }
        "ElicitationResult" => tracker.close_request(&elicitation_id(record), Outcome::Ok, events),
        _ => {}
    }
}

/// The request id for an approval: the call it belongs to when the input
/// matches (or the actor has only one open call), else the input digest.
pub(crate) fn approval_id(tracker: &Tracker, actor: &Actor, record: &HookRecord) -> String {
    tracker
        .match_call(actor, record.digest.as_deref())
        .or_else(|| record.digest.clone())
        .unwrap_or_else(|| "approval".to_string())
}

/// Closes whatever request a finished or denied call was holding open.
fn close_call(
    tracker: &mut Tracker,
    record: &HookRecord,
    outcome: Outcome,
    events: &mut Vec<Event>,
) {
    if let Some(id) = &record.tool_use_id {
        tracker.close_request(id, outcome, events);
        tracker.forget_call(id);
    }
    if let Some(digest) = &record.digest {
        tracker.close_request(digest, outcome, events);
    }
}

/// Elicitations may come without an id; then the server names the request.
fn elicitation_id(record: &HookRecord) -> String {
    record
        .elicitation_id
        .clone()
        .or_else(|| {
            record
                .mcp_server
                .as_deref()
                .map(|server| request_key("mcp", server))
        })
        .unwrap_or_else(|| "mcp_input".to_string())
}

/// `Stop` and `SubagentStop` list the background tasks still running.
fn reconcile(tracker: &mut Tracker, record: &HookRecord, events: &mut Vec<Event>) {
    if let Some(tasks) = &record.background_tasks {
        let running: HashSet<&str> = tasks.iter().map(|task| task.id.as_str()).collect();
        tracker.reconcile_agents(&running, events);
    }
}
