//! Rows: the published view of each node, and the changes between views.

use crate::tree::{Kind, State, Tree, rfc3339};
use eywa_api::Reason;
use serde::Deserialize;
use std::collections::BTreeMap;

pub use eywa_api::{Counts, Row};

#[derive(Clone, Debug, PartialEq)]
// A change lives only until it is published.
#[allow(clippy::large_enum_variant)]
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
                .and_then(|reason| Reason::deserialize(reason).ok())
                // An unknown reason stays in the request's attributes.
                .filter(|reason| *reason != Reason::Other),
            _ => None,
        };
        let counts = matches!(node.kind, Kind::Machine | Kind::Client).then(|| self.counts(id));
        Some(Row {
            id: id.to_string(),
            kind: node.kind,
            state,
            since: rfc3339(since),
            reason,
            cause,
            counts,
            attrs: eywa_api::Attrs::exactly(&node.attrs),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Approvals, Client, Event, Interrupts, Observes, STALE_AFTER};
    use serde_json::json;

    #[test]
    fn attrs_with_unexpected_types_keep_the_row() {
        let client = Client {
            name: "claude",
            observes: Observes {
                agents: true,
                approvals: Approvals::Actor,
                questions: true,
                mcp_input: true,
                interrupts: Interrupts::Inferred,
            },
        };
        let mut tree = Tree::new(&[client], STALE_AFTER, 0);
        let session = "/claude/session:s1";
        let attrs = json!({"pid": "not a number", "cwd": "/tmp"});
        tree.apply(
            Event::Open {
                id: session.into(),
                attrs: attrs.as_object().unwrap().clone(),
            },
            1,
        );
        let row = tree
            .rows()
            .into_iter()
            .find(|row| row.id == session)
            .expect("the row is published");
        assert_eq!(row.attrs.cwd.as_deref(), Some("/tmp"));
        assert_eq!(row.attrs.pid, None);
        assert_eq!(row.attrs.other["pid"], "not a number");
        assert_eq!(serde_json::to_value(&row.attrs).unwrap(), attrs);
        // A consumer reads back the same attributes the server published.
        let decoded: eywa_api::Attrs = serde_json::from_value(attrs).unwrap();
        assert_eq!(decoded, row.attrs);
    }
}
