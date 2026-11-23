//! Adapters turn one client's hook records into tree events. The bookkeeping
//! every client needs (turns, subagents, open tool calls and the requests
//! waiting on them) lives in [`Tracker`]; each adapter maps its client's
//! events onto it.
//!
//! Synchronous hooks block the agent, so one thread's events arrive in order;
//! only different threads interleave. Activity carrying a closed node's own id
//! therefore reopens it: a blocked `Stop` continues its turn, and a resumed
//! subagent starts again.

pub mod claude;
pub mod codex;
pub mod grok;
pub mod muse;

use crate::record::HookRecord;
use crate::tree::{Attrs, Client, Event, Millis, Outcome};
use std::collections::{HashMap, HashSet, VecDeque};

pub trait Adapter: Send {
    fn client(&self) -> Client;
    fn handle(&mut self, record: &HookRecord, at: Millis) -> Vec<Event>;
    /// Applies a session's state as the client's own records state it,
    /// outside hooks. Only Claude Code has such a source so far.
    fn observe(&mut self, _state: &crate::registry::SessionState, _at: Millis) -> Vec<Event> {
        Vec::new()
    }
    /// The settle time has passed since the session was observed going
    /// idle: whatever the adapter was waiting to learn, it decides now.
    fn settle(&mut self, _session_id: &str, _at: Millis) -> Vec<Event> {
        Vec::new()
    }
    /// Drops all state for a session that ended outside the adapter's view,
    /// for example because its process exited.
    fn forget_session(&mut self, session_id: &str);
}

/// Parallel tool calls run their hooks at the same moment, so a sibling's
/// `PreToolUse` can arrive just after another call's approval request. A new
/// tool call answers an approval only once the approval has been open this
/// long, which is less than anyone takes to respond to a prompt.
pub(crate) const SIBLING_WINDOW: Millis = 1_000;

/// How many open tool calls per actor are remembered for matching an
/// approval to its call.
const RECENT_CALLS: usize = 64;

pub fn session_path(client: &str, session: &str) -> String {
    format!("/{client}/session:{session}")
}

pub fn turn_path(session_path: &str, turn: &str) -> String {
    format!("{session_path}/turn:{turn}")
}

pub fn agent_path(turn_path: &str, agent: &str) -> String {
    format!("{turn_path}/agent:{agent}")
}

pub fn request_path(actor_path: &str, request: &str) -> String {
    format!("{actor_path}/request:{request}")
}

pub(crate) fn open(id: String, attrs: Attrs) -> Event {
    Event::Open { id, attrs }
}

pub(crate) fn close(id: String, outcome: Outcome) -> Event {
    Event::Close { id, outcome }
}

pub(crate) fn retract(id: String) -> Event {
    Event::Retract { id }
}

/// Opens (or refreshes) a session with the attributes a record carries.
pub(crate) fn open_session(path: &str, record: &HookRecord) -> Event {
    let mut attrs = Attrs::new();
    if let Some(cwd) = &record.cwd {
        attrs.insert("cwd".into(), cwd.clone().into());
    }
    if let Some(pid) = record.pid {
        attrs.insert("pid".into(), pid.into());
    }
    if let Some(surface) = &record.surface {
        attrs.insert("surface".into(), surface.clone().into());
    }
    open(path.to_string(), attrs)
}

/// One attribute holding a name the client stated, or none if it stated none.
pub(crate) fn name_attr(key: &str, name: Option<&str>) -> Attrs {
    let mut attrs = Attrs::new();
    if let Some(name) = name.filter(|name| !name.is_empty()) {
        attrs.insert(key.into(), name.into());
    }
    attrs
}

/// A request id built from free text, such as an MCP server name, made safe
/// to use as a path segment.
pub(crate) fn request_key(prefix: &str, text: &str) -> String {
    let key: String = format!("{prefix}-{text}")
        .chars()
        .map(|c| if c == '/' { '-' } else { c })
        .take(crate::record::MAX_ID_BYTES)
        .collect();
    key
}

/// Why a request is waiting on the user.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Reason {
    Approval,
    Question,
    McpInput,
}

impl Reason {
    fn label(self) -> &'static str {
        match self {
            Reason::Approval => "approval",
            Reason::Question => "question",
            Reason::McpInput => "mcp_input",
        }
    }
}

/// Whoever raised an event: the main thread working in a root turn, or a
/// subagent.
#[derive(Clone, Debug)]
pub(crate) struct Actor {
    /// The subagent id, or empty for the main thread.
    pub key: String,
    /// The root turn the actor works in.
    pub turn: String,
    pub path: String,
    /// The subagent's type, when the record at hand names it.
    pub agent_type: Option<String>,
}

impl Actor {
    pub(crate) fn typed(mut self, agent_type: Option<&str>) -> Actor {
        self.agent_type = agent_type.filter(|kind| !kind.is_empty()).map(String::from);
        self
    }
}

struct Request {
    actor: String,
    path: String,
    reason: Reason,
    opened_at: Millis,
}

struct Agent {
    turn: String,
    open: bool,
    agent_type: Option<String>,
}

/// One session's bookkeeping.
pub(crate) struct Tracker {
    session: String,
    open_turn: Option<String>,
    latest_turn: Option<String>,
    numbered: u64,
    agents: HashMap<String, Agent>,
    /// Open tool calls per actor: `(digest, tool_use_id)`, oldest first.
    calls: HashMap<String, VecDeque<(String, String)>>,
    newest_call: Option<String>,
    requests: HashMap<String, Request>,
}

impl Tracker {
    pub(crate) fn new(session: String) -> Tracker {
        Tracker {
            session,
            open_turn: None,
            latest_turn: None,
            numbered: 0,
            agents: HashMap::new(),
            calls: HashMap::new(),
            newest_call: None,
            requests: HashMap::new(),
        }
    }

    /// The main thread, in `turn` or else in the open turn.
    pub(crate) fn root(&self, turn: Option<&str>) -> Option<Actor> {
        let turn = turn.map(String::from).or_else(|| self.open_turn.clone())?;
        Some(Actor {
            key: String::new(),
            path: turn_path(&self.session, &turn),
            turn,
            agent_type: None,
        })
    }

    /// Subagent `agent`, in the root turn that started it, else in `turn`,
    /// else in the open or latest root turn. A subagent's own turn ids never
    /// become root turns.
    pub(crate) fn agent(&self, agent: &str, turn: Option<&str>) -> Option<Actor> {
        let turn = self
            .agents
            .get(agent)
            .map(|known| known.turn.clone())
            .or_else(|| turn.map(String::from))
            .or_else(|| self.open_turn.clone())
            .or_else(|| self.latest_turn.clone())?;
        Some(Actor {
            key: agent.to_string(),
            path: agent_path(&turn_path(&self.session, &turn), agent),
            turn,
            agent_type: None,
        })
    }

    /// Starts a turn, numbered when the client gives no id. A different
    /// open turn was interrupted and closes as cancelled; the same id
    /// (steering, a repeated prompt) only confirms it.
    pub(crate) fn begin_turn(&mut self, turn: Option<&str>, attrs: Attrs, events: &mut Vec<Event>) {
        let turn = turn.map(String::from).unwrap_or_else(|| {
            self.numbered += 1;
            format!("n{}", self.numbered)
        });
        if let Some(previous) = self.open_turn.clone().filter(|open| *open != turn) {
            self.end_turn(Some(&previous), Outcome::Cancelled, events);
        }
        events.push(open(turn_path(&self.session, &turn), attrs));
        self.open_turn = Some(turn.clone());
        self.latest_turn = Some(turn);
    }

    /// Ends `turn`, or the open turn. The main thread's requests in it close
    /// with it; its subagents keep running.
    pub(crate) fn end_turn(
        &mut self,
        turn: Option<&str>,
        outcome: Outcome,
        events: &mut Vec<Event>,
    ) {
        let Some(turn) = turn.map(String::from).or_else(|| self.open_turn.clone()) else {
            return;
        };
        self.finish_turn(&turn, |id| close(id, outcome), events);
    }

    /// Ends the open turn before its outcome is known, and returns its id.
    /// Whoever calls this records the outcome on the session once it is.
    pub(crate) fn end_turn_unsettled(&mut self, events: &mut Vec<Event>) -> Option<String> {
        let turn = self.open_turn.clone()?;
        self.finish_turn(&turn, |id| Event::End { id }, events);
        Some(turn)
    }

    /// Gives the open turn its real id, `turn`, when it was opened under a
    /// placeholder. It is one turn and it goes on running, so nothing is
    /// recorded about how any turn ended.
    pub(crate) fn rename_turn(&mut self, turn: &str, events: &mut Vec<Event>) {
        let Some(placeholder) = self.open_turn.clone().filter(|open| open != turn) else {
            return;
        };
        events.push(open(turn_path(&self.session, turn), Attrs::new()));
        self.finish_turn(&placeholder, retract, events);
        self.open_turn = Some(turn.to_string());
        self.latest_turn = Some(turn.to_string());
    }

    fn finish_turn(
        &mut self,
        turn: &str,
        ended: impl FnOnce(String) -> Event,
        events: &mut Vec<Event>,
    ) {
        let path = turn_path(&self.session, turn);
        events.push(ended(path.clone()));
        if self.open_turn.as_deref() == Some(turn) {
            self.open_turn = None;
        }
        let prefix = format!("{path}/request:");
        self.requests
            .retain(|_, request| !request.path.starts_with(&prefix));
        self.calls.remove("");
    }

    /// Opens the actor's node, or confirms it. Activity is proof the actor is
    /// running, so this reopens a turn or subagent that had closed. A
    /// subagent's activity never reopens the root turn it belongs to. A
    /// subagent's type, once named, stays with it when it opens again.
    pub(crate) fn enter(&mut self, actor: &Actor, events: &mut Vec<Event>) {
        if actor.key.is_empty() {
            events.push(open(actor.path.clone(), Attrs::new()));
            if self.open_turn.is_none() {
                self.open_turn = Some(actor.turn.clone());
            }
            self.latest_turn = Some(actor.turn.clone());
            return;
        }
        let agent = self
            .agents
            .entry(actor.key.clone())
            .or_insert_with(|| Agent {
                turn: actor.turn.clone(),
                open: true,
                agent_type: None,
            });
        agent.open = true;
        if actor.agent_type.is_some() {
            agent.agent_type = actor.agent_type.clone();
        }
        let attrs = name_attr("agent_type", agent.agent_type.as_deref());
        events.push(open(actor.path.clone(), attrs));
        // Opening the subagent opened its turn with it if the turn was gone.
        // The turn isn't running, and this is no news of how it ended.
        if self.open_turn.as_deref() != Some(actor.turn.as_str()) {
            events.push(retract(turn_path(&self.session, &actor.turn)));
        }
    }

    /// Starts (or restarts) a subagent, of `agent_type` when the client names it.
    pub(crate) fn start_agent(
        &mut self,
        agent: &str,
        turn: Option<&str>,
        agent_type: Option<&str>,
        events: &mut Vec<Event>,
    ) {
        if let Some(actor) = self.agent(agent, turn) {
            self.enter(&actor.typed(agent_type), events);
        }
    }

    /// Stops a subagent. It can start again later under the same id.
    pub(crate) fn stop_agent(&mut self, agent: &str, outcome: Outcome, events: &mut Vec<Event>) {
        let Some(actor) = self.agent(agent, None) else {
            return;
        };
        let Some(state) = self.agents.get_mut(agent).filter(|state| state.open) else {
            return;
        };
        state.open = false;
        events.push(close(actor.path, outcome));
        self.requests.retain(|_, request| request.actor != agent);
        self.calls.remove(agent);
    }

    /// Closes the open subagents a client no longer lists as running,
    /// except any waiting on the user.
    pub(crate) fn reconcile_agents(&mut self, running: &HashSet<&str>, events: &mut Vec<Event>) {
        let waiting: HashSet<String> = self
            .requests
            .values()
            .map(|request| request.actor.clone())
            .collect();
        let finished: Vec<String> = self
            .agents
            .iter()
            .filter(|(id, state)| {
                state.open && !running.contains(id.as_str()) && !waiting.contains(*id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for agent in finished {
            self.stop_agent(&agent, Outcome::Ok, events);
        }
    }

    /// Remembers a tool call that has started, for matching an approval to it.
    pub(crate) fn remember_call(
        &mut self,
        actor: &Actor,
        digest: Option<&str>,
        tool_use_id: Option<&str>,
    ) {
        let Some(tool_use_id) = tool_use_id else {
            return;
        };
        let calls = self.calls.entry(actor.key.clone()).or_default();
        calls.push_back((
            digest.unwrap_or_default().to_string(),
            tool_use_id.to_string(),
        ));
        if calls.len() > RECENT_CALLS {
            calls.pop_front();
        }
        self.newest_call = Some(tool_use_id.to_string());
    }

    pub(crate) fn forget_call(&mut self, tool_use_id: &str) {
        for calls in self.calls.values_mut() {
            calls.retain(|(_, id)| id != tool_use_id);
        }
    }

    /// The call an approval is for: the actor's newest open call with the
    /// same input, else its only open call.
    pub(crate) fn match_call(&self, actor: &Actor, digest: Option<&str>) -> Option<String> {
        let calls = self.calls.get(&actor.key)?;
        digest
            .and_then(|digest| calls.iter().rev().find(|(seen, _)| seen == digest))
            .or_else(|| (calls.len() == 1).then(|| &calls[0]))
            .map(|(_, id)| id.clone())
    }

    /// The most recently started tool call in the session, from any actor.
    pub(crate) fn newest_call(&self) -> Option<String> {
        self.newest_call.clone()
    }

    /// A new tool call from the actor answers its approvals, except within
    /// the sibling window.
    pub(crate) fn answer_approvals(&mut self, actor: &Actor, at: Millis, events: &mut Vec<Event>) {
        self.close_requests(
            |request| {
                request.actor == actor.key
                    && request.reason == Reason::Approval
                    && at.saturating_sub(request.opened_at) >= SIBLING_WINDOW
            },
            Outcome::Ok,
            events,
        );
    }

    /// Opens a request under the actor, opening the actor first if needed.
    /// `about` holds what the client says the request is for, such as the
    /// tool it asks about.
    pub(crate) fn open_request(
        &mut self,
        actor: &Actor,
        id: &str,
        reason: Reason,
        about: Attrs,
        at: Millis,
        events: &mut Vec<Event>,
    ) {
        self.enter(actor, events);
        if let Some(request) = self.requests.get(id) {
            // The same actor asking again on an open request says what it is
            // asking about now.
            if request.actor == actor.key && !about.is_empty() {
                events.push(Event::Attr {
                    id: request.path.clone(),
                    attrs: about,
                });
            }
            return;
        }
        let path = request_path(&actor.path, id);
        let mut attrs = Attrs::new();
        attrs.insert("reason".into(), reason.label().into());
        attrs.extend(about);
        events.push(open(path.clone(), attrs));
        self.requests.insert(
            id.to_string(),
            Request {
                actor: actor.key.clone(),
                path,
                reason,
                opened_at: at,
            },
        );
    }

    pub(crate) fn close_request(&mut self, id: &str, outcome: Outcome, events: &mut Vec<Event>) {
        if let Some(request) = self.requests.remove(id) {
            events.push(close(request.path, outcome));
        }
    }

    fn close_requests(
        &mut self,
        matches: impl Fn(&Request) -> bool,
        outcome: Outcome,
        events: &mut Vec<Event>,
    ) {
        let ids: Vec<String> = self
            .requests
            .iter()
            .filter(|(_, request)| matches(request))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.close_request(&id, outcome, events);
        }
    }

    /// Closes the actor's open requests of one kind.
    pub(crate) fn close_reason(
        &mut self,
        actor: Option<&Actor>,
        reason: Reason,
        events: &mut Vec<Event>,
    ) {
        self.close_requests(
            |request| {
                request.reason == reason && actor.is_none_or(|actor| request.actor == actor.key)
            },
            Outcome::Ok,
            events,
        );
    }

    pub(crate) fn close_all_requests(&mut self, events: &mut Vec<Event>) {
        self.close_requests(|_| true, Outcome::Ok, events);
    }

    pub(crate) fn has_requests(&self) -> bool {
        !self.requests.is_empty()
    }

    /// Marks open approvals as confirmed shown to the user: some clients
    /// ask an automatic reviewer before they ask a person.
    pub(crate) fn mark_prompted(&self, events: &mut Vec<Event>) {
        for request in self
            .requests
            .values()
            .filter(|r| r.reason == Reason::Approval)
        {
            let mut attrs = Attrs::new();
            attrs.insert("prompted".into(), true.into());
            events.push(Event::Attr {
                id: request.path.clone(),
                attrs,
            });
        }
    }
}

/// Every session of one client, plus the ones that have ended.
#[derive(Default)]
pub(crate) struct Sessions {
    trackers: HashMap<String, Tracker>,
    ended: EndedSessions,
}

impl Sessions {
    /// Whether to process a record. An ended session stays ended for late
    /// records, but a new start or prompt brings it back: a resumed session
    /// reuses its id.
    pub(crate) fn admits(&mut self, record: &HookRecord, session_id: &str) -> bool {
        self.ended.admits(session_id, &record.event)
    }

    pub(crate) fn is_ended(&self, session_id: &str) -> bool {
        self.ended.set.contains(session_id)
    }

    pub(crate) fn tracker(&mut self, client: &str, session_id: &str) -> &mut Tracker {
        self.trackers
            .entry(session_id.to_string())
            .or_insert_with(|| Tracker::new(session_path(client, session_id)))
    }

    pub(crate) fn end(&mut self, session_id: &str) {
        self.trackers.remove(session_id);
        self.ended.insert(session_id);
    }
}

/// Sessions that have ended, bounded so a long-running service doesn't grow
/// without limit.
#[derive(Default)]
struct EndedSessions {
    order: VecDeque<String>,
    set: HashSet<String>,
}

const ENDED_CAPACITY: usize = 1024;

impl EndedSessions {
    fn insert(&mut self, session_id: &str) {
        if self.set.insert(session_id.to_string()) {
            self.order.push_back(session_id.to_string());
            if self.order.len() > ENDED_CAPACITY
                && let Some(oldest) = self.order.pop_front()
            {
                self.set.remove(&oldest);
            }
        }
    }

    fn admits(&mut self, session_id: &str, event: &str) -> bool {
        if !self.set.contains(session_id) {
            return true;
        }
        if !matches!(event, "SessionStart" | "UserPromptSubmit") {
            return false;
        }
        self.set.remove(session_id);
        self.order.retain(|session| session != session_id);
        true
    }
}
