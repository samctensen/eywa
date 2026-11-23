//! The local API: requests, selections, wire messages and subscriber queues.

use crate::row::{Change, Row};
use crate::tree::{Kind, kind_of};
use serde_json::{Value, json};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};

pub use eywa_api::Op;

#[derive(Clone, Debug, PartialEq)]
pub enum Selection {
    /// Nodes at or below `path`, down to `depth` levels beneath it.
    Tree { path: String, depth: u32 },
    /// Every open request, across all clients.
    Requests,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApiRequest {
    pub op: Op,
    pub selection: Selection,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApiError {
    pub code: &'static str,
    pub message: String,
}

fn bad_request(message: &str) -> ApiError {
    ApiError {
        code: "bad_request",
        message: message.to_string(),
    }
}

pub fn parse_request(line: &str) -> Result<ApiRequest, ApiError> {
    let value: Value =
        serde_json::from_str(line).map_err(|_| bad_request("request is not JSON"))?;
    let object = value
        .as_object()
        .ok_or_else(|| bad_request("request is not an object"))?;
    if object.get("v") != Some(&json!(1)) {
        return Err(ApiError {
            code: "unsupported_version",
            message: "only version 1 is supported".to_string(),
        });
    }
    let op = match object.get("op").and_then(Value::as_str) {
        Some("get") => Op::Get,
        Some("subscribe") => Op::Subscribe,
        Some("stop") => Op::Stop,
        _ => return Err(bad_request("op must be get, subscribe or stop")),
    };
    let selection = match object.get("view").map(Value::as_str) {
        None | Some(Some("tree")) => {
            let path = match object.get("path") {
                None => "/".to_string(),
                Some(path) => path
                    .as_str()
                    .filter(|path| kind_of(path).is_some())
                    .ok_or_else(|| bad_request("path must be a node path such as /claude"))?
                    .to_string(),
            };
            let depth = match object.get("depth") {
                None => 1,
                Some(depth) => depth
                    .as_u64()
                    .and_then(|depth| u32::try_from(depth).ok())
                    .ok_or_else(|| bad_request("depth must be a non-negative integer"))?,
            };
            Selection::Tree { path, depth }
        }
        Some(Some("requests")) => Selection::Requests,
        Some(_) => return Err(bad_request("unknown view")),
    };
    Ok(ApiRequest { op, selection })
}

fn depth_of(id: &str) -> usize {
    if id == "/" {
        0
    } else {
        id.matches('/').count()
    }
}

impl Selection {
    pub fn includes(&self, id: &str) -> bool {
        match self {
            Selection::Requests => kind_of(id) == Some(Kind::Request),
            Selection::Tree { path, depth } => {
                let beneath = path == "/" || id == path || id.starts_with(&format!("{path}/"));
                beneath && depth_of(id) - depth_of(path) <= *depth as usize
            }
        }
    }
}

pub fn snapshot_line(rev: u64, rows: &[Row]) -> String {
    json!({"v": 1, "type": "snapshot", "rev": rev, "rows": rows}).to_string()
}

pub fn change_line(rev: u64, change: &Change) -> String {
    match change {
        Change::Upsert(row) => json!({"v": 1, "type": "upsert", "rev": rev, "row": row}),
        Change::Remove(id) => json!({"v": 1, "type": "remove", "rev": rev, "id": id}),
    }
    .to_string()
}

pub fn stopping_line() -> String {
    json!({"v": 1, "type": "stopping"}).to_string()
}

pub fn error_line(error: &ApiError) -> String {
    json!({"v": 1, "type": "error", "code": error.code, "message": error.message}).to_string()
}

/// Subscribers and their bounded queues. A subscriber that falls behind is
/// dropped, which ends its connection; it resubscribes for a fresh snapshot.
pub struct Subscribers {
    queue: usize,
    list: Vec<(Selection, SyncSender<String>)>,
}

impl Subscribers {
    pub fn new(queue: usize) -> Subscribers {
        Subscribers {
            queue: queue.max(1),
            list: Vec::new(),
        }
    }

    /// Registers a subscriber, queueing its snapshot of `rows` at `rev`.
    pub fn add(&mut self, selection: Selection, rev: u64, rows: &[Row]) -> Receiver<String> {
        let (sender, receiver) = sync_channel(self.queue);
        let selected: Vec<Row> = rows
            .iter()
            .filter(|row| selection.includes(&row.id))
            .cloned()
            .collect();
        if sender.try_send(snapshot_line(rev, &selected)).is_ok() {
            self.list.push((selection, sender));
        }
        receiver
    }

    pub fn publish(&mut self, changes: &[(u64, Change)]) {
        self.list.retain(|(selection, sender)| {
            changes
                .iter()
                .filter(|(_, change)| selection.includes(change.id()))
                .all(|(rev, change)| sender.try_send(change_line(*rev, change)).is_ok())
        });
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
}
