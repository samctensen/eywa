//! Hook sequences replayed through the adapters. Each step is an event name
//! and the record's other fields; every record defaults to session `s1` and
//! turn `p1`, and steps run one second apart unless a test sets the time.

use eywa::adapter::Adapter;
use eywa::adapter::claude::ClaudeAdapter;
use eywa::adapter::codex::CodexAdapter;
use eywa::adapter::grok::GrokAdapter;
use eywa::adapter::muse::MuseAdapter;
use eywa::record::HookRecord;
use eywa::registry::{SessionState, Status};
use eywa::row::Change;
use eywa::tree::{Millis, STALE_AFTER, State, Tree};
use serde_json::{Value, json};

type Step = (&'static str, Value);

fn feed(tree: &mut Tree, adapter: &mut dyn Adapter, (event, fields): &Step, at: Millis) {
    let mut record = json!({
        "client": adapter.client().name, "session_id": "s1", "turn_id": "p1", "event": event,
    });
    record
        .as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    let record: HookRecord = serde_json::from_value(record).unwrap();
    for event in adapter.handle(&record, at) {
        tree.apply(event, at);
    }
}

fn replay(adapter: &mut dyn Adapter, steps: &[Step]) -> Tree {
    let mut tree = Tree::new(&[adapter.client()], STALE_AFTER, 0);
    for (n, step) in steps.iter().enumerate() {
        feed(&mut tree, adapter, step, n as Millis * 1000);
    }
    tree
}

fn claude(steps: &[Step]) -> Tree {
    replay(&mut ClaudeAdapter::new(), steps)
}

fn codex(steps: &[Step]) -> Tree {
    replay(&mut CodexAdapter::new(), steps)
}

fn grok(steps: &[Step]) -> Tree {
    replay(&mut GrokAdapter::new(), steps)
}

fn muse(steps: &[Step]) -> Tree {
    replay(&mut MuseAdapter::new(), steps)
}

fn last_outcome(tree: &Tree, session: &str) -> Value {
    tree.attrs(session).unwrap()["last_turn"]["outcome"].clone()
}

fn attrs(tree: &Tree, id: &str) -> Value {
    Value::Object(tree.attrs(id).unwrap_or_else(|| panic!("no {id}")).clone())
}

/// A record from Grok child session `c1`, which carries no prompt id.
fn grok_child(fields: Value) -> Value {
    let mut record = json!({"session_id": "c1", "turn_id": null});
    record
        .as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    record
}

/// The design's first demonstration: the main turn ends while a subagent
/// waits for approval; approving and finishing the subagent clears it.
#[test]
fn first_demonstration() {
    let waiting = [
        ("SessionStart", json!({"source": "startup"})),
        ("UserPromptSubmit", json!({})),
        ("SubagentStart", json!({"agent_id": "a1"})),
        (
            "PreToolUse",
            json!({"agent_id": "a1", "tool_name": "Bash", "tool_use_id": "tu1", "digest": "d1"}),
        ),
        (
            "PermissionRequest",
            json!({"agent_id": "a1", "tool_name": "Bash", "digest": "d1"}),
        ),
        ("Stop", json!({"background_tasks": [{"id": "a1"}]})),
    ];
    let t = claude(&waiting);
    assert_eq!(t.state("/claude"), Some(State::NeedsInput));
    assert_eq!(
        t.cause("/claude").as_deref(),
        Some("/claude/session:s1/turn:p1/agent:a1/request:tu1")
    );

    let finished = [
        (
            "PostToolUse",
            json!({"agent_id": "a1", "tool_use_id": "tu1"}),
        ),
        ("SubagentStop", json!({"agent_id": "a1"})),
    ];
    let t = claude(&[waiting.as_slice(), &finished].concat());
    assert_eq!(t.state("/claude"), Some(State::Idle));
}

/// Claude has no interrupt hook: the next prompt or the idle notification
/// closes an interrupted turn.
#[test]
fn interrupted_turn_is_cancelled() {
    let t = claude(&[
        ("UserPromptSubmit", json!({})),
        ("UserPromptSubmit", json!({"turn_id": "p2"})),
    ]);
    assert_eq!(last_outcome(&t, "/claude/session:s1"), "cancelled");
    assert!(t.contains("/claude/session:s1/turn:p2"));

    let t = claude(&[
        ("UserPromptSubmit", json!({})),
        ("Notification", json!({"notification_type": "idle_prompt"})),
    ]);
    assert_eq!(t.state("/claude"), Some(State::Idle));
}

/// Claude's registry going idle closes the turn at once. How it ended is
/// settled afterwards: by the Stop hook, a new prompt, or silence. The
/// registry opening the next turn before its hooks do changes none of that.
#[test]
fn idle_closes_the_turn_before_its_outcome_is_known() {
    let session = "/claude/session:s1";
    let registry = |status| SessionState {
        session_id: "s1".into(),
        pid: 1,
        status,
        status_at: None,
        cwd: None,
        entrypoint: None,
    };
    let apply = |tree: &mut Tree, events: Vec<eywa::tree::Event>, at: Millis| {
        for event in events {
            tree.apply(event, at);
        }
    };
    // A turn open at 0, and the registry idle at one second.
    let interrupted = || {
        let mut adapter = ClaudeAdapter::new();
        let mut tree = Tree::new(&[adapter.client()], STALE_AFTER, 0);
        feed(&mut tree, &mut adapter, &("UserPromptSubmit", json!({})), 0);
        let events = adapter.observe(&registry(Status::Idle), 1_000);
        apply(&mut tree, events, 1_000);
        (adapter, tree)
    };
    let last_turn = |tree: &Tree| tree.attrs(session).unwrap().get("last_turn").cloned();

    let (mut adapter, mut tree) = interrupted();
    assert_eq!(tree.state(session), Some(State::Idle));
    assert_eq!(last_turn(&tree), None);
    let late = ("PostToolUse", json!({"tool_use_id": "tu1"}));
    feed(&mut tree, &mut adapter, &late, 1_100);
    assert_eq!(
        tree.state(session),
        Some(State::Idle),
        "a late tool hook can't reopen the turn"
    );
    feed(&mut tree, &mut adapter, &("Stop", json!({})), 1_200);
    assert_eq!(
        last_turn(&tree),
        Some(json!({"outcome": "ok", "ended_at": "1970-01-01T00:00:01.000Z"}))
    );

    let (mut adapter, mut tree) = interrupted();
    let next = ("UserPromptSubmit", json!({"turn_id": "p2"}));
    feed(&mut tree, &mut adapter, &next, 1_500);
    assert_eq!(last_outcome(&tree, session), "cancelled");
    assert!(tree.contains("/claude/session:s1/turn:p2"));

    let (mut adapter, mut tree) = interrupted();
    let events = adapter.settle("s1", 3_000);
    apply(&mut tree, events, 3_000);
    assert_eq!(last_outcome(&tree, session), "cancelled");

    // Busy again: the idle didn't hold. The turn it closed is the one still
    // running, and nothing is recorded for it.
    let (mut adapter, mut tree) = interrupted();
    let events = adapter.observe(&registry(Status::Busy), 1_300);
    apply(&mut tree, events, 1_300);
    assert!(tree.contains("/claude/session:s1/turn:p1"));
    feed(&mut tree, &mut adapter, &late, 1_350);
    let events = adapter.settle("s1", 3_000);
    apply(&mut tree, events, 3_000);
    assert_eq!(tree.state(session), Some(State::Working));
    assert_eq!(last_turn(&tree), None);
    // A new prompt then finds that turn interrupted.
    feed(&mut tree, &mut adapter, &next, 3_500);
    assert_eq!(last_outcome(&tree, session), "cancelled");

    // A session that ends first gets no outcome afterwards.
    let (mut adapter, mut tree) = interrupted();
    feed(&mut tree, &mut adapter, &("SessionEnd", json!({})), 1_400);
    assert!(adapter.settle("s1", 3_000).is_empty());

    // The registry can report the next turn before its prompt hook does. The
    // placeholder it opens is that same turn, so replacing it with the real
    // one says nothing about how any turn ended.
    let (mut adapter, mut tree) = interrupted();
    let events = adapter.settle("s1", 3_000);
    apply(&mut tree, events, 3_000);
    let events = adapter.observe(&registry(Status::Busy), 4_000);
    apply(&mut tree, events, 4_000);
    assert!(tree.contains("/claude/session:s1/turn:running"));
    feed(&mut tree, &mut adapter, &next, 4_030);
    assert!(tree.contains("/claude/session:s1/turn:p2"));
    assert!(!tree.contains("/claude/session:s1/turn:running"));
    assert_eq!(
        last_turn(&tree),
        Some(json!({"outcome": "cancelled", "ended_at": "1970-01-01T00:00:01.000Z"}))
    );

    // The first hook to name a turn can be the one that ends it, as when
    // Eywa starts mid-turn: the outcome is the real turn's.
    let mut adapter = ClaudeAdapter::new();
    let mut tree = Tree::new(&[adapter.client()], STALE_AFTER, 0);
    let events = adapter.observe(&registry(Status::Busy), 0);
    apply(&mut tree, events, 0);
    feed(&mut tree, &mut adapter, &("StopFailure", json!({})), 500);
    assert_eq!(tree.state(session), Some(State::Idle));
    assert_eq!(
        last_turn(&tree),
        Some(json!({"outcome": "error", "ended_at": "1970-01-01T00:00:00.500Z"}))
    );
}

/// A parallel sibling's hook can arrive just after a permission request; a
/// tool call after the user had time to answer means it was answered.
#[test]
fn approval_survives_a_racing_sibling() {
    let mut adapter = ClaudeAdapter::new();
    let mut t = Tree::new(&[adapter.client()], STALE_AFTER, 0);
    let tool =
        |id: &str, digest: &str| json!({"tool_name": "Read", "tool_use_id": id, "digest": digest});
    let steps: [(Millis, Step); 4] = [
        (0, ("PreToolUse", tool("tu1", "d1"))),
        (
            5,
            (
                "PermissionRequest",
                json!({"tool_name": "Read", "digest": "d1"}),
            ),
        ),
        (8, ("PreToolUse", tool("tu2", "d2"))),
        (2_000, ("PreToolUse", tool("tu3", "d3"))),
    ];
    for (at, step) in &steps[..3] {
        feed(&mut t, &mut adapter, step, *at);
    }
    assert_eq!(t.state("/claude"), Some(State::NeedsInput));
    feed(&mut t, &mut adapter, &steps[3].1, steps[3].0);
    assert_eq!(t.state("/claude"), Some(State::Working));
}

#[test]
fn questions_and_mcp_input_are_requests() {
    let t = claude(&[
        ("UserPromptSubmit", json!({})),
        (
            "PreToolUse",
            json!({"tool_name": "AskUserQuestion", "tool_use_id": "tu5"}),
        ),
    ]);
    let row = t.rows().into_iter().find(|r| r.id == "/claude").unwrap();
    assert_eq!(row.reason.as_deref(), Some("question"));

    let t = claude(&[
        ("UserPromptSubmit", json!({})),
        ("Elicitation", json!({"elicitation_id": "e1"})),
    ]);
    let row = t.rows().into_iter().find(|r| r.id == "/claude").unwrap();
    assert_eq!(row.reason.as_deref(), Some("mcp_input"));
}

/// One thread's hooks arrive in order, so activity under a closed node's own
/// id means it is running again: a blocked Stop, a resumed subagent.
#[test]
fn activity_reopens_what_it_belongs_to() {
    let t = claude(&[
        ("UserPromptSubmit", json!({})),
        ("Stop", json!({})),
        ("PreToolUse", json!({"tool_use_id": "tu1"})),
    ]);
    assert_eq!(
        t.state("/claude"),
        Some(State::Working),
        "a blocked Stop continued the turn"
    );

    let t = claude(&[
        ("UserPromptSubmit", json!({})),
        ("SubagentStart", json!({"agent_id": "a1"})),
        ("SubagentStop", json!({"agent_id": "a1"})),
        ("Stop", json!({})),
        ("SubagentStart", json!({"agent_id": "a1"})),
    ]);
    assert!(
        t.contains("/claude/session:s1/turn:p1/agent:a1"),
        "a resumed subagent runs again"
    );
    assert_eq!(
        t.attrs("/claude/session:s1").unwrap()["last_turn"]["ended_at"],
        "1970-01-01T00:00:03.000Z",
        "and its turn ended when it did, not when the subagent resumed"
    );

    // An ended session stays ended for everything but a new start or prompt.
    let t = claude(&[
        ("UserPromptSubmit", json!({})),
        ("SessionEnd", json!({})),
        ("PostToolUse", json!({"tool_use_id": "tu1"})),
    ]);
    assert!(!t.contains("/claude/session:s1"));
}

/// `--resume` reuses the session id, and Codex resumes threads it ended.
#[test]
fn resumed_session_is_tracked_again() {
    let steps = |turn: &str| {
        [
            ("SessionStart", json!({})),
            ("SessionEnd", json!({})),
            ("SessionStart", json!({"source": "resume"})),
            ("UserPromptSubmit", json!({"turn_id": turn})),
            (
                "PermissionRequest",
                json!({"turn_id": turn, "tool_name": "Bash", "digest": "d1"}),
            ),
        ]
    };
    assert_eq!(
        claude(&steps("p2")).state("/claude"),
        Some(State::NeedsInput)
    );
    assert_eq!(codex(&steps("t2")).state("/codex"), Some(State::NeedsInput));
}

#[test]
fn new_session_in_the_same_process_ends_the_old_one() {
    let t = claude(&[
        ("SessionStart", json!({"pid": 42})),
        (
            "SessionStart",
            json!({"session_id": "s2", "source": "clear", "pid": 42}),
        ),
    ]);
    assert!(!t.contains("/claude/session:s1"));
    assert!(t.contains("/claude/session:s2"));
}

/// After a restart Eywa hasn't seen the turn start, so activity opens it.
#[test]
fn activity_after_a_restart_opens_what_was_missed() {
    let t = claude(&[("PostToolUse", json!({"tool_use_id": "tu1"}))]);
    assert_eq!(t.state("/claude"), Some(State::Working));

    let background = [
        ("Stop", json!({})),
        (
            "PermissionRequest",
            json!({"agent_id": "a7", "tool_name": "Bash", "digest": "d7"}),
        ),
    ];
    assert_eq!(
        claude(&background).state("/claude"),
        Some(State::NeedsInput)
    );
    let done = [
        background.as_slice(),
        &[("SubagentStop", json!({"agent_id": "a7"}))],
    ]
    .concat();
    let t = claude(&done);
    assert_eq!(t.state("/claude"), Some(State::Idle));
    assert!(
        !t.contains("/claude/session:s1/turn:p1"),
        "the ended turn wasn't revived"
    );

    assert_eq!(
        codex(&[("PreToolUse", json!({}))]).state("/codex"),
        Some(State::Working)
    );
}

/// `Stop` lists the background tasks still running. Unlisted agents close,
/// except one waiting on you; no list means no reconciliation.
#[test]
fn stop_reconciles_background_agents() {
    let start = [
        ("UserPromptSubmit", json!({})),
        ("SubagentStart", json!({"agent_id": "a1"})),
        ("SubagentStart", json!({"agent_id": "a2"})),
        ("SubagentStart", json!({"agent_id": "a3"})),
        (
            "PermissionRequest",
            json!({"agent_id": "a3", "tool_name": "Bash", "digest": "d3"}),
        ),
    ];
    let listed = [("Stop", json!({"background_tasks": [{"id": "a1"}]}))];
    let t = claude(&[start.as_slice(), &listed].concat());
    let agent = |id: &str| format!("/claude/session:s1/turn:p1/agent:{id}");
    assert!(t.contains(&agent("a1")));
    assert!(!t.contains(&agent("a2")));
    assert!(t.contains(&agent("a3")), "an agent waiting on you stays");

    let t = claude(&[start.as_slice(), &[("Stop", json!({}))]].concat());
    assert!(t.contains(&agent("a2")));
}

/// Inside a Codex subagent, `turn_id` is the subagent's own turn. It must not
/// become a root turn that nothing ever closes.
#[test]
fn codex_subagents_stay_under_the_root_turn() {
    let child = json!({"agent_id": "a1", "turn_id": "child-turn"});
    let t = codex(&[
        ("UserPromptSubmit", json!({})),
        ("SubagentStart", child.clone()),
        (
            "PreToolUse",
            json!({"agent_id": "a1", "turn_id": "child-turn", "tool_use_id": "tu1", "digest": "d1"}),
        ),
        (
            "PermissionRequest",
            json!({"agent_id": "a1", "turn_id": "child-turn", "digest": "d1"}),
        ),
    ]);
    assert_eq!(
        t.cause("/codex").as_deref(),
        Some("/codex/session:s1/turn:p1/agent:a1/request:tu1"),
        "the approval belongs to the subagent, under the root turn"
    );
    let t = codex(&[
        ("UserPromptSubmit", json!({})),
        ("SubagentStart", child.clone()),
        (
            "PreToolUse",
            json!({"agent_id": "a1", "turn_id": "child-turn"}),
        ),
        ("Stop", json!({})),
        ("SubagentStop", child),
    ]);
    assert_eq!(t.state("/codex"), Some(State::Idle));
    assert!(!t.contains("/codex/session:s1/turn:child-turn"));
}

#[test]
fn codex_approvals_and_turns() {
    let asking = [
        ("UserPromptSubmit", json!({})),
        ("PreToolUse", json!({"digest": "d1"})),
        ("PermissionRequest", json!({"digest": "d1"})),
        ("PostToolUse", json!({"digest": "d9"})),
    ];
    let t = codex(&asking);
    assert_eq!(t.state("/codex"), Some(State::NeedsInput));
    assert_eq!(
        t.cause("/codex").as_deref(),
        Some("/codex/session:s1/turn:p1/request:d1"),
        "Codex approvals attach to the turn"
    );
    let answered = [
        asking.as_slice(),
        &[("PreToolUse", json!({"digest": "d2"}))],
    ]
    .concat();
    assert_eq!(codex(&answered).state("/codex"), Some(State::Working));

    let t = codex(&[
        ("UserPromptSubmit", json!({})),
        ("SubagentStart", json!({"agent_id": "a1"})),
        ("Interrupt", json!({})),
    ]);
    assert_eq!(last_outcome(&t, "/codex/session:s1"), "cancelled");
    assert_eq!(
        t.state("/codex"),
        Some(State::Working),
        "the subagent outlives the turn"
    );
}

/// Grok subagents are child sessions, and approvals come only as a
/// notification with no ids, so they attach to the turn. Rejecting one
/// cancels the turn.
#[test]
fn grok_child_sessions_and_turn_approvals() {
    let child = |fields: Value| {
        let mut record = json!({"session_id": "c1", "turn_id": null});
        record
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        record
    };
    let asking = [
        ("UserPromptSubmit", json!({})),
        ("SubagentStart", json!({"agent_id": "c1"})),
        ("PreToolUse", child(json!({"tool_use_id": "tu1"}))),
        (
            "Notification",
            json!({"turn_id": null, "notification_type": "permission_prompt"}),
        ),
    ];
    let t = grok(&asking);
    assert_eq!(t.state("/grok"), Some(State::NeedsInput));
    assert_eq!(
        t.cause("/grok").as_deref(),
        Some("/grok/session:s1/turn:p1/request:tu1")
    );
    assert!(
        t.contains("/grok/session:s1/turn:p1/agent:c1"),
        "the child is an agent, not a session"
    );

    let rejected = [
        asking.as_slice(),
        &[("StopCancelled", json!({"reason": "permission_rejected"}))],
    ]
    .concat();
    let t = grok(&rejected);
    assert_eq!(last_outcome(&t, "/grok/session:s1"), "cancelled");
    assert_eq!(
        t.state("/grok"),
        Some(State::Working),
        "the subagent is still running"
    );

    let ended = [rejected.as_slice(), &[("SessionEnd", child(json!({})))]].concat();
    assert_eq!(grok(&ended).state("/grok"), Some(State::Idle));
}

/// Muse names the acting thread in its environment, which the hook record
/// carries as `agent_id`. `ToolUseStart` is the only sign an approval was granted.
#[test]
fn muse_thread_approvals() {
    let asking = [
        ("UserPromptSubmit", json!({})),
        ("SubagentStart", json!({"agent_id": "th2"})),
        (
            "PreToolUse",
            json!({"agent_id": "th2", "tool_use_id": "tu1", "digest": "d1"}),
        ),
        (
            "PermissionRequest",
            json!({"agent_id": "th2", "digest": "d1"}),
        ),
    ];
    let t = muse(&asking);
    assert_eq!(
        t.cause("/muse").as_deref(),
        Some("/muse/session:s1/turn:p1/agent:th2/request:tu1")
    );
    let granted = [
        asking.as_slice(),
        &[(
            "ToolUseStart",
            json!({"agent_id": "th2", "tool_use_id": "tu1"}),
        )],
    ]
    .concat();
    assert_eq!(muse(&granted).state("/muse"), Some(State::Working));
}

/// An approval or question names the tool it is for whenever the hook that
/// opens it does.
#[test]
fn requests_name_their_tool() {
    let asking = [
        ("UserPromptSubmit", json!({})),
        (
            "PreToolUse",
            json!({"tool_name": "Bash", "tool_use_id": "tu1", "digest": "d1"}),
        ),
        (
            "PermissionRequest",
            json!({"tool_name": "Bash", "digest": "d1"}),
        ),
    ];
    let approval = json!({"reason": "approval", "tool_name": "Bash"});
    for (client, tree) in [
        ("claude", claude(&asking)),
        ("codex", codex(&asking)),
        ("muse", muse(&asking)),
    ] {
        let request = format!("/{client}/session:s1/turn:p1/request:tu1");
        assert_eq!(attrs(&tree, &request), approval, "{client}");
    }

    // The approval's own hook names the tool, even when it is matched to a
    // different call: Claude saw only the Read start, not the Bash one.
    let t = claude(&[
        ("UserPromptSubmit", json!({})),
        (
            "PreToolUse",
            json!({"tool_name": "Read", "tool_use_id": "tu1", "digest": "d1"}),
        ),
        (
            "PermissionRequest",
            json!({"tool_name": "Bash", "digest": "d9"}),
        ),
    ]);
    assert_eq!(
        attrs(&t, "/claude/session:s1/turn:p1/request:tu1"),
        approval
    );

    let ask = |tool: &str| {
        [
            ("UserPromptSubmit", json!({})),
            (
                "PreToolUse",
                json!({"tool_name": tool, "tool_use_id": "tu5"}),
            ),
        ]
    };
    for (client, tool, tree) in [
        ("claude", "AskUserQuestion", claude(&ask("AskUserQuestion"))),
        (
            "codex",
            "request_user_input",
            codex(&ask("request_user_input")),
        ),
        ("grok", "ask_user_question", grok(&ask("ask_user_question"))),
    ] {
        let request = format!("/{client}/session:s1/turn:p1/request:tu5");
        let question = json!({"reason": "question", "tool_name": tool});
        assert_eq!(attrs(&tree, &request), question, "{client}");
    }
}

/// A request whose hook doesn't name its tool names none, even when Eywa
/// knows a call it is likely for.
#[test]
fn requests_name_no_tool_they_were_not_told() {
    let call = (
        "PreToolUse",
        json!({"tool_name": "Bash", "tool_use_id": "tu1", "digest": "d1"}),
    );
    let notified = [
        ("UserPromptSubmit", json!({})),
        call.clone(),
        (
            "Notification",
            json!({"notification_type": "permission_prompt"}),
        ),
    ];
    let approval = json!({"reason": "approval"});
    // Grok keys its approval to the newest call, but the notification says
    // nothing of which call it is for.
    let t = grok(&notified);
    assert_eq!(attrs(&t, "/grok/session:s1/turn:p1/request:tu1"), approval);
    // Prompts no approval hook announced.
    let t = claude(&notified);
    assert_eq!(
        attrs(&t, "/claude/session:s1/turn:p1/request:prompt"),
        approval
    );
    let t = muse(&notified);
    assert_eq!(
        attrs(&t, "/muse/session:s1/turn:p1/request:prompt"),
        approval
    );

    // Claude's session registry says it is waiting, not on which tool.
    let mut adapter = ClaudeAdapter::new();
    let mut t = Tree::new(&[adapter.client()], STALE_AFTER, 0);
    feed(&mut t, &mut adapter, &("UserPromptSubmit", json!({})), 0);
    feed(&mut t, &mut adapter, &call, 500);
    let waiting = SessionState {
        session_id: "s1".into(),
        pid: 1,
        status: Status::Waiting(Some("permission prompt".into())),
        status_at: None,
        cwd: None,
        entrypoint: None,
    };
    for event in adapter.observe(&waiting, 1_000) {
        t.apply(event, 1_000);
    }
    assert_eq!(
        attrs(&t, "/claude/session:s1/turn:p1/request:waiting"),
        approval
    );
}

#[test]
fn mcp_input_names_the_server_asking() {
    let asked = |fields: Value| claude(&[("UserPromptSubmit", json!({})), ("Elicitation", fields)]);
    let request = json!({"reason": "mcp_input", "mcp_server": "github"});
    let t = asked(json!({"elicitation_id": "e1", "mcp_server": "github"}));
    assert_eq!(attrs(&t, "/claude/session:s1/turn:p1/request:e1"), request);
    let t = asked(json!({"mcp_server": "github"}));
    assert_eq!(
        attrs(&t, "/claude/session:s1/turn:p1/request:mcp-github"),
        request
    );
    let t = asked(json!({"elicitation_id": "e1"}));
    assert_eq!(
        attrs(&t, "/claude/session:s1/turn:p1/request:e1"),
        json!({"reason": "mcp_input"})
    );
}

/// An agent row names the subagent's type once the client does, and keeps it
/// when the subagent runs again. Only hooks that name the agent's own type
/// count: Claude's start, every Codex record from inside the subagent, and a
/// Grok child's own records. Muse names none Eywa can use.
#[test]
fn agents_name_their_type() {
    let agent = |client: &str| format!("/{client}/session:s1/turn:p1/agent:a1");
    let explore = json!({"agent_type": "Explore"});
    let untyped = json!({});

    let started = [
        ("UserPromptSubmit", json!({})),
        (
            "SubagentStart",
            json!({"agent_id": "a1", "agent_type": "Explore"}),
        ),
    ];
    assert_eq!(attrs(&claude(&started), &agent("claude")), explore);
    assert_eq!(attrs(&codex(&started), &agent("codex")), explore);
    // Stopped, then running again without a new start.
    let again = [
        started.as_slice(),
        &[
            ("SubagentStop", json!({"agent_id": "a1", "agent_type": ""})),
            ("UserPromptSubmit", json!({"agent_id": "a1"})),
        ],
    ]
    .concat();
    assert_eq!(attrs(&codex(&again), &agent("codex")), explore);
    let again = [
        started.as_slice(),
        &[
            ("SubagentStop", json!({"agent_id": "a1", "agent_type": ""})),
            (
                "PreToolUse",
                json!({"agent_id": "a1", "tool_use_id": "tu1"}),
            ),
        ],
    ]
    .concat();
    assert_eq!(attrs(&claude(&again), &agent("claude")), explore);

    // Seen first through its activity, as after a restart.
    let active = [
        ("UserPromptSubmit", json!({})),
        (
            "PermissionRequest",
            json!({"agent_id": "a1", "agent_type": "Explore", "tool_name": "Bash", "digest": "d1"}),
        ),
    ];
    let t = codex(&active);
    assert_eq!(attrs(&t, &agent("codex")), explore);
    assert_eq!(
        attrs(&t, &format!("{}/request:d1", agent("codex"))),
        json!({"reason": "approval", "tool_name": "Bash"})
    );
    assert_eq!(
        attrs(&claude(&active), &agent("claude")),
        untyped,
        "only Claude's start names the agent's own type"
    );

    // A Grok start doesn't say for certain whose type it names; the child's
    // own records do.
    let spawned = [
        ("UserPromptSubmit", json!({})),
        (
            "SubagentStart",
            json!({"agent_id": "c1", "agent_type": "explore"}),
        ),
    ];
    let child = "/grok/session:s1/turn:p1/agent:c1";
    assert_eq!(attrs(&grok(&spawned), child), untyped);
    let prompted = [
        spawned.as_slice(),
        &[(
            "UserPromptSubmit",
            grok_child(json!({"agent_type": "explore"})),
        )],
    ]
    .concat();
    assert_eq!(
        attrs(&grok(&prompted), child),
        json!({"agent_type": "explore"})
    );

    let t = muse(&[
        ("UserPromptSubmit", json!({})),
        ("SubagentStart", json!({"agent_id": "a1"})),
        (
            "PreToolUse",
            json!({"agent_id": "a1", "agent_type": "worker", "tool_use_id": "tu1"}),
        ),
    ]);
    assert_eq!(attrs(&t, &agent("muse")), untyped);
}

/// Two approvals can land on one request, when the second matches no call of
/// its own. The row names the tool asked about last, and only its own actor's.
#[test]
fn a_later_approval_on_the_same_request_names_its_tool() {
    let asked_twice = [
        ("UserPromptSubmit", json!({})),
        (
            "PreToolUse",
            json!({"tool_name": "Bash", "tool_use_id": "tu1", "digest": "d1"}),
        ),
        (
            "PermissionRequest",
            json!({"tool_name": "Bash", "digest": "d1"}),
        ),
        (
            "PermissionRequest",
            json!({"tool_name": "apply_patch", "digest": "d7"}),
        ),
    ];
    for (client, tree) in [
        ("claude", claude(&asked_twice)),
        ("codex", codex(&asked_twice)),
        ("muse", muse(&asked_twice)),
    ] {
        assert_eq!(
            attrs(&tree, &format!("/{client}/session:s1/turn:p1/request:tu1")),
            json!({"reason": "approval", "tool_name": "apply_patch"}),
            "{client}"
        );
    }

    // With no digest and no call to match, the main thread's approval and a
    // subagent's share one request id. The subagent's tool isn't the turn's.
    let t = codex(&[
        ("UserPromptSubmit", json!({})),
        ("PermissionRequest", json!({"tool_name": "Bash"})),
        ("SubagentStart", json!({"agent_id": "a1"})),
        (
            "PermissionRequest",
            json!({"agent_id": "a1", "tool_name": "apply_patch"}),
        ),
    ]);
    assert_eq!(
        attrs(&t, "/codex/session:s1/turn:p1/request:approval"),
        json!({"reason": "approval", "tool_name": "Bash"})
    );
}

/// A Grok child that starts a subagent of its own keeps its own type, and
/// the start names no type for the agent it spawns.
#[test]
fn a_grok_childs_start_types_neither_agent() {
    let t = grok(&[
        ("UserPromptSubmit", json!({})),
        ("SubagentStart", json!({"agent_id": "c1"})),
        (
            "UserPromptSubmit",
            grok_child(json!({"agent_type": "explore"})),
        ),
        (
            "SubagentStart",
            grok_child(json!({"agent_id": "c2", "agent_type": "plan"})),
        ),
    ]);
    assert_eq!(
        attrs(&t, "/grok/session:s1/turn:p1/agent:c1"),
        json!({"agent_type": "explore"})
    );
    assert_eq!(attrs(&t, "/grok/session:s1/turn:p1/agent:c2"), json!({}));
}

/// A session row names the model its main thread runs on, once the client
/// says so, and follows a later change.
#[test]
fn sessions_name_their_model() {
    let started = [("SessionStart", json!({"model": "m1"}))];
    for (client, tree) in [
        ("claude", claude(&started)),
        ("codex", codex(&started)),
        ("grok", grok(&started)),
        ("muse", muse(&started)),
    ] {
        let session = format!("/{client}/session:s1");
        assert_eq!(attrs(&tree, &session)["model"], "m1", "{client}");
    }
    assert_eq!(
        attrs(
            &claude(&[("SessionStart", json!({}))]),
            "/claude/session:s1"
        ),
        json!({}),
        "no model until the client names one"
    );

    // A record that names a subagent may carry the subagent's model, so it
    // never changes the session's.
    let delegated = [
        ("UserPromptSubmit", json!({"model": "m1"})),
        ("SubagentStart", json!({"agent_id": "a1", "model": "m2"})),
        (
            "PreToolUse",
            json!({"agent_id": "a1", "model": "m2", "tool_use_id": "tu1"}),
        ),
    ];
    for (client, tree) in [("codex", codex(&delegated)), ("muse", muse(&delegated))] {
        let session = format!("/{client}/session:s1");
        assert_eq!(attrs(&tree, &session)["model"], "m1", "{client}");
        let agent = format!("{session}/turn:p1/agent:a1");
        assert_eq!(attrs(&tree, &agent).get("model"), None, "{client}");
    }

    // The main thread naming another model is one upsert of the session row;
    // naming the same one again is none.
    let mut adapter = CodexAdapter::new();
    let mut tree = Tree::new(&[adapter.client()], STALE_AFTER, 0);
    let steps = [
        ("UserPromptSubmit", json!({"model": "m1"})),
        ("PreToolUse", json!({"model": "m1", "tool_use_id": "tu1"})),
        ("PreToolUse", json!({"model": "m3", "tool_use_id": "tu2"})),
    ];
    let mut changes = Vec::new();
    for (n, step) in steps.iter().enumerate() {
        feed(&mut tree, &mut adapter, step, n as Millis * 1000);
        changes.push(tree.take_changes());
    }
    assert_eq!(changes[1], [], "the same model again");
    match changes[2].as_slice() {
        [Change::Upsert(row)] => {
            assert_eq!(row.id, "/codex/session:s1");
            assert_eq!(row.attrs["model"], "m3");
        }
        other => panic!("expected one upsert: {other:?}"),
    }
}

/// A type named after its agent opened reaches subscribers as one upsert of
/// that row, and hearing it again publishes nothing.
#[test]
fn a_type_named_later_is_published_once() {
    let mut adapter = GrokAdapter::new();
    let mut tree = Tree::new(&[adapter.client()], STALE_AFTER, 0);
    let steps = [
        ("UserPromptSubmit", json!({})),
        ("SubagentStart", json!({"agent_id": "c1"})),
        (
            "UserPromptSubmit",
            grok_child(json!({"agent_type": "explore"})),
        ),
        (
            "PreToolUse",
            grok_child(
                json!({"agent_type": "explore", "tool_name": "read_file", "tool_use_id": "tu1"}),
            ),
        ),
    ];
    let mut changes = Vec::new();
    for (n, step) in steps.iter().enumerate() {
        feed(&mut tree, &mut adapter, step, n as Millis * 1000);
        changes.push(tree.take_changes());
    }
    let agent = "/grok/session:s1/turn:p1/agent:c1";
    match changes[2].as_slice() {
        [Change::Upsert(row)] => {
            assert_eq!(row.id, agent);
            assert_eq!(row.attrs["agent_type"], "explore");
        }
        other => panic!("expected one upsert: {other:?}"),
    }
    assert_eq!(changes[3], [], "nothing changed");
}
