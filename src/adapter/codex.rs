//! The Codex adapter. See the design's "Codex" adapter section and
//! `docs/research/openai.md`.
//!
//! `session_id` names the root thread for a whole agent tree. Inside a
//! subagent, `agent_id` names the subagent and `turn_id` is the subagent's own
//! turn, which never becomes a root turn.

use super::claude::approval_id;
use super::{Adapter, Reason, Sessions, Tracker, close, name_attr, open_session, session_path};
use crate::record::HookRecord;
use crate::tree::{Approvals, Attrs, Client, Event, Interrupts, Millis, Observes, Outcome};

const CLIENT: Client = Client {
    name: "codex",
    observes: Observes {
        agents: true,
        approvals: Approvals::Actor,
        questions: true,
        mcp_input: false,
        interrupts: Interrupts::Hook,
    },
};

#[derive(Default)]
pub struct CodexAdapter {
    sessions: Sessions,
}

impl CodexAdapter {
    pub fn new() -> CodexAdapter {
        CodexAdapter::default()
    }
}

impl Adapter for CodexAdapter {
    fn client(&self) -> Client {
        CLIENT
    }

    fn handle(&mut self, record: &HookRecord, at: Millis) -> Vec<Event> {
        if !self.sessions.admits(record, &record.session_id) {
            return Vec::new();
        }
        let session = session_path(CLIENT.name, &record.session_id);
        let mut events = vec![open_session(&session, record)];
        if record.event == "SessionEnd" {
            events.push(close(session, Outcome::Ok));
            self.sessions.end(&record.session_id);
            return events;
        }
        let tracker = self.sessions.tracker(CLIENT.name, &record.session_id);
        handle_event(tracker, record, at, &mut events);
        events
    }

    fn forget_session(&mut self, session_id: &str) {
        self.sessions.end(session_id);
    }
}

fn handle_event(tracker: &mut Tracker, record: &HookRecord, at: Millis, events: &mut Vec<Event>) {
    let in_subagent = record.agent_id.is_some();
    // Every record from inside a subagent names its type.
    let actor = match &record.agent_id {
        Some(agent) => tracker
            .agent(agent, None)
            .map(|actor| actor.typed(record.agent_type.as_deref())),
        None => tracker.root(record.turn_id.as_deref()),
    };
    let tool_use_id = record.tool_use_id.as_deref();
    match record.event.as_str() {
        "UserPromptSubmit" if in_subagent => {
            if let Some(actor) = &actor {
                tracker.enter(actor, events);
            }
        }
        "UserPromptSubmit" => tracker.begin_turn(record.turn_id.as_deref(), Attrs::new(), events),
        // Subagent turns end with SubagentStop; only a root Stop ends a root turn.
        "Stop" if !in_subagent => {
            tracker.end_turn(record.turn_id.as_deref(), Outcome::Ok, events);
        }
        "Interrupt" => tracker.end_turn(record.turn_id.as_deref(), Outcome::Cancelled, events),
        "PreToolUse" => {
            if let Some(actor) = &actor {
                tracker.answer_approvals(actor, at, events);
                tracker.remember_call(actor, record.digest.as_deref(), tool_use_id);
                match (record.tool_name.as_deref(), tool_use_id) {
                    (Some(tool @ "request_user_input"), Some(id)) => {
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
        // Fires only when the tool succeeds.
        "PostToolUse" => {
            if let Some(id) = tool_use_id {
                tracker.close_request(id, Outcome::Ok, events);
                tracker.forget_call(id);
            }
            if let Some(digest) = &record.digest {
                tracker.close_request(digest, Outcome::Ok, events);
            }
            if let Some(actor) = &actor {
                tracker.enter(actor, events);
            }
        }
        // A subagent belongs to the root turn that is running, whatever
        // `turn_id` its own events carry.
        "SubagentStart" => {
            if let Some(agent) = &record.agent_id {
                tracker.start_agent(agent, None, record.agent_type.as_deref(), events);
            }
        }
        // Fires after each of the subagent's turns; it can run again.
        "SubagentStop" => {
            if let Some(agent) = &record.agent_id {
                tracker.stop_agent(agent, Outcome::Ok, events);
            }
        }
        _ => {}
    }
}
