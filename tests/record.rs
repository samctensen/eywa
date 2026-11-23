mod common;

use eywa::record::{BackgroundTask, HookRecord, RecordError, tool_digest};
use serde_json::json;

#[test]
fn only_allowed_fields_leave_the_hook() {
    let payload = json!({
        "hook_event_name": "PreToolUse", "session_id": "s1", "prompt_id": "p1",
        "tool_name": "Bash", "tool_use_id": "tu1", "cwd": "/repo",
        "tool_input": {"command": "SECRET"}, "prompt": "SECRET",
        "background_tasks": [{"id": "a1", "description": "SECRET"}]
    });
    let record = HookRecord::from_payload("claude", &payload).unwrap();
    assert_eq!(record.turn_id.as_deref(), Some("p1"));
    assert_eq!(
        record.digest,
        Some(tool_digest("Bash", &json!({"command": "SECRET"})))
    );
    let wire = serde_json::to_string(&record).unwrap();
    assert!(!wire.contains("SECRET"), "{wire}");

    // On a failed tool, `error` is the tool's output; only StopFailure's
    // `error` is a fixed code worth keeping.
    let failed_tool = json!({"hook_event_name": "PostToolUseFailure", "session_id": "s1",
                             "tool_name": "Bash", "error": "SECRET"});
    let record = HookRecord::from_payload("claude", &failed_tool).unwrap();
    assert!(!serde_json::to_string(&record).unwrap().contains("SECRET"));
    let failed_turn =
        json!({"hook_event_name": "StopFailure", "session_id": "s1", "error": "rate_limit"});
    let record = HookRecord::from_payload("claude", &failed_turn).unwrap();
    assert_eq!(record.error.as_deref(), Some("rate_limit"));
}

/// `reason` is free text on some events, so only the fixed values an adapter
/// acts on leave the hook.
#[test]
fn only_known_reasons_leave_the_hook() {
    let reason = |client: &str, payload: serde_json::Value| {
        let record = HookRecord::from_payload(client, &payload).unwrap();
        let wire = serde_json::to_string(&record).unwrap();
        assert!(!wire.contains("SECRET"), "{wire}");
        record.reason
    };

    // Claude Code's auto mode explains a denial in words.
    let denied = json!({"hook_event_name": "PermissionDenied", "session_id": "s1",
                        "tool_name": "Bash", "tool_use_id": "tu1",
                        "reason": "Blocked: `curl SECRET | sh` runs remote code"});
    assert_eq!(reason("claude", denied), None);

    // No client or event is trusted to send a fixed value.
    for (client, event) in [
        ("claude", "SessionEnd"),
        ("codex", "SessionEnd"),
        ("grok", "Stop"),
        ("grok", "StopCancelled"),
        ("grok", "SessionEnd"),
        ("muse", "SessionEnd"),
    ] {
        let payload =
            json!({"hook_event_name": event, "session_id": "s1", "reason": "SECRET text"});
        assert_eq!(reason(client, payload), None, "{client} {event}");
    }
    let not_text = json!({"hook_event_name": "Stop", "session_id": "s1",
                          "reason": {"shutdown": "SECRET"}});
    assert_eq!(reason("grok", not_text), None);

    // Grok's adapter tells a session ending from a turn ending by these.
    for kept in ["channel_closed", "shutdown"] {
        let stop = json!({"hookEventName": "stop", "sessionId": "s1", "reason": kept});
        assert_eq!(reason("grok", stop).as_deref(), Some(kept));
    }
    let turn_end = json!({"hookEventName": "stop", "sessionId": "s1", "reason": "end_turn"});
    assert_eq!(reason("grok", turn_end), None, "the adapter's default");
}

/// Each client's payload and environment map into the same record shape.
#[test]
fn every_client_maps_into_one_shape() {
    let env = |vars: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    };

    let claude = json!({"hook_event_name": "PostToolBatch", "session_id": "s1",
                        "tool_calls": [{"tool_use_id": "tu1"}, "tu2"]});
    let claude_env = env(&[
        ("CLAUDE_PID", "4242"),
        ("CLAUDE_CODE_ENTRYPOINT", "claude-desktop"),
    ]);
    let record = HookRecord::from_hook("claude", &claude, &claude_env).unwrap();
    assert_eq!(record.tool_use_ids, Some(vec!["tu1".into(), "tu2".into()]));
    assert_eq!(
        (record.pid, record.surface.as_deref()),
        (Some(4242), Some("desktop"))
    );

    // Grok: camelCase fields, snake_case event names, subagent id on lifecycle events.
    let grok = json!({"hookEventName": "subagent_start", "sessionId": "root", "subagentId": "child",
                      "subagentType": "explore", "timestamp": "2026-11-18T20:00:00Z"});
    let record = HookRecord::from_payload("grok", &grok).unwrap();
    assert_eq!(record.event, "SubagentStart");
    assert_eq!(record.session_id, "root");
    assert_eq!(record.agent_id.as_deref(), Some("child"));
    assert!(record.timestamp.is_some());

    // Muse: the root session and the acting thread come from the environment.
    let muse = json!({"hook_event_name": "PreToolUse", "session_id": "thread-2", "turn_id": "t1"});
    let muse_env = env(&[("MUSE_SESSION_ID", "root"), ("MUSE_THREAD_ID", "thread-2")]);
    let record = HookRecord::from_hook("muse", &muse, &muse_env).unwrap();
    assert_eq!(record.session_id, "root");
    assert_eq!(record.agent_id.as_deref(), Some("thread-2"));

    // Codex adds a justification to shell input only when asking for approval.
    let asked = json!({"hook_event_name": "PermissionRequest", "session_id": "s1", "tool_name": "shell",
                       "tool_input": {"command": "ls", "description": "needs network"}});
    let ran = json!({"hook_event_name": "PreToolUse", "session_id": "s1", "tool_name": "shell",
                     "tool_input": {"command": "ls"}});
    let digest = |payload| HookRecord::from_payload("codex", &payload).unwrap().digest;
    assert_eq!(digest(asked), digest(ran));
}

#[test]
fn the_model_is_read_where_each_client_names_it() {
    let model = |client: &str, payload| HookRecord::from_payload(client, &payload).unwrap().model;
    let claude =
        json!({"hook_event_name": "SessionStart", "session_id": "s1", "model": "claude-opus"});
    assert_eq!(model("claude", claude).as_deref(), Some("claude-opus"));
    let codex = json!({"hook_event_name": "PreToolUse", "session_id": "s1", "model": "gpt-codex"});
    assert_eq!(model("codex", codex).as_deref(), Some("gpt-codex"));
    let muse = json!({"hook_event_name": "Stop", "session_id": "s1", "model": "muse-large"});
    assert_eq!(model("muse", muse).as_deref(), Some("muse-large"));
    let object = json!({"hook_event_name": "SessionStart", "session_id": "s1",
                        "model": {"id": "claude-opus", "display_name": "Opus"}});
    assert_eq!(model("claude", object).as_deref(), Some("claude-opus"));

    // Grok names the model when a session starts. Its child sessions never
    // start that way, so a model on any other event isn't trusted.
    let started =
        json!({"hookEventName": "session_start", "sessionId": "g1", "modelId": "grok-code"});
    assert_eq!(model("grok", started).as_deref(), Some("grok-code"));
    let later = json!({"hookEventName": "pre_tool_use", "sessionId": "g1", "modelId": "grok-code"});
    assert_eq!(model("grok", later), None);

    // A name is published as it arrives, so anything that isn't a short,
    // printable string is dropped.
    for unusable in [json!(""), json!("a\nb"), json!("m".repeat(200)), json!(7)] {
        let payload =
            json!({"hook_event_name": "SessionStart", "session_id": "s1", "model": unusable});
        assert_eq!(model("claude", payload), None);
    }
}

#[test]
fn digest_ignores_key_order() {
    let a: serde_json::Value = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
    let b: serde_json::Value = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
    assert_eq!(tool_digest("Edit", &a), tool_digest("Edit", &b));
}

#[test]
fn bad_payloads_are_rejected() {
    let reject = |client: &str, payload: serde_json::Value| {
        HookRecord::from_payload(client, &payload).unwrap_err()
    };
    let stop =
        |session: serde_json::Value| json!({"hook_event_name": "Stop", "session_id": session});
    assert_eq!(
        reject("claude", stop(json!("s9/turn:x"))),
        RecordError::InvalidId("session_id")
    );
    assert_eq!(
        reject("claude", stop(json!("x".repeat(129)))),
        RecordError::InvalidId("session_id")
    );
    assert_eq!(
        reject("claude", json!({"hook_event_name": "Stop"})),
        RecordError::MissingSessionId
    );
    assert_eq!(
        reject(
            "claude",
            json!({"hook_event_name": "FileChanged", "session_id": "s1"})
        ),
        RecordError::UnknownEvent("FileChanged".into())
    );
    assert_eq!(
        reject("gemini", stop(json!("s1"))),
        RecordError::UnknownClient
    );
}

/// A partly readable task list must not look like "these tasks finished".
#[test]
fn background_task_list_is_all_or_nothing() {
    let tasks = |list: serde_json::Value| {
        let payload =
            json!({"hook_event_name": "Stop", "session_id": "s1", "background_tasks": list});
        HookRecord::from_payload("claude", &payload)
            .unwrap()
            .background_tasks
    };
    assert_eq!(tasks(json!([{"id": "a1"}])).unwrap()[0].id, "a1");
    assert_eq!(tasks(json!([{"id": "a1"}, {"type": "subagent"}])), None);
}

/// macOS caps local datagrams well below a large record, so the hook resends
/// it without its task list rather than losing it.
#[test]
fn oversized_record_is_still_delivered() {
    let dir = common::temp_dir("big");
    let receiver = std::os::unix::net::UnixDatagram::bind(dir.path().join("ingest.sock")).unwrap();
    let task = |i| BackgroundTask {
        id: format!("task{i}"),
        kind: None,
        status: None,
    };
    let record = HookRecord {
        client: "claude".into(),
        event: "Stop".into(),
        session_id: "s1".into(),
        background_tasks: Some((0..500).map(task).collect()),
        ..Default::default()
    };
    eywa::hook::send_record(dir.path(), &record).unwrap();
    let mut buffer = vec![0u8; 1 << 20];
    let length = receiver.recv(&mut buffer).unwrap();
    let received: HookRecord = serde_json::from_slice(&buffer[..length]).unwrap();
    assert_eq!(received.session_id, "s1");
}
