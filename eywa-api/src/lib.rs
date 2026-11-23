//! Eywa's local API, version 1: the requests a program sends to Eywa's
//! `api.sock`, and the messages and rows it reads back. Eywa's server writes
//! these same types, so decoding with them reads exactly what Eywa publishes.
//! See `docs/api.md` for what every field means.
//!
//! Version 1 only ever adds things. Every enum has an `Other` variant that
//! stands in for a value a newer Eywa may send, and `Message::Other` stands
//! in for a message type this version doesn't know. Neither keeps what was
//! sent, so they encode as `"other"`. `Attrs::other` does keep the attributes
//! this version doesn't name. Eywa itself never produces `Other`.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::ffi::OsString;
use std::path::PathBuf;

/// The API socket's file name inside Eywa's directory.
pub const API_SOCKET: &str = "api.sock";

/// Eywa's directory: `$EYWA_DIR`, or `~/Library/Application Support/eywa`.
pub fn default_dir() -> PathBuf {
    dir_from(std::env::var_os("EYWA_DIR"), std::env::var_os("HOME"))
}

fn dir_from(eywa_dir: Option<OsString>, home: Option<OsString>) -> PathBuf {
    match eywa_dir {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(home.unwrap_or_default()).join("Library/Application Support/eywa"),
    }
}

macro_rules! wire_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($(#[$vmeta])* $variant,)+
            /// Stands in for a value from a newer Eywa that this version
            /// doesn't know. The value itself isn't kept.
            #[serde(other)]
            Other,
        }
    };
}

wire_enum!(
    /// What a node is.
    Kind { Machine, Client, Process, Session, Turn, Agent, Request }
);
wire_enum!(
    /// A node's state, derived from everything beneath it.
    State {
        /// An open request exists beneath the node.
        NeedsInput,
        /// An open turn or subagent exists beneath it.
        Working,
        /// Something is open that Eywa can't confirm.
        Unknown,
        /// None of the above.
        Idle,
    }
);
wire_enum!(
    /// Why a request is waiting on the user.
    Reason { Approval, Question, McpInput }
);
wire_enum!(
    /// How a turn ended.
    Outcome { Ok, Error, Cancelled }
);
wire_enum!(
    /// Who sent a turn's prompt. Claude Code only.
    Origin { Person, Machine }
);
wire_enum!(
    /// Whom a client attributes an approval request to.
    Approvals {
        /// The subagent or main thread that asked.
        Actor,
        /// Only the turn.
        Turn,
        /// Approvals aren't visible.
        None,
    }
);
wire_enum!(
    /// How Eywa learns that a turn was interrupted.
    Interrupts {
        /// The client reports it.
        Hook,
        /// Eywa infers it from what happens next.
        Inferred,
    }
);
wire_enum!(
    /// What a request asks Eywa to do.
    Op { Get, Subscribe, Stop }
);
wire_enum!(
    /// Which rows a request selects.
    View { Tree, Requests }
);

/// One request, sent as a single JSON line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    pub op: Op,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<View>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u32>,
}

impl Request {
    /// A snapshot of the tree at and below `path`, `depth` levels deep, then
    /// every change.
    pub fn subscribe(path: &str, depth: u32) -> Request {
        Request {
            v: 1,
            op: Op::Subscribe,
            view: None,
            path: Some(path.to_string()),
            depth: Some(depth),
        }
    }

    /// Every open request across all clients, then every change.
    pub fn requests() -> Request {
        Request {
            v: 1,
            op: Op::Subscribe,
            view: Some(View::Requests),
            path: None,
            depth: None,
        }
    }
}

/// One message from Eywa. On the wire every message also carries `"v": 1`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// A message lives only as long as it takes to read or write one line.
#[allow(clippy::large_enum_variant)]
pub enum Message {
    /// Every row in the selection, sorted by id. Always first.
    Snapshot { rev: u64, rows: Vec<Row> },
    /// A row appeared or changed: replace the whole row.
    Upsert { rev: u64, row: Row },
    /// A row disappeared.
    Remove { rev: u64, id: String },
    /// The request was rejected; the connection then closes.
    Error { code: String, message: String },
    /// The answer to `stop`.
    Stopping,
    /// A message type from a newer Eywa.
    #[serde(other)]
    Other,
}

impl Serialize for Message {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Wire<'a> {
            v: u32,
            #[serde(flatten)]
            body: Body<'a>,
        }
        #[derive(Serialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Body<'a> {
            Snapshot { rev: u64, rows: &'a [Row] },
            Upsert { rev: u64, row: &'a Row },
            Remove { rev: u64, id: &'a str },
            Error { code: &'a str, message: &'a str },
            Stopping,
            Other,
        }
        let body = match self {
            Message::Snapshot { rev, rows } => Body::Snapshot { rev: *rev, rows },
            Message::Upsert { rev, row } => Body::Upsert { rev: *rev, row },
            Message::Remove { rev, id } => Body::Remove { rev: *rev, id },
            Message::Error { code, message } => Body::Error { code, message },
            Message::Stopping => Body::Stopping,
            Message::Other => Body::Other,
        };
        Wire { v: 1, body }.serialize(serializer)
    }
}

/// The published view of one node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Row {
    /// The node's path, such as `/claude/session:7f3a`. Treat it as opaque.
    pub id: String,
    pub kind: Kind,
    pub state: State,
    /// When `state` last changed: RFC 3339, UTC, with milliseconds.
    pub since: String,
    /// Why the user is needed. Only when `state` is `NeedsInput`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
    /// The node responsible for the state. Every state but `Idle`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    /// Open sessions beneath. Machine and client rows only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counts: Option<Counts>,
    #[serde(default)]
    pub attrs: Attrs,
}

/// Open sessions beneath a machine or client row, in total and by state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub sessions: u32,
    pub needs_input: u32,
    pub working: u32,
    pub unknown: u32,
    pub idle: u32,
}

macro_rules! attrs {
    ($($(#[$meta:meta])* $name:ident: $type:ty,)+) => {
        /// Facts about a node. Each is present only when the agent states it.
        ///
        /// Decoding never fails on an attribute's value: one this version
        /// can't hold, such as a string `pid`, stays in `other` under its own
        /// name.
        #[derive(Clone, Debug, Default, PartialEq, Serialize)]
        pub struct Attrs {
            $(
                $(#[$meta])*
                #[serde(skip_serializing_if = "Option::is_none")]
                pub $name: Option<$type>,
            )+
            /// Attributes this version doesn't name, or whose value it
            /// can't hold.
            #[serde(flatten)]
            pub other: Map<String, Value>,
        }

        impl Attrs {
            fn read(map: &Map<String, Value>, read: Read) -> Attrs {
                let mut attrs = Attrs::default();
                for (key, value) in map {
                    let held = match key.as_str() {
                        $(stringify!($name) => {
                            attrs.$name = match read {
                                Read::Decode => Field::decode(value),
                                Read::Exact => Field::exact(value),
                            };
                            attrs.$name.is_some()
                        })+
                        _ => false,
                    };
                    if !held {
                        attrs.other.insert(key.clone(), value.clone());
                    }
                }
                attrs
            }
        }
    };
}

// The one list of documented attributes: `Attrs`' fields, and what reading a
// map looks for.
attrs! {
    /// Client: what the client lets Eywa see.
    observes: Observes,
    /// Session: the working directory.
    cwd: String,
    /// Session: the agent process.
    pid: u32,
    /// Session: usually `cli`, `desktop` or `ide`.
    surface: String,
    /// Session: the model, as the agent names it.
    model: String,
    /// Session: how the most recent turn ended.
    last_turn: LastTurn,
    /// Turn: who sent the prompt. Claude Code only.
    origin: Origin,
    /// Agent: such as `Explore`, or the name of an agent you defined.
    agent_type: String,
    /// Request: why it waits on the user. Always present on requests.
    reason: Reason,
    /// Request: the agent confirmed it is showing the request.
    prompted: bool,
    /// Request: the tool asking for approval, or asking the question.
    tool_name: String,
    /// Request: the MCP server, on `mcp_input` requests.
    mcp_server: String,
}

impl Attrs {
    /// The attributes in `map`, for a program that writes the API, as Eywa
    /// does. A value becomes a field only if the field writes it back
    /// unchanged; anything else stays in `other`, including an enum value
    /// this version doesn't know. So the result always encodes to `map`, and
    /// never holds an `Other`.
    pub fn exactly(map: &Map<String, Value>) -> Attrs {
        Attrs::read(map, Read::Exact)
    }
}

impl<'de> Deserialize<'de> for Attrs {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Attrs::read(&Map::deserialize(deserializer)?, Read::Decode))
    }
}

/// How strictly a map's values become fields.
#[derive(Clone, Copy)]
enum Read {
    /// Whatever the field's type can hold. An unknown enum value is `Other`.
    Decode,
    /// Only what the field's type writes back unchanged.
    Exact,
}

/// How an attribute's type reads a JSON value.
trait Field: Serialize + DeserializeOwned {
    fn decode(value: &Value) -> Option<Self> {
        Self::deserialize(value).ok()
    }

    fn exact(value: &Value) -> Option<Self> {
        let decoded = Self::decode(value)?;
        (serde_json::to_value(&decoded).ok()? == *value).then_some(decoded)
    }
}

impl Field for Observes {}
impl Field for LastTurn {}
impl Field for Origin {}
impl Field for Reason {}

/// Decoding these loses nothing, so there is nothing to check.
macro_rules! lossless {
    ($($type:ty),+) => {
        $(impl Field for $type {
            fn exact(value: &Value) -> Option<Self> {
                Self::decode(value)
            }
        })+
    };
}
lossless!(String, u32, bool);

/// What a client lets Eywa see.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observes {
    pub agents: bool,
    pub approvals: Approvals,
    pub questions: bool,
    pub mcp_input: bool,
    pub interrupts: Interrupts,
}

/// How and when a session's most recent turn ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastTurn {
    pub outcome: Outcome,
    /// RFC 3339, UTC, with milliseconds.
    pub ended_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn same(a: impl Serialize, b: Value) {
        assert_eq!(serde_json::to_value(a).unwrap(), b);
    }

    #[test]
    fn serializes_messages_with_v_and_type() {
        same(
            Message::Remove {
                rev: 3,
                id: "/claude".into(),
            },
            json!({"v": 1, "type": "remove", "rev": 3, "id": "/claude"}),
        );
        same(Message::Stopping, json!({"v": 1, "type": "stopping"}));
    }

    #[test]
    fn decodes_a_documented_row() {
        let line = json!({
            "v": 1, "type": "upsert", "rev": 44,
            "row": {
                "id": "/claude/session:7f3a/turn:12/agent:a2/request:t9",
                "kind": "request",
                "state": "needs_input",
                "since": "2026-11-18T20:41:07.000Z",
                "reason": "approval",
                "cause": "/claude/session:7f3a/turn:12/agent:a2/request:t9",
                "attrs": { "reason": "approval", "prompted": true, "tool_name": "Bash" }
            }
        });
        let message: Message = serde_json::from_value(line.clone()).unwrap();
        let Message::Upsert { rev: 44, ref row } = message else {
            panic!("not an upsert: {message:?}");
        };
        assert_eq!(row.kind, Kind::Request);
        assert_eq!(row.state, State::NeedsInput);
        assert_eq!(row.reason, Some(Reason::Approval));
        assert_eq!(row.attrs.tool_name.as_deref(), Some("Bash"));
        assert_eq!(row.attrs.prompted, Some(true));
        same(&message, line);
    }

    #[test]
    fn decodes_what_a_newer_eywa_may_send() {
        let other: Message = serde_json::from_str(r#"{"v":1,"type":"heartbeat"}"#).unwrap();
        assert_eq!(other, Message::Other);
        let row: Row = serde_json::from_value(json!({
            "id": "/robot", "kind": "robot", "state": "sleeping", "since": "2026-11-18T20:41:07.000Z",
            "reason": "vibes", "glow": 1,
            "attrs": {
                "colour": "red",
                "observes": {
                    "agents": true, "approvals": "someone", "questions": true,
                    "mcp_input": false, "interrupts": "psychic"
                }
            }
        }))
        .unwrap();
        assert_eq!(row.kind, Kind::Other);
        assert_eq!(row.state, State::Other);
        assert_eq!(row.reason, Some(Reason::Other));
        assert_eq!(row.attrs.other["colour"], "red");
        let observes = row.attrs.observes.unwrap();
        assert_eq!(observes.approvals, Approvals::Other);
        assert_eq!(observes.interrupts, Interrupts::Other);
    }

    /// A documented attribute holding a type this version doesn't expect
    /// doesn't cost the row: it stays in `other`, and encodes back unchanged.
    #[test]
    fn attrs_with_unexpected_types_still_decode() {
        let line = json!({
            "v": 1, "type": "upsert", "rev": 7,
            "row": {
                "id": "/claude/session:s1", "kind": "session", "state": "idle",
                "since": "2026-11-18T20:41:07.000Z",
                "attrs": {
                    "cwd": "/tmp", "pid": "not a number", "last_turn": 5,
                    "surface": null, "colour": "red"
                }
            }
        });
        let message: Message = serde_json::from_value(line.clone()).unwrap();
        let Message::Upsert { ref row, .. } = message else {
            panic!("not an upsert: {message:?}");
        };
        assert_eq!(row.attrs.cwd.as_deref(), Some("/tmp"));
        assert_eq!(row.attrs.pid, None);
        assert_eq!(row.attrs.last_turn, None);
        assert_eq!(row.attrs.surface, None);
        assert_eq!(row.attrs.other["pid"], "not a number");
        assert_eq!(row.attrs.other["last_turn"], 5);
        assert_eq!(row.attrs.other["surface"], Value::Null);
        assert_eq!(row.attrs.other["colour"], "red");
        same(&message, line);
    }

    #[test]
    fn subscribe_request_matches_the_docs() {
        same(
            Request::subscribe("/", 1),
            json!({"v": 1, "op": "subscribe", "path": "/", "depth": 1}),
        );
        same(
            Request::requests(),
            json!({"v": 1, "op": "subscribe", "view": "requests"}),
        );
    }

    #[test]
    fn the_directory_is_eywa_dir_or_under_home() {
        let home = Some(OsString::from("/Users/me"));
        assert_eq!(
            dir_from(Some("/tmp/eywa".into()), home.clone()),
            PathBuf::from("/tmp/eywa")
        );
        assert_eq!(
            dir_from(None, home),
            PathBuf::from("/Users/me/Library/Application Support/eywa")
        );
    }

    /// What a writer of the API builds from a map always encodes back to
    /// that map, and never holds an `Other`.
    #[test]
    fn exact_attrs_encode_back_to_their_map() {
        let map = json!({
            "cwd": "/tmp", "pid": 7, "prompted": true, "reason": "vibes",
            "origin": "person", "surface": 3, "colour": "red",
            "last_turn": { "outcome": "ok", "ended_at": "2026-11-18T20:41:07.000Z", "extra": 1 },
            "observes": {
                "agents": true, "approvals": "actor", "questions": true,
                "mcp_input": false, "interrupts": "hook"
            }
        });
        let attrs = Attrs::exactly(map.as_object().unwrap());
        assert_eq!(attrs.cwd.as_deref(), Some("/tmp"));
        assert_eq!(attrs.pid, Some(7));
        assert_eq!(attrs.prompted, Some(true));
        assert_eq!(attrs.origin, Some(Origin::Person));
        assert_eq!(attrs.observes.unwrap().approvals, Approvals::Actor);
        assert_eq!(attrs.reason, None, "an unknown reason isn't Other");
        assert_eq!(attrs.other["reason"], "vibes");
        assert_eq!(attrs.surface, None);
        assert_eq!(attrs.last_turn, None, "a field would be dropped");
        same(&attrs, map.clone());
        // Decoding the same map keeps what it can, as `Other` where it must.
        let decoded: Attrs = serde_json::from_value(map).unwrap();
        assert_eq!(decoded.reason, Some(Reason::Other));
        assert_eq!(decoded.last_turn.unwrap().outcome, Outcome::Ok);
    }
}
