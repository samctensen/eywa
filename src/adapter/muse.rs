//! The Muse Code adapter. See `docs/research/muse.md`.
//!
//! Records arrive with the root session as `session_id` and, for events
//! raised inside a subagent thread, that thread as `agent_id` (both from the
//! hook environment). Subagent lifecycle events name the child instead.
//! There is no interrupt hook; `ToolUseStart` reports a granted approval.

use super::claude::approval_id;
use super::{Adapter, Reason, Sessions, Tracker, close, name_attr, open_session, session_path};
use crate::record::HookRecord;
use crate::tree::{Approvals, Attrs, Client, Event, Interrupts, Millis, Observes, Outcome};

const CLIENT: Client = Client {
    name: "muse",
    observes: Observes {
        agents: true,
        approvals: Approvals::Actor,
        questions: false,
        mcp_input: false,
        interrupts: Interrupts::Inferred,
    },
};

#[derive(Default)]
pub struct MuseAdapter {
    sessions: Sessions,
}

impl MuseAdapter {
    pub fn new() -> MuseAdapter {
        MuseAdapter::default()
    }
}

impl Adapter for MuseAdapter {
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
            if record.agent_id.is_none() {
                events.push(close(session, Outcome::Ok));
                self.sessions.end(&record.session_id);
            }
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
    let lifecycle = matches!(record.event.as_str(), "SubagentStart" | "SubagentStop");
    let thread = record.agent_id.as_deref().filter(|_| !lifecycle);
    let actor = match thread {
        Some(thread) => tracker.agent(thread, None),
        None => tracker.root(record.turn_id.as_deref()),
    };
    let tool_use_id = record.tool_use_id.as_deref();
    match record.event.as_str() {
        "UserPromptSubmit" if thread.is_some() => {
            if let Some(actor) = &actor {
                tracker.enter(actor, events);
            }
        }
        "UserPromptSubmit" => tracker.begin_turn(record.turn_id.as_deref(), Attrs::new(), events),
        "PreToolUse" => {
            if let Some(actor) = &actor {
                tracker.answer_approvals(actor, at, events);
                tracker.remember_call(actor, record.digest.as_deref(), tool_use_id);
                tracker.enter(actor, events);
            }
        }
        "PermissionRequest" => {
            if let Some(actor) = &actor {
                let id = approval_id(tracker, actor, record);
                let about = name_attr("tool_name", record.tool_name.as_deref());
                tracker.open_request(actor, &id, Reason::Approval, about, at, events);
            }
        }
        // The notification doesn't name a tool.
        "Notification" if record.notification_type.as_deref() == Some("permission_prompt") => {
            if tracker.has_requests() {
                tracker.mark_prompted(events);
            } else if let Some(root) = tracker.root(None) {
                let about = Attrs::new();
                tracker.open_request(&root, "prompt", Reason::Approval, about, at, events);
            }
        }
        // The approved call has started.
        "ToolUseStart" => {
            if let Some(id) = tool_use_id {
                tracker.close_request(id, Outcome::Ok, events);
            }
        }
        "PostToolUse" | "PostToolUseFailure" => {
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
        // Muse names a subagent's type only on hooks Eywa doesn't open it from.
        "SubagentStart" => {
            if let Some(child) = &record.agent_id {
                tracker.start_agent(child, None, None, events);
            }
        }
        "SubagentStop" => {
            if let Some(child) = &record.agent_id {
                tracker.stop_agent(child, Outcome::Ok, events);
            }
        }
        "Stop" => match (thread, &actor) {
            (Some(_), Some(actor)) => tracker.enter(actor, events),
            (None, _) => tracker.end_turn(record.turn_id.as_deref(), Outcome::Ok, events),
            _ => {}
        },
        "StopFailure" => match thread {
            Some(thread) => tracker.stop_agent(thread, Outcome::Error, events),
            None => tracker.end_turn(record.turn_id.as_deref(), Outcome::Error, events),
        },
        _ => {}
    }
}
