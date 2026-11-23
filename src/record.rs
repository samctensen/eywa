//! Hook records: the allowed fields of a client's hook payload and hook
//! environment, in one shape for every client.
//!
//! Prompts, responses, commands and tool arguments never reach a record.
//! Tool input is reduced to a digest so an approval can be matched with the
//! tool call it belongs to.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub const MAX_ID_BYTES: usize = 128;

pub const CLAUDE_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PermissionDenied",
    "PostToolUse",
    "PostToolUseFailure",
    "PostToolBatch",
    "Elicitation",
    "ElicitationResult",
    "Notification",
    "SubagentStart",
    "SubagentStop",
    "TeammateIdle",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

pub const CODEX_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "SubagentStart",
    "SubagentStop",
    "Stop",
    "Interrupt",
    "SessionEnd",
];

pub const GROK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionDenied",
    "Notification",
    "SubagentStart",
    "SubagentStop",
    "Stop",
    "StopFailure",
    "StopCancelled",
    "SessionEnd",
];

pub const MUSE_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "Notification",
    "ToolUseStart",
    "PostToolUse",
    "PostToolUseFailure",
    "SubagentStart",
    "SubagentStop",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

/// The `reason` values that leave the hook: the two Grok's `Stop` sends at
/// session end, which its adapter acts on. Elsewhere `reason` can be free
/// text, such as why Claude Code's auto mode denied a tool call.
pub const KEPT_REASONS: &[&str] = &["channel_closed", "shutdown"];

/// The hook events a client reports, or `None` for a client Eywa doesn't
/// support.
pub fn events_of(client: &str) -> Option<&'static [&'static str]> {
    Some(match client {
        "claude" => CLAUDE_EVENTS,
        "codex" => CODEX_EVENTS,
        "grok" => GROK_EVENTS,
        "muse" => MUSE_EVENTS,
        _ => return None,
    })
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct HookRecord {
    pub client: String,
    pub event: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// The calls a batch event reports as resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elicitation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_tasks: Option<Vec<BackgroundTask>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// When the client says the event happened, used to drop a copy that
    /// arrives through a second hook registration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BackgroundTask {
    pub id: String,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum RecordError {
    NotAnObject,
    UnknownClient,
    UnknownEvent(String),
    MissingSessionId,
    InvalidId(&'static str),
}

/// Reads one variable from the hook's environment. Tests pass their own.
pub type Environment<'a> = &'a dyn Fn(&str) -> Option<String>;

impl HookRecord {
    /// Builds a record from a payload alone, as if the environment were empty.
    pub fn from_payload(client: &str, payload: &Value) -> Result<HookRecord, RecordError> {
        HookRecord::from_hook(client, payload, &|_| None)
    }

    /// Builds a record from a hook payload and the hook's environment,
    /// keeping only the allowed fields.
    pub fn from_hook(
        client: &str,
        payload: &Value,
        env: Environment,
    ) -> Result<HookRecord, RecordError> {
        let object = payload.as_object().ok_or(RecordError::NotAnObject)?;
        let events = events_of(client).ok_or(RecordError::UnknownClient)?;
        let event = event_name(client, object);
        if !events.contains(&event.as_str()) {
            return Err(RecordError::UnknownEvent(event));
        }
        let turn_keys: &[&str] = match client {
            "claude" => &["prompt_id"],
            "grok" => &["promptId", "prompt_id"],
            _ => &["turn_id"],
        };
        let tool_name = text(object, &["tool_name", "toolName"]);
        let tool_input = get(object, &["tool_input", "toolInput"]);
        let digest = match (&tool_name, tool_input) {
            (Some(name), Some(input)) => Some(tool_digest(name, &digest_input(client, input))),
            _ => None,
        };
        // StopFailure's `error` is a fixed code; elsewhere the same field can
        // hold tool output, which must not leave the hook.
        let error = (event == "StopFailure")
            .then(|| text(object, &["error"]))
            .flatten();
        let mut record = HookRecord {
            client: client.to_string(),
            session_id: id(object, "session_id", &["session_id", "sessionId"])?
                .ok_or(RecordError::MissingSessionId)?,
            turn_id: id(object, "turn_id", turn_keys)?,
            agent_id: id(object, "agent_id", &["agent_id"])?,
            agent_type: text(object, &["agent_type", "subagentType"]),
            tool_use_id: id(object, "tool_use_id", &["tool_use_id", "toolUseId"])?,
            tool_use_ids: tool_calls(object)?,
            tool_name,
            notification_type: text(object, &["notification_type", "notificationType"]),
            source: text(object, &["source"]),
            reason: text(object, &["reason"])
                .filter(|reason| KEPT_REASONS.contains(&reason.as_str())),
            elicitation_id: id(object, "elicitation_id", &["elicitation_id"])?,
            mcp_server: text(object, &["mcp_server_name"]),
            error,
            background_tasks: background_tasks(object),
            cwd: text(object, &["cwd"]),
            transcript_path: text(object, &["transcript_path", "transcriptPath"]),
            digest,
            timestamp: get(object, &["timestamp"]).and_then(scalar),
            pid: None,
            surface: None,
            event,
        };
        match client {
            "claude" => {
                record.pid = env("CLAUDE_PID").and_then(|pid| pid.parse().ok());
                record.surface = env("CLAUDE_CODE_ENTRYPOINT").map(|entry| claude_surface(&entry));
            }
            "grok" if is_subagent_event(&record.event) => {
                record.agent_id = id(object, "agent_id", &["subagentId"])?;
            }
            "muse" => apply_muse_threads(&mut record, object, env)?,
            _ => {}
        }
        Ok(record)
    }

    /// Whether every id that can become part of a node path is valid. The
    /// service checks this on each datagram, since anything running as the
    /// user can write to the ingest socket.
    pub fn has_valid_ids(&self) -> bool {
        is_valid_id(&self.session_id)
            && [
                &self.turn_id,
                &self.agent_id,
                &self.tool_use_id,
                &self.elicitation_id,
                &self.digest,
            ]
            .into_iter()
            .flatten()
            .chain(self.tool_use_ids.iter().flatten())
            .all(|id| is_valid_id(id))
    }
}

/// Grok sends its own snake_case `hookEventName` and, on every payload, the
/// matching Claude-style `hook_event_name`.
fn event_name(client: &str, object: &Map<String, Value>) -> String {
    if let Some(name) = text(object, &["hook_event_name"]) {
        return name;
    }
    match (client, text(object, &["hookEventName"])) {
        ("grok", Some(snake)) => snake
            .split('_')
            .map(|word| {
                let mut letters = word.chars();
                letters.next().map_or_else(String::new, |first| {
                    first.to_uppercase().chain(letters).collect()
                })
            })
            .collect(),
        _ => String::new(),
    }
}

fn is_subagent_event(event: &str) -> bool {
    matches!(event, "SubagentStart" | "SubagentStop")
}

/// Muse names threads in the environment: `MUSE_SESSION_ID` is the root
/// session and `MUSE_THREAD_ID` the thread raising the event. Subagent
/// lifecycle events name the child in the payload instead.
fn apply_muse_threads(
    record: &mut HookRecord,
    object: &Map<String, Value>,
    env: Environment,
) -> Result<(), RecordError> {
    let valid = |value: Option<String>| value.filter(|value| is_valid_id(value));
    let root = valid(env("MUSE_SESSION_ID")).unwrap_or_else(|| record.session_id.clone());
    let thread = valid(env("MUSE_THREAD_ID")).unwrap_or_else(|| root.clone());
    record.agent_id = if is_subagent_event(&record.event) {
        id(object, "agent_id", &["child_session_id", "subagent_id"])?
    } else {
        (thread != root).then_some(thread)
    };
    record.session_id = root;
    Ok(())
}

/// Claude Code's `CLAUDE_CODE_ENTRYPOINT`, as Eywa's surface vocabulary.
pub(crate) fn claude_surface(entrypoint: &str) -> String {
    let surface = if entrypoint.contains("desktop") {
        "desktop"
    } else if ["vscode", "jetbrains", "ide"]
        .iter()
        .any(|ide| entrypoint.contains(ide))
    {
        "ide"
    } else if entrypoint == "cli" {
        "cli"
    } else {
        entrypoint
    };
    surface.to_string()
}

/// The tool input as hashed. Codex adds the model's justification to a
/// shell command's input on `PermissionRequest` only, so it is left out.
fn digest_input(client: &str, input: &Value) -> Value {
    match (client, input) {
        ("codex", Value::Object(map)) => {
            let mut map = map.clone();
            map.remove("description");
            Value::Object(map)
        }
        _ => input.clone(),
    }
}

/// SHA-256 over the tool name, a newline and the tool input serialized with
/// sorted keys. Returns the first 16 bytes as lowercase hex.
pub fn tool_digest(tool_name: &str, tool_input: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(tool_name.as_bytes());
    hasher.update(b"\n");
    hasher.update(canonical_json(tool_input).as_bytes());
    hasher.finalize()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// JSON with object keys sorted at every level, so the same input hashes the
/// same whatever order its keys arrived in.
fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let body: Vec<String> = entries
                .into_iter()
                .map(|(key, value)| {
                    format!("{}:{}", Value::from(key.as_str()), canonical_json(value))
                })
                .collect();
            format!("{{{}}}", body.join(","))
        }
        Value::Array(items) => {
            let body: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", body.join(","))
        }
        other => other.to_string(),
    }
}

/// The first present, non-null value among `keys`.
fn get<'a>(object: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .filter_map(|key| object.get(*key))
        .find(|value| !value.is_null())
}

fn text(object: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    get(object, keys)?.as_str().map(String::from)
}

/// A string or number as text.
fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// An id becomes part of a node path, so it must be short and contain no `/`.
fn is_valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_ID_BYTES && !value.contains('/')
}

fn id(
    object: &Map<String, Value>,
    field: &'static str,
    keys: &[&str],
) -> Result<Option<String>, RecordError> {
    match get(object, keys) {
        None => Ok(None),
        Some(Value::String(value)) if is_valid_id(value) => Ok(Some(value.clone())),
        Some(_) => Err(RecordError::InvalidId(field)),
    }
}

/// The ids of the calls in a batch, given as strings or as objects.
fn tool_calls(object: &Map<String, Value>) -> Result<Option<Vec<String>>, RecordError> {
    let Some(calls) = object.get("tool_calls").and_then(Value::as_array) else {
        return Ok(None);
    };
    calls
        .iter()
        .map(|call| {
            let id = match call {
                Value::String(id) => Some(id.as_str()),
                Value::Object(call) => ["tool_use_id", "id"]
                    .iter()
                    .find_map(|key| call.get(*key).and_then(Value::as_str)),
                _ => None,
            };
            id.filter(|id| is_valid_id(id))
                .map(String::from)
                .ok_or(RecordError::InvalidId("tool_calls"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// The background task list, or `None` if any entry lacks a usable id. A
/// partly read list must not look like "these tasks finished".
fn background_tasks(object: &Map<String, Value>) -> Option<Vec<BackgroundTask>> {
    get(object, &["background_tasks", "backgroundTasks"])?
        .as_array()?
        .iter()
        .map(|task| {
            let task = task.as_object()?;
            Some(BackgroundTask {
                id: text(task, &["id"]).filter(|id| is_valid_id(id))?,
                kind: text(task, &["type"]),
                status: text(task, &["status"]),
            })
        })
        .collect()
}
