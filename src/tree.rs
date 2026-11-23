//! The node tree: what Eywa has observed, and the states derived from it.
//!
//! Every node is identified by its path, for example
//! `/claude/session:7f3a/turn:12/agent:a2/request:t9`. The machine is `/` and
//! each client is `/<name>`; every deeper segment is `kind:id`.

use std::collections::BTreeMap;

pub use eywa_api::{Approvals, Interrupts, Kind, Observes, Outcome, State};

/// Milliseconds since the Unix epoch.
pub type Millis = u64;

/// How long an open turn, agent or request may go without any event before
/// it becomes unconfirmed.
pub const STALE_AFTER: Millis = 3_600_000;

pub type Attrs = serde_json::Map<String, serde_json::Value>;

/// The kind a path segment's prefix names, below the client.
fn kind_from_prefix(prefix: &str) -> Option<Kind> {
    Some(match prefix {
        "process" => Kind::Process,
        "session" => Kind::Session,
        "turn" => Kind::Turn,
        "agent" => Kind::Agent,
        "request" => Kind::Request,
        _ => return None,
    })
}

fn allows_child(parent: Kind, child: Kind) -> bool {
    use Kind::*;
    matches!(
        (parent, child),
        (Client, Process)
            | (Client, Session)
            | (Session, Turn)
            | (Turn, Agent)
            | (Turn, Request)
            | (Agent, Agent)
            | (Agent, Request)
    )
}

fn is_work(kind: Kind) -> bool {
    matches!(kind, Kind::Turn | Kind::Agent)
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Open {
        id: String,
        attrs: Attrs,
    },
    Close {
        id: String,
        outcome: Outcome,
    },
    /// A node ended and how isn't known yet: it closes with no outcome. For a
    /// turn that clears the session's `last_turn` until the outcome follows.
    End {
        id: String,
    },
    /// A node closes without having ended: it stood in for another node, or
    /// only what is beneath it is running. Nothing is recorded, so a turn's
    /// retraction leaves the session's `last_turn` as it was.
    Retract {
        id: String,
    },
    Attr {
        id: String,
        attrs: Attrs,
    },
    Lost {
        scope: String,
    },
    Sync {
        scope: String,
        nodes: Vec<(String, Attrs)>,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct Client {
    pub name: &'static str,
    pub observes: Observes,
}

/// Returns the kind of node a path names, or `None` if the path is malformed.
pub fn kind_of(id: &str) -> Option<Kind> {
    if id == "/" {
        return Some(Kind::Machine);
    }
    let rest = id.strip_prefix('/')?;
    let mut segments = rest.split('/');
    let client = segments.next()?;
    if client.is_empty() || client.contains(':') {
        return None;
    }
    let mut kind = Kind::Client;
    for segment in segments {
        let (prefix, name) = segment.split_once(':')?;
        let child = kind_from_prefix(prefix)?;
        if name.is_empty() || !allows_child(kind, child) {
            return None;
        }
        kind = child;
    }
    Some(kind)
}

fn parent_of(id: &str) -> Option<&str> {
    if id == "/" {
        return None;
    }
    match id.rfind('/') {
        Some(0) => Some("/"),
        Some(i) => Some(&id[..i]),
        None => None,
    }
}

fn depth_of(id: &str) -> usize {
    if id == "/" {
        0
    } else {
        id.matches('/').count()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Node {
    pub(crate) kind: Kind,
    pub(crate) open: bool,
    confirmed: bool,
    pub(crate) opened_at: Millis,
    last_confirmed: Millis,
    pub(crate) attrs: Attrs,
}

impl Node {
    /// A process node stands for activity no session has claimed yet, so no
    /// source can ever vouch for it.
    fn is_unconfirmed(&self) -> bool {
        self.kind == Kind::Process || !self.confirmed
    }

    fn can_go_stale(&self) -> bool {
        is_work(self.kind) || self.kind == Kind::Request
    }
}

pub struct Tree {
    pub(crate) nodes: BTreeMap<String, Node>,
    clients: Vec<Client>,
    stale_after: Millis,
    /// Each node's derived state and when it last changed.
    pub(crate) states: BTreeMap<String, (State, Millis)>,
    /// The rows last returned by `take_changes`.
    pub(crate) published: BTreeMap<String, crate::row::Row>,
}

impl Tree {
    pub fn new(clients: &[Client], stale_after: Millis, at: Millis) -> Tree {
        let mut nodes = BTreeMap::new();
        nodes.insert("/".to_string(), Node::new(Kind::Machine, at));
        for client in clients {
            let mut node = Node::new(Kind::Client, at);
            node.attrs.insert(
                "observes".into(),
                serde_json::to_value(client.observes).expect("observes serializes"),
            );
            nodes.insert(format!("/{}", client.name), node);
        }
        let mut tree = Tree {
            nodes,
            clients: clients.to_vec(),
            stale_after,
            states: BTreeMap::new(),
            published: BTreeMap::new(),
        };
        tree.refresh_states(at);
        tree
    }

    /// Records `at` as the `since` time of every node whose state changed.
    fn refresh_states(&mut self, at: Millis) {
        let current: BTreeMap<String, State> = self
            .nodes
            .keys()
            .filter_map(|id| Some((id.clone(), self.state(id)?)))
            .collect();
        self.states.retain(|id, _| current.contains_key(id));
        for (id, state) in current {
            if self.states.get(&id).map(|(known, _)| *known) != Some(state) {
                self.states.insert(id, (state, at));
            }
        }
    }

    /// Moves a node's `since` back to `at` when a source knows its current
    /// state began before Eywa saw it.
    pub fn backdate(&mut self, id: &str, state: State, at: Millis) {
        if let Some((known, since)) = self.states.get_mut(id)
            && *known == state
            && at < *since
        {
            *since = at;
        }
    }

    pub fn apply(&mut self, event: Event, at: Millis) {
        match event {
            Event::Open { id, attrs } => {
                self.open(&id, attrs, at);
                self.link_process(&id);
            }
            Event::Close { id, outcome } => {
                if let Some(parent) = parent_of(&id) {
                    self.confirm(parent, at);
                }
                self.close(&id, Some(outcome), at);
            }
            Event::End { id } => {
                if let Some(parent) = parent_of(&id) {
                    self.confirm(parent, at);
                }
                self.close(&id, None, at);
            }
            Event::Retract { id } => {
                if let Some(parent) = parent_of(&id) {
                    self.confirm(parent, at);
                }
                self.retract(&id);
            }
            Event::Attr { id, attrs } => {
                if let Some(node) = self.nodes.get_mut(&id) {
                    node.attrs.extend(attrs);
                    self.confirm(&id, at);
                    self.link_process(&id);
                }
            }
            Event::Lost { scope } => {
                let scope: Vec<String> =
                    self.subtree(&scope).map(|(path, _)| path.clone()).collect();
                for path in scope {
                    if let Some(node) = self.nodes.get_mut(&path)
                        && !matches!(node.kind, Kind::Machine | Kind::Client)
                    {
                        node.confirmed = false;
                    }
                }
            }
            Event::Sync { scope, nodes } => self.sync(&scope, nodes, at),
        }
        self.prune();
        self.refresh_states(at);
    }

    /// Unconfirms open turns, agents and requests that have heard nothing for
    /// `stale_after`. Elapsed time never closes anything.
    pub fn expire(&mut self, at: Millis) {
        let stale_after = self.stale_after;
        for node in self.nodes.values_mut() {
            if node.open
                && node.confirmed
                && node.can_go_stale()
                && node.last_confirmed + stale_after <= at
            {
                node.confirmed = false;
            }
        }
        self.refresh_states(at);
    }

    /// When the next open turn, agent or request goes stale, if any.
    pub fn next_deadline(&self) -> Option<Millis> {
        self.nodes
            .values()
            .filter(|node| node.open && node.confirmed && node.can_go_stale())
            .map(|node| node.last_confirmed + self.stale_after)
            .min()
    }

    /// Closes every session that ran in process `pid` and removes any process
    /// node for it. Returns the ids of the sessions it closed.
    pub fn process_exited(&mut self, pid: u32, at: Millis) -> Vec<String> {
        let sessions: Vec<String> = self
            .nodes
            .iter()
            .filter(|(_, node)| {
                node.kind == Kind::Session
                    && node.open
                    && node.attrs.get("pid") == Some(&pid.into())
            })
            .map(|(path, _)| path.clone())
            .collect();
        for session in &sessions {
            self.close(session, Some(Outcome::Ok), at);
        }
        for client in self.clients.clone() {
            self.close(
                &format!("/{}/process:{pid}", client.name),
                Some(Outcome::Ok),
                at,
            );
        }
        self.prune();
        self.refresh_states(at);
        sessions
    }

    /// Marks a node and all its ancestors as confirmed by a source at `at`.
    fn confirm(&mut self, id: &str, at: Millis) {
        let mut path = Some(id);
        while let Some(current) = path {
            if let Some(node) = self.nodes.get_mut(current) {
                node.confirmed = true;
                node.last_confirmed = at;
            }
            path = parent_of(current);
        }
    }

    /// A session that names its process claims that process's activity, so
    /// the placeholder process node goes away.
    fn link_process(&mut self, id: &str) {
        let Some(node) = self.nodes.get(id) else {
            return;
        };
        if node.kind != Kind::Session {
            return;
        }
        let Some(pid) = node.attrs.get("pid").and_then(|pid| pid.as_u64()) else {
            return;
        };
        let client = id[1..].split('/').next().unwrap_or("");
        self.nodes.remove(&format!("/{client}/process:{pid}"));
    }

    fn sync(&mut self, scope: &str, nodes: Vec<(String, Attrs)>, at: Millis) {
        if !matches!(kind_of(scope), Some(Kind::Machine | Kind::Client)) {
            self.open(scope, Attrs::new(), at);
        }
        if !self.nodes.contains_key(scope) {
            return;
        }
        let beneath: Vec<String> = self
            .descendants(scope)
            .map(|(path, _)| path.clone())
            .collect();
        for path in beneath {
            self.nodes.remove(&path);
        }
        let prefix = if scope == "/" {
            "/".to_string()
        } else {
            format!("{scope}/")
        };
        for (id, attrs) in nodes {
            if id.starts_with(&prefix) {
                self.open(&id, attrs, at);
                self.link_process(&id);
            }
        }
    }

    pub fn state(&self, id: &str) -> Option<State> {
        self.evaluate(id).map(|(state, _)| state)
    }

    pub fn cause(&self, id: &str) -> Option<String> {
        self.evaluate(id).and_then(|(_, cause)| cause)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.nodes.contains_key(id)
    }

    pub fn attrs(&self, id: &str) -> Option<&Attrs> {
        self.nodes.get(id).map(|node| &node.attrs)
    }

    fn is_known_client(&self, id: &str) -> bool {
        let name = id[1..].split('/').next().unwrap_or("");
        self.clients.iter().any(|client| client.name == name)
    }

    fn open(&mut self, id: &str, attrs: Attrs, at: Millis) {
        let Some(kind) = kind_of(id) else { return };
        if matches!(kind, Kind::Machine | Kind::Client) || !self.is_known_client(id) {
            return;
        }
        let mut missing = Vec::new();
        let mut ancestor = parent_of(id);
        while let Some(path) = ancestor {
            if self.nodes.contains_key(path) {
                break;
            }
            missing.push(path.to_string());
            ancestor = parent_of(path);
        }
        for path in missing.into_iter().rev() {
            let kind = kind_of(&path).expect("ancestor of a valid path is valid");
            self.nodes.insert(path, Node::new(kind, at));
        }
        let node = self
            .nodes
            .entry(id.to_string())
            .or_insert_with(|| Node::new(kind, at));
        node.open = true;
        node.attrs.extend(attrs);
        self.confirm(id, at);
    }

    /// Closes a node. A turn closing with an outcome records it on its
    /// session as `last_turn`; with none, the outcome is still to come and
    /// `last_turn` is cleared until it arrives.
    fn close(&mut self, id: &str, outcome: Option<Outcome>, at: Millis) {
        if self.retract(id) != Some(Kind::Turn) {
            return;
        }
        let Some(session) = parent_of(id).and_then(|path| self.nodes.get_mut(path)) else {
            return;
        };
        match outcome {
            Some(outcome) => {
                session
                    .attrs
                    .insert("last_turn".into(), last_turn(outcome, at));
            }
            // The turn before no longer says how the latest one ended.
            None => {
                session.attrs.shift_remove("last_turn");
            }
        }
    }

    /// Closes a node and records nothing about how it ended. Returns the
    /// node's kind if it was open.
    fn retract(&mut self, id: &str) -> Option<Kind> {
        let node = self.nodes.get(id)?;
        if !node.open || matches!(node.kind, Kind::Machine | Kind::Client) {
            return None;
        }
        let kind = node.kind;
        let prefix = format!("{id}/");
        for (path, node) in self.nodes.range_mut(prefix.clone()..) {
            if !path.starts_with(&prefix) {
                break;
            }
            let direct_request = node.kind == Kind::Request && parent_of(path) == Some(id);
            if kind == Kind::Session || direct_request {
                node.open = false;
            }
        }
        if let Some(node) = self.nodes.get_mut(id) {
            node.open = false;
        }
        Some(kind)
    }

    /// Removes closed nodes that have nothing open beneath them.
    fn prune(&mut self) {
        let removable: Vec<String> = self
            .nodes
            .iter()
            .filter(|(path, node)| {
                !node.open
                    && !matches!(node.kind, Kind::Machine | Kind::Client)
                    && !self.descendants(path).any(|(_, child)| child.open)
            })
            .map(|(path, _)| path.clone())
            .collect();
        for path in removable {
            self.nodes.remove(&path);
        }
    }

    pub(crate) fn descendants<'a>(
        &'a self,
        id: &str,
    ) -> impl Iterator<Item = (&'a String, &'a Node)> + 'a {
        let prefix = if id == "/" {
            "/".to_string()
        } else {
            format!("{id}/")
        };
        let root = id == "/";
        self.nodes
            .range(prefix.clone()..)
            .take_while(move |(path, _)| path.starts_with(&prefix))
            .filter(move |(path, _)| !(root && path.as_str() == "/"))
    }

    fn subtree<'a>(&'a self, id: &str) -> impl Iterator<Item = (&'a String, &'a Node)> + 'a {
        self.nodes
            .get_key_value(id)
            .into_iter()
            .chain(self.descendants(id))
    }

    /// The derived state of a node and the node responsible for it.
    pub(crate) fn evaluate(&self, id: &str) -> Option<(State, Option<String>)> {
        self.nodes.get(id)?;
        let mut best: [Option<(&String, &Node)>; 3] = [None, None, None];
        for (path, node) in self.subtree(id).filter(|(_, node)| node.open) {
            let category = match node.kind {
                _ if node.is_unconfirmed() => 2,
                Kind::Request => 0,
                kind if is_work(kind) => 1,
                _ => continue,
            };
            let slot = &mut best[category];
            if slot.is_none_or(|current| outranks(path, node, current)) {
                *slot = Some((path, node));
            }
        }
        let states = [State::NeedsInput, State::Working, State::Unknown];
        for (state, candidate) in states.into_iter().zip(best) {
            if let Some((path, _)) = candidate {
                return Some((state, Some(path.clone())));
            }
        }
        Some((State::Idle, None))
    }
}

/// Whether `(path, node)` is a better cause than `current`: deeper first,
/// then opened earlier, then the lower path.
fn outranks(path: &str, node: &Node, current: (&String, &Node)) -> bool {
    let (current_path, current_node) = current;
    (
        depth_of(path),
        std::cmp::Reverse(node.opened_at),
        std::cmp::Reverse(path),
    ) > (
        depth_of(current_path),
        std::cmp::Reverse(current_node.opened_at),
        std::cmp::Reverse(current_path.as_str()),
    )
}

impl Node {
    fn new(kind: Kind, at: Millis) -> Node {
        Node {
            kind,
            open: true,
            confirmed: true,
            opened_at: at,
            last_confirmed: at,
            attrs: Attrs::new(),
        }
    }
}

/// A session's `last_turn` attribute: how its most recent turn ended, and when.
pub(crate) fn last_turn(outcome: Outcome, ended_at: Millis) -> serde_json::Value {
    serde_json::json!({"outcome": outcome, "ended_at": rfc3339(ended_at)})
}

/// Formats a time as RFC 3339 with milliseconds, the form used on the wire.
pub(crate) fn rfc3339(at: Millis) -> String {
    let time = std::time::UNIX_EPOCH + std::time::Duration::from_millis(at);
    humantime::format_rfc3339_millis(time).to_string()
}
