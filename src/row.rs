//! Rows: the published view of each node, and the changes between views.

use crate::tree::{Attrs, Kind, Millis, State, Tree, rfc3339};
use serde::{Serialize, Serializer};
use std::collections::BTreeMap;

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Row {
    pub id: String,
    pub kind: Kind,
    pub state: State,
    #[serde(serialize_with = "serialize_time")]
    pub since: Millis,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counts: Option<Counts>,
    pub attrs: Attrs,
}

/// Open sessions beneath a machine or client row, in total and by state.
#[derive(Serialize, Clone, Debug, PartialEq, Default)]
pub struct Counts {
    pub sessions: u32,
    pub needs_input: u32,
    pub working: u32,
    pub unknown: u32,
    pub idle: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    Upsert(Row),
    Remove(String),
}

impl Change {
    pub fn id(&self) -> &str {
        match self {
            Change::Upsert(row) => &row.id,
            Change::Remove(id) => id,
        }
    }
}

fn serialize_time<S: Serializer>(at: &Millis, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&rfc3339(*at))
}

impl Tree {
    /// Every node's row, sorted by id.
    pub fn rows(&self) -> Vec<Row> {
        self.nodes.keys().filter_map(|id| self.row(id)).collect()
    }

    /// The rows that changed or disappeared since the last call, sorted by id.
    pub fn take_changes(&mut self) -> Vec<Change> {
        let current: BTreeMap<String, Row> = self
            .rows()
            .into_iter()
            .map(|row| (row.id.clone(), row))
            .collect();
        let mut changes: Vec<Change> = self
            .published
            .keys()
            .filter(|id| !current.contains_key(*id))
            .map(|id| Change::Remove(id.clone()))
            .collect();
        changes.extend(
            current
                .values()
                .filter(|row| self.published.get(&row.id) != Some(row))
                .map(|row| Change::Upsert(row.clone())),
        );
        changes.sort_by(|a, b| a.id().cmp(b.id()));
        self.published = current;
        changes
    }

    fn row(&self, id: &str) -> Option<Row> {
        let node = self.nodes.get(id)?;
        let (state, cause) = self.evaluate(id)?;
        let since = self
            .states
            .get(id)
            .map_or(node.opened_at, |(_, since)| *since);
        let reason = match (state, &cause) {
            (State::NeedsInput, Some(cause)) => self
                .nodes
                .get(cause)
                .and_then(|node| node.attrs.get("reason"))
                .and_then(|reason| reason.as_str())
                .map(String::from),
            _ => None,
        };
        let counts = matches!(node.kind, Kind::Machine | Kind::Client).then(|| self.counts(id));
        Some(Row {
            id: id.to_string(),
            kind: node.kind,
            state,
            since,
            reason,
            cause,
            counts,
            attrs: node.attrs.clone(),
        })
    }

    fn counts(&self, id: &str) -> Counts {
        let mut counts = Counts::default();
        for (path, _) in self
            .descendants(id)
            .filter(|(_, node)| node.kind == Kind::Session && node.open)
        {
            counts.sessions += 1;
            match self.evaluate(path).map(|(state, _)| state) {
                Some(State::NeedsInput) => counts.needs_input += 1,
                Some(State::Working) => counts.working += 1,
                Some(State::Unknown) => counts.unknown += 1,
                Some(State::Idle | State::Other) | None => counts.idle += 1,
            }
        }
        counts
    }
}
