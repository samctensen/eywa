mod common;

use common::*;
use eywa::row::Change;
use eywa::tree::{Event, Outcome, STALE_AFTER, State};
use serde_json::json;

#[test]
fn state_rolls_up_from_the_deepest_open_node() {
    let mut t = tree();
    assert_eq!(t.state("/claude"), Some(State::Idle));

    open(&mut t, "/claude/session:s1", 1);
    assert_eq!(
        t.state("/claude"),
        Some(State::Idle),
        "an open session isn't work"
    );

    open(&mut t, "/claude/session:s1/turn:p1/agent:a1", 2);
    assert_eq!(t.state("/"), Some(State::Working));

    open(&mut t, "/claude/session:s1/turn:p1/agent:a1/request:r1", 3);
    open(&mut t, "/claude/session:s2/turn:p2/request:r2", 4);
    assert_eq!(t.state("/"), Some(State::NeedsInput));
    assert_eq!(
        t.cause("/").as_deref(),
        Some("/claude/session:s1/turn:p1/agent:a1/request:r1"),
        "the cause is the deepest responsible node"
    );
}

#[test]
fn closing_rules() {
    let mut t = tree();
    open(&mut t, "/claude/session:s1/turn:p1/agent:a1/request:r1", 1);
    open(&mut t, "/claude/session:s1/turn:p1/agent:a2", 1);

    close(
        &mut t,
        "/claude/session:s1/turn:p1/agent:a2",
        Outcome::Ok,
        2,
    );
    assert_eq!(
        t.state("/claude"),
        Some(State::NeedsInput),
        "a sibling finishing doesn't clear a request"
    );

    close(&mut t, "/claude/session:s1/turn:p1", Outcome::Ok, 3);
    assert!(
        t.contains("/claude/session:s1/turn:p1/agent:a1"),
        "a turn ending keeps its subagents"
    );
    assert_eq!(
        t.attrs("/claude/session:s1").unwrap()["last_turn"]["outcome"],
        "ok"
    );

    close(
        &mut t,
        "/claude/session:s1/turn:p1/agent:a1",
        Outcome::Ok,
        4,
    );
    assert!(
        !t.contains("/claude/session:s1/turn:p1"),
        "nothing open beneath, so pruned"
    );
    assert_eq!(t.state("/claude"), Some(State::Idle));

    open(&mut t, "/claude/session:s1/turn:p2/request:r2", 5);
    close(&mut t, "/claude/session:s1", Outcome::Ok, 6);
    assert!(
        !t.contains("/claude/session:s1"),
        "a session closes everything beneath it"
    );

    // An end closes like a close but records no outcome: that follows later.
    // Until it does, the turn before no longer says how the latest one ended.
    open(&mut t, "/claude/session:s2/turn:p0", 7);
    close(&mut t, "/claude/session:s2/turn:p0", Outcome::Ok, 7);
    open(&mut t, "/claude/session:s2/turn:p1/request:r1", 7);
    open(&mut t, "/claude/session:s2/turn:p1/agent:a1", 7);
    t.apply(
        Event::End {
            id: "/claude/session:s2/turn:p1".into(),
        },
        8,
    );
    assert_eq!(
        t.state("/claude/session:s2"),
        Some(State::Working),
        "the subagent keeps running"
    );
    assert!(
        !t.contains("/claude/session:s2/turn:p1/request:r1"),
        "the turn's own request closed with it"
    );
    assert!(
        t.attrs("/claude/session:s2")
            .unwrap()
            .get("last_turn")
            .is_none()
    );

    // A retraction closes the same way and says nothing about any outcome.
    open(&mut t, "/claude/session:s3/turn:p0", 9);
    close(&mut t, "/claude/session:s3/turn:p0", Outcome::Cancelled, 9);
    open(&mut t, "/claude/session:s3/turn:running/request:r1", 10);
    t.apply(
        Event::Retract {
            id: "/claude/session:s3/turn:running".into(),
        },
        11,
    );
    assert_eq!(t.state("/claude/session:s3"), Some(State::Idle));
    assert_eq!(
        t.attrs("/claude/session:s3").unwrap()["last_turn"]["outcome"],
        "cancelled"
    );
}

#[test]
fn silence_makes_work_unknown_never_idle() {
    let mut t = tree();
    open(&mut t, "/claude/session:idle", 0);
    open(&mut t, "/claude/session:s1/turn:p1", 0);
    assert_eq!(t.next_deadline(), Some(STALE_AFTER));

    t.expire(STALE_AFTER);
    assert_eq!(t.state("/claude/session:s1"), Some(State::Unknown));
    assert_eq!(t.state("/claude/session:idle"), Some(State::Idle));

    open(
        &mut t,
        "/claude/session:s1/turn:p1/agent:a1",
        STALE_AFTER + 1,
    );
    assert_eq!(
        t.state("/claude"),
        Some(State::Working),
        "any event reconfirms"
    );
}

#[test]
fn process_nodes_hold_unknown_until_claimed_or_exited() {
    let mut t = tree();
    open(&mut t, "/claude/process:42", 1);
    open(&mut t, "/claude/process:43", 1);
    assert_eq!(t.state("/claude"), Some(State::Unknown));

    open_with(&mut t, "/claude/session:s1", json!({"pid": 42}), 2);
    open(&mut t, "/claude/session:s1/turn:p1", 2);
    assert!(
        !t.contains("/claude/process:42"),
        "a session claims its process"
    );

    assert_eq!(
        t.process_exited(42, 3),
        vec!["/claude/session:s1".to_string()]
    );
    assert!(t.process_exited(43, 3).is_empty());
    assert_eq!(t.state("/claude"), Some(State::Idle));
}

#[test]
fn lost_and_sync() {
    let mut t = tree();
    open(&mut t, "/claude/session:s1/turn:p1", 1);
    t.apply(
        Event::Lost {
            scope: "/claude/session:s1".into(),
        },
        2,
    );
    assert_eq!(t.state("/claude"), Some(State::Unknown));

    let nodes = vec![("/claude/session:s1/turn:p2".into(), Default::default())];
    t.apply(
        Event::Sync {
            scope: "/claude/session:s1".into(),
            nodes,
        },
        3,
    );
    assert!(!t.contains("/claude/session:s1/turn:p1"));
    assert_eq!(t.state("/claude"), Some(State::Working));
}

#[test]
fn malformed_paths_are_ignored() {
    let mut t = tree();
    open(&mut t, "/claude/turn:p1", 1);
    open(&mut t, "/gemini/session:s1", 1);
    close(&mut t, "/claude/session:nope", Outcome::Ok, 1);
    assert_eq!(t.state("/"), Some(State::Idle));
    assert_eq!(t.rows().len(), 2);
}

#[test]
fn client_row_has_the_wire_shape() {
    let mut t = tree();
    open_with(
        &mut t,
        "/claude/session:s1/turn:p1/request:r1",
        json!({"reason": "approval"}),
        1,
    );
    open(&mut t, "/claude/session:s2/turn:p2", 2);
    open(&mut t, "/claude/session:s3", 3);
    let row = t.rows().into_iter().find(|r| r.id == "/claude").unwrap();
    assert_eq!(
        serde_json::to_value(&row).unwrap(),
        json!({
            "id": "/claude", "kind": "client", "state": "needs_input",
            "since": "1970-01-01T00:00:00.001Z", "reason": "approval",
            "cause": "/claude/session:s1/turn:p1/request:r1",
            "counts": {"sessions": 3, "needs_input": 1, "working": 1, "unknown": 0, "idle": 1},
            "attrs": {"observes": {"agents": true, "approvals": "actor", "questions": true,
                                   "mcp_input": true, "interrupts": "inferred"}}
        })
    );
}

#[test]
fn changes_report_only_what_changed() {
    let mut t = tree();
    assert_eq!(t.take_changes().len(), 2, "the machine and client rows");
    assert!(t.take_changes().is_empty());

    open(&mut t, "/claude/session:s1/turn:p1", 1);
    t.take_changes();
    close(&mut t, "/claude/session:s1/turn:p1", Outcome::Ok, 2);
    let changes = t.take_changes();
    assert!(changes.contains(&Change::Remove("/claude/session:s1/turn:p1".into())));

    // An attribute named after the node opened is one upsert of that row;
    // opening it again with what it already has publishes nothing.
    let agent = "/claude/session:s1/turn:p2/agent:a1";
    open(&mut t, agent, 3);
    t.take_changes();
    let typed = json!({"agent_type": "Explore"});
    open_with(&mut t, agent, typed.clone(), 4);
    match t.take_changes().as_slice() {
        [Change::Upsert(row)] => assert_eq!(serde_json::to_value(&row.attrs).unwrap(), typed),
        other => panic!("expected one upsert: {other:?}"),
    }
    open_with(&mut t, agent, typed, 5);
    open(&mut t, agent, 6);
    assert!(t.take_changes().is_empty());
}
