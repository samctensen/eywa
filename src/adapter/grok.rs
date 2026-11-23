//! The Grok Build adapter. See `docs/research/grok.md`.
//!
//! A Grok subagent is a child session: `SubagentStart` names it, and its own
//! events carry its session id. The adapter maps each child to the root
//! session that spawned it, flattening nested subagents under the root turn.
//! Approvals arrive only as a "permission prompt" notification with no tool
//! or turn id, so they attach to the turn and name no tool.

use super::{Adapter, Reason, Sessions, Tracker, close, name_attr, open_session, session_path};
use crate::record::HookRecord;
use crate::tree::{Approvals, Attrs, Client, Event, Interrupts, Millis, Observes, Outcome};
use std::collections::{HashMap, HashSet};

const CLIENT: Client = Client {
    name: "grok",
    observes: Observes {
        agents: true,
        approvals: Approvals::Turn,
        questions: true,
        mcp_input: false,
        interrupts: Interrupts::Hook,
    },
};

const QUESTION_TOOLS: &[&str] = &["ask_user_question", "AskQuestion"];

#[derive(Default)]
pub struct GrokAdapter {
    sessions: Sessions,
    /// Each child session and the root session it belongs to.
    children: HashMap<String, String>,
}

impl GrokAdapter {
    pub fn new() -> GrokAdapter {
        GrokAdapter::default()
    }
}

impl Adapter for GrokAdapter {
    fn client(&self) -> Client {
        CLIENT
    }

    fn handle(&mut self, record: &HookRecord, at: Millis) -> Vec<Event> {
        let child = self
            .children
            .contains_key(&record.session_id)
            .then(|| record.session_id.clone());
        let root = child
            .as_ref()
            .and_then(|child| self.children.get(child).cloned())
            .unwrap_or_else(|| record.session_id.clone());
        if !self.sessions.admits(record, &root) {
            return Vec::new();
        }
        let session = session_path(CLIENT.name, &root);
        let mut events = vec![open_session(&session, record)];
        let tracker = self.sessions.tracker(CLIENT.name, &root);
        if record.event == "SessionEnd" {
            match &child {
                Some(child) => tracker.stop_agent(child, Outcome::Ok, &mut events),
                None => {
                    events.push(close(session, Outcome::Ok));
                    self.sessions.end(&root);
                    self.children.retain(|_, parent| *parent != root);
                }
            }
            return events;
        }
        if record.event == "SubagentStart"
            && let Some(spawned) = &record.agent_id
        {
            self.children.insert(spawned.clone(), root.clone());
        }
        let tracker = self.sessions.tracker(CLIENT.name, &root);
        handle_event(tracker, record, child.as_deref(), at, &mut events);
        events
    }

    fn forget_session(&mut self, session_id: &str) {
        self.sessions.end(session_id);
        self.children.retain(|_, parent| parent != session_id);
    }
}

fn handle_event(
    tracker: &mut Tracker,
    record: &HookRecord,
    child: Option<&str>,
    at: Millis,
    events: &mut Vec<Event>,
) {
    // Tool events carry no prompt id, so the root actor works in the open turn.
    let actor = match child {
        Some(child) => tracker
            .agent(child, None)
            .map(|actor| actor.typed(record.agent_type.as_deref())),
        None => tracker.root(record.turn_id.as_deref()),
    };
    let tool_use_id = record.tool_use_id.as_deref();
    let ended = |tracker: &mut Tracker, outcome: Outcome, events: &mut Vec<Event>| match child {
        Some(child) => tracker.stop_agent(child, outcome, events),
        None => tracker.end_turn(record.turn_id.as_deref(), outcome, events),
    };
    match record.event.as_str() {
        "UserPromptSubmit" if child.is_some() => {
            if let Some(actor) = &actor {
                tracker.enter(actor, events);
            }
        }
        "UserPromptSubmit" => tracker.begin_turn(record.turn_id.as_deref(), Attrs::new(), events),
        "PreToolUse" => {
            // Approvals belong to the turn, so any new tool call can answer one.
            if let Some(root) = tracker.root(None) {
                tracker.answer_approvals(&root, at, events);
            }
            if let Some(actor) = &actor {
                tracker.remember_call(actor, record.digest.as_deref(), tool_use_id);
                match (record.tool_name.as_deref(), tool_use_id) {
                    (Some(tool), Some(id)) if QUESTION_TOOLS.contains(&tool) => {
                        let about = name_attr("tool_name", Some(tool));
                        tracker.open_request(actor, id, Reason::Question, about, at, events);
                    }
                    _ => tracker.enter(actor, events),
                }
            }
        }
        "PostToolUse" | "PostToolUseFailure" => {
            if let Some(id) = tool_use_id {
                tracker.close_request(id, Outcome::Ok, events);
                tracker.forget_call(id);
            }
            if let Some(actor) = &actor {
                tracker.enter(actor, events);
            }
        }
        "PermissionDenied" => {
            if let Some(id) = tool_use_id {
                tracker.close_request(id, Outcome::Cancelled, events);
            }
        }
        "Notification" => match record.notification_type.as_deref() {
            // Keyed by the newest call, so that call finishing answers it. That
            // the prompt is for that call is a guess, so it names no tool.
            Some("permission_prompt") => {
                if let Some(root) = tracker.root(None) {
                    let id = tracker.newest_call().unwrap_or_else(|| "approval".into());
                    let about = Attrs::new();
                    tracker.open_request(&root, &id, Reason::Approval, about, at, events);
                }
            }
            Some("idle_prompt") => tracker.end_turn(None, Outcome::Cancelled, events),
            _ => {}
        },
        // Not even a root's start is trusted to name the type, since the root
        // may be a child Eywa never saw start. The child's records name it.
        "SubagentStart" => {
            if let Some(spawned) = &record.agent_id {
                tracker.start_agent(spawned, None, None, events);
            }
        }
        "SubagentStop" => {
            if let Some(stopped) = record.agent_id.as_deref().or(child) {
                tracker.stop_agent(stopped, Outcome::Ok, events);
            }
        }
        "Stop" if child.is_none() => {
            match record.reason.as_deref() {
                Some("channel_closed" | "shutdown") => {
                    tracker.end_turn(None, Outcome::Cancelled, events);
                }
                _ => tracker.end_turn(record.turn_id.as_deref(), Outcome::Ok, events),
            }
            if let Some(tasks) = &record.background_tasks {
                let running: HashSet<&str> = tasks
                    .iter()
                    .filter(|task| task.kind.as_deref().is_none_or(|kind| kind == "subagent"))
                    .map(|task| task.id.as_str())
                    .collect();
                tracker.reconcile_agents(&running, events);
            }
        }
        "StopFailure" => ended(tracker, Outcome::Error, events),
        // Rejecting or dismissing an approval, or an interrupt, cancels the turn.
        "StopCancelled" => ended(tracker, Outcome::Cancelled, events),
        _ => {}
    }
}
