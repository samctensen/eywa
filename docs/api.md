# Eywa API v1

This is the contract for programs that read from Eywa: status lights, menu bar
apps, Stream Deck plugins, scripts. It covers how to connect, what to send,
every message and field Eywa sends back with its type and when it's present,
and how to keep a correct picture as things change.

The same contract is published as a JSON Schema in
[`api-v1.schema.json`](api-v1.schema.json), for validating messages or
generating types.

## Connect

Eywa listens on a Unix stream socket:

```
~/Library/Application Support/eywa/api.sock
```

If `EYWA_DIR` is set in Eywa's environment, the socket is
`$EYWA_DIR/api.sock` instead. The directory is readable only by your user, so
only your own programs can connect.

Each connection carries one request:

1. Connect.
2. Write one request: a JSON object on a single line, ending in `\n`, at most
   64 KiB.
3. Read messages: one JSON object per line, UTF-8.

If the socket doesn't exist or refuses the connection, Eywa isn't running.

## Requests

```jsonl
{"v":1,"op":"subscribe","path":"/","depth":1}
{"v":1,"op":"get","path":"/claude","depth":2}
{"v":1,"op":"subscribe","view":"requests"}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `v` | integer | required | Always `1`. |
| `op` | string | required | `get`: send one snapshot, then close the connection. `subscribe`: send a snapshot, then every change until you disconnect. `stop`: shut Eywa down, as `eywa stop` does. |
| `view` | string | `"tree"` | `tree`: the nodes at and below `path`. `requests`: every open request across all clients, as a flat list. |
| `path` | string | `"/"` | Tree view only. The node to start at, for example `/claude` or `/claude/session:7f3a`. |
| `depth` | integer ≥ 0 | `1` | Tree view only. How many levels below `path` to include. `0` sends the node alone. |

Every row summarizes its whole subtree, whatever depth you ask for. Depth only
limits which rows you receive, so ask for the shallowest selection that answers
your question:

| You want | Send |
| --- | --- |
| One light: is anything waiting on me or working? | `{"v":1,"op":"subscribe","path":"/","depth":0}` |
| One light per agent app | `{"v":1,"op":"subscribe","path":"/","depth":1}` |
| A list of everything waiting on me | `{"v":1,"op":"subscribe","view":"requests"}` |
| Every session of one app | `{"v":1,"op":"subscribe","path":"/claude","depth":1}` |
| Everything | `{"v":1,"op":"subscribe","path":"/","depth":64}` |

There's no "unlimited" depth. To get everything, pass a depth deeper than any
tree, such as `64`.

## Messages

Every message has `v` (always `1`) and `type`.

```jsonl
{"v":1,"type":"snapshot","rev":41,"rows":[ … ]}
{"v":1,"type":"upsert","rev":44,"row":{ … }}
{"v":1,"type":"remove","rev":45,"id":"/claude/session:7f3a"}
{"v":1,"type":"error","code":"bad_request","message":"unknown view"}
{"v":1,"type":"stopping"}
```

| `type` | Fields | Meaning |
| --- | --- | --- |
| `snapshot` | `rev`: integer, `rows`: [Row] | Every row in your selection right now, sorted by `id`. Always the first message. |
| `upsert` | `rev`: integer, `row`: Row | A row appeared, or one of its fields changed. Replace the whole row. |
| `remove` | `rev`: integer, `id`: string | A row disappeared. |
| `error` | `code`: string, `message`: string | The request was rejected. Eywa closes the connection after it. |
| `stopping` | none | The answer to `stop`. Eywa then closes every connection and exits. |

`rev` is the revision: an integer that goes up by one with every change Eywa
publishes, anywhere in the tree. A `subscribe` sends every change newer than
its snapshot, in order, with none missed between the two. A filtered subscription sees gaps in `rev`, because it doesn't receive
changes outside its selection. `rev` starts again from zero when Eywa
restarts, so never compare revisions from different connections.

| Error `code` | Cause |
| --- | --- |
| `bad_request` | The request isn't a JSON object, or a field is invalid. `message` says which. |
| `unsupported_version` | `v` isn't `1`. |

A path that's well formed but names nothing returns an empty snapshot, not an
error. For a subscription, rows appear once that node exists.

## Rows

A row is the published view of one node in Eywa's tree.

```json
{
  "id": "/claude",
  "kind": "client",
  "state": "needs_input",
  "since": "2026-11-18T20:41:07.000Z",
  "reason": "approval",
  "cause": "/claude/session:7f3a/turn:12/agent:a2/request:t9",
  "counts": { "sessions": 4, "needs_input": 1, "working": 2, "unknown": 0, "idle": 1 },
  "attrs": { "observes": { "agents": true, "approvals": "actor", "questions": true, "mcp_input": true, "interrupts": "inferred" } }
}
```

| Field | Type | Present | Meaning |
| --- | --- | --- | --- |
| `id` | string | always | The node's path. See [Ids](#ids). |
| `kind` | string | always | What the node is. See [Kinds](#kinds). |
| `state` | string | always | `needs_input`, `working`, `unknown` or `idle`. See [States](#states). |
| `since` | string | always | When `state` last changed: RFC 3339, UTC, with milliseconds, for example `2026-11-18T20:41:07.000Z`. |
| `reason` | string | when `state` is `needs_input` | Why you're needed: `approval`, `question` or `mcp_input`. |
| `cause` | string | when `state` isn't `idle` | The id of the node responsible for the state: the deepest one, then the one open longest. It can be the row's own id. |
| `counts` | object | on `machine` and `client` rows | The open sessions beneath this row, in total and by state. See [Counts](#counts). |
| `attrs` | object | always, possibly empty | Facts about the node. See [Attributes](#attributes). |

Optional fields are left out when they don't apply; they're never `null`.

### Ids

An id is the node's path from the machine:

```
/                                                     machine
/claude                                               client
/claude/process:4321                                  process
/claude/session:7f3a                                  session
/claude/session:7f3a/turn:12                          turn
/claude/session:7f3a/turn:12/agent:a2                 agent
/claude/session:7f3a/turn:12/agent:a2/agent:b7        agent started by an agent
/claude/session:7f3a/turn:12/agent:a2/request:t9      request
/claude/session:7f3a/turn:12/request:t4               request from the main thread
```

- The machine is `/`. A client is `/<name>`: `claude`, `codex`, `grok` or
  `muse`.
- Every deeper segment is `<kind>:<name>`. The name is the client's own id
  for the session, turn, agent or call. It never contains `/`, can contain
  `:`, and is at most 128 characters.
- A row's parent is its id up to the last `/`, or `/` for a client.

Treat ids as opaque keys. Use `kind` rather than parsing the id, and don't
assume a row's parent is in your map: your depth may leave it out.

### Kinds

| `kind` | Parent | What it is |
| --- | --- | --- |
| `machine` | none | Your Mac. Always present. |
| `client` | machine | An agent app. One row for each supported app, always present, even when it isn't installed. |
| `process` | client | A running agent process that hasn't reported a session yet, usually because it started before Eywa. Always `unknown`. It goes away when one of its sessions reports or the process exits. |
| `session` | client | One conversation. |
| `turn` | session | One prompt and the work done in response to it. |
| `agent` | turn or agent | A subagent. It can outlive the turn that started it. |
| `request` | turn or agent | Something waiting on you: a tool approval, a question or an MCP input request. A request under a turn comes from the main thread; one under an agent, from that subagent. |

A turn that has ended stays in the tree while a subagent it started keeps
running; its row's state then comes from that subagent. Rows for ended nodes
are removed.

### States

A node's state is derived from its whole subtree. The first rule that matches
wins:

| `state` | Rule | Show it as |
| --- | --- | --- |
| `needs_input` | An open request exists beneath it. | Waiting on you. `reason` says why and `cause` says where. |
| `working` | An open turn or subagent exists beneath it. | Busy. |
| `unknown` | Something is open that Eywa can't confirm: it started before Eywa did, or an open turn, subagent or request has heard nothing for an hour. | Can't tell. Not idle. |
| `idle` | None of the above. | Nothing to do. |

Elapsed time never makes anything `idle`, so an `idle` row really is idle,
within what that client lets Eywa see. Check the client's
[`observes`](#client) record to find out what that is.

### Counts

`counts` appears on `machine` and `client` rows.

| Field | Type | Meaning |
| --- | --- | --- |
| `sessions` | integer | Open sessions beneath the row. `0` means the app isn't running any. |
| `needs_input` | integer | Of those, how many are `needs_input`. |
| `working` | integer | How many are `working`. |
| `unknown` | integer | How many are `unknown`. |
| `idle` | integer | How many are `idle`. |

The four states always add up to `sessions`. `process` rows aren't sessions
and aren't counted, so a client can be `unknown` with `sessions` at `0`: an
agent process is running that Eywa hasn't heard from yet.

### Attributes

Each kind has its own attributes. Any of them can be missing when the client
doesn't report it.

#### Machine, process

No attributes yet. `attrs` is `{}`.

#### Client

| Attribute | Type | Meaning |
| --- | --- | --- |
| `observes` | object | What this client lets Eywa see, so you can tell "really idle" from "can't see this". Always present. |
| `observes.agents` | boolean | Subagents are visible. |
| `observes.approvals` | string | `actor`: approvals are attributed to the subagent or main thread that asked. `turn`: only to the turn. `none`: approvals aren't visible. |
| `observes.questions` | boolean | Questions to you are visible. |
| `observes.mcp_input` | boolean | MCP input requests are visible. |
| `observes.interrupts` | string | `hook`: the client reports interrupted turns. `inferred`: Eywa infers them from what happens next. |

What each client reports today:

| Client | `agents` | `approvals` | `questions` | `mcp_input` | `interrupts` |
| --- | --- | --- | --- | --- | --- |
| `claude` | `true` | `actor` | `true` | `true` | `inferred` |
| `codex` | `true` | `actor` | `true` | `false` | `hook` |
| `grok` | `true` | `turn` | `true` | `false` | `hook` |
| `muse` | `true` | `actor` | `false` | `false` | `inferred` |

#### Session

| Attribute | Type | Meaning |
| --- | --- | --- |
| `cwd` | string | The session's working directory, as an absolute path. |
| `pid` | integer | The agent process the session runs in. Several sessions can share one process. |
| `surface` | string | Where the session runs: `cli`, `desktop` or `ide`. Claude Code entry points Eywa doesn't recognize, such as SDK ones, pass through unchanged. |
| `last_turn` | object | How the session's most recent turn ended. Missing until a turn ends, and while Eywa is still learning how the latest turn ended: after a Claude Code interrupt it follows the `idle` state by up to two seconds. |
| `last_turn.outcome` | string | `ok`, `error` or `cancelled` (interrupted). |
| `last_turn.ended_at` | string | When it ended, in the same format as `since`. |
| `model` | string | The model the session's main thread runs on, as the client names it, for example `claude-opus-5-5`. Missing until the client names it. Eywa doesn't translate names, so they aren't comparable across clients. |

Where each client names the model:

| Client | `model` |
| --- | --- |
| `claude` | When the session starts, resumes, clears or compacts, if Claude Code states it. A model you switch to mid-session isn't seen until the next of those. |
| `codex` | On every main-thread event, so a switch shows at the next one. |
| `grok` | When the session starts. A later switch isn't seen. |
| `muse` | On every main-thread event, so a switch shows at the next one. |

A subagent can run on another model. Eywa doesn't publish that: an event
that names a subagent never changes the session's `model`.

Session titles aren't published, because clients generate them from your
prompts.

#### Turn

| Attribute | Type | Meaning |
| --- | --- | --- |
| `origin` | string | Claude Code only. `person`: you sent the prompt. `machine`: something else did, for example a background task reporting back. |

#### Agent

| Attribute | Type | Meaning |
| --- | --- | --- |
| `agent_type` | string | The kind of subagent, as the client names it: a built-in type such as `Explore`, or the name of an agent you defined. Missing until the client names it, so it can arrive in a later `upsert` of the row. |

Where each client names it:

| Client | `agent_type` |
| --- | --- |
| `claude` | When the subagent starts or resumes. A subagent Eywa first hears of through its work, for example after Eywa restarts, has none until it starts again. |
| `codex` | On everything the subagent does: its role. |
| `grok` | From the subagent's own hooks, not from the one that starts it, so it arrives in an `upsert` just after the row appears, once the subagent first acts. |
| `muse` | Never. Muse doesn't name a subagent's type when it starts or works. |

#### Request

| Attribute | Type | Meaning |
| --- | --- | --- |
| `reason` | string | `approval`, `question` or `mcp_input`. Always present. |
| `prompted` | boolean | `true` once the client confirms it's showing the request to you. Some clients run an automatic reviewer before asking a person, so an approval without it may be decided without you. |
| `tool_name` | string | The tool the request is for, as the client names it: for an approval, the tool waiting to run, such as `Bash` or `mcp__github__create_issue`; for a question, the client's question tool. Missing when the client doesn't say which tool; Eywa never guesses. |
| `mcp_server` | string | On `mcp_input` requests: the MCP server asking. Missing when the client doesn't say. |

These are names only. A tool's input never reaches Eywa.

Where each client names them:

| Client | `tool_name` | `mcp_server` |
| --- | --- | --- |
| `claude` | Approvals and questions, except an approval Claude announces only as a notification, such as a sandbox network request, and a request Eywa learns of only from Claude's session status, for example after Eywa restarts. | MCP input requests, with the same exception for session status. |
| `codex` | Approvals and questions. | MCP input isn't visible. |
| `grok` | Questions only. Grok announces an approval without naming its tool. | MCP input isn't visible. |
| `muse` | Approvals, except one Eywa learns of only from Muse's "waiting for approval" notification, for example after Eywa restarts. | MCP input isn't visible. |

A request row's top-level `reason` is only there while its state is
`needs_input`. Read `attrs.reason` instead when listing requests, since it's
there in every state.

## Subscribing correctly

1. **Keep a map of rows by `id`.** On `snapshot`, replace the whole map with its
   rows. On `upsert`, set `rows[row.id] = row`. On `remove`, delete
   `rows[id]`. That's the whole algorithm.
2. **Read the state, don't compute it.** Each row's `state`, `cause`, `reason`
   and `counts` already summarize everything beneath it, including rows outside
   your selection. A status light subscribes at depth `0` and reads one row.
3. **Read promptly.** Each subscriber has a queue of 256 messages. If you fall
   behind, Eywa closes your connection rather than slowing anything else down.
4. **When the connection closes, subscribe again.** That happens when Eywa
   restarts or when you fall behind. Wait about a second, reconnect, send the
   same request, and let the new snapshot replace everything. While you're
   disconnected, show your data as unavailable rather than as current.
5. **Expect quiet.** A subscription only sends when something in its selection
   changes, and there's no heartbeat. A closed connection is the only sign
   that Eywa stopped.
6. **Drill down with `cause`.** `cause` names the node responsible for a
   state. If it isn't in your map, `get` it with `{"v":1,"op":"get","path":
   <cause>,"depth":0}`.
7. **Treat `unknown` as unknown.** It means Eywa can't confirm what's
   happening, not that nothing is.
8. **Ignore what you don't recognize.** Skip message types, fields and
   attributes you don't know, and handle enum values you don't know. See
   [Compatibility](#compatibility).

## Compatibility

Within version 1, Eywa may add message types, fields, attributes, kinds of
node, request reasons and error codes. It won't remove or rename a field,
change a field's type or change what a state means. Anything incompatible
becomes version 2, requested with `"v":2`.

So consumers should skip unknown message types, ignore unknown fields and
attributes, and accept unknown enum values. If you generate types from the
JSON Schema, make sure its enums don't reject new values when you decode.

## Types

TypeScript definitions for the whole API:

```ts
/** Eywa API v1. */
export type Kind = "machine" | "client" | "process" | "session" | "turn" | "agent" | "request";
export type State = "needs_input" | "working" | "unknown" | "idle";
export type Reason = "approval" | "question" | "mcp_input";
export type Outcome = "ok" | "error" | "cancelled";

export interface Request {
  v: 1;
  op: "get" | "subscribe" | "stop";
  view?: "tree" | "requests"; // default "tree"
  path?: string; // default "/"
  depth?: number; // default 1
}

export type Message =
  | { v: 1; type: "snapshot"; rev: number; rows: Row[] }
  | { v: 1; type: "upsert"; rev: number; row: Row }
  | { v: 1; type: "remove"; rev: number; id: string }
  | { v: 1; type: "error"; code: string; message: string }
  | { v: 1; type: "stopping" };

export interface Row {
  id: string;
  kind: Kind;
  state: State;
  /** RFC 3339, UTC, with milliseconds. */
  since: string;
  /** Only when state is "needs_input". */
  reason?: Reason;
  /** Every state but "idle". */
  cause?: string;
  /** Machine and client rows only. */
  counts?: Counts;
  attrs: Attrs;
}

export interface Counts {
  sessions: number;
  needs_input: number;
  working: number;
  unknown: number;
  idle: number;
}

export interface Attrs {
  /** Client. */
  observes?: Observes;
  /** Session. */
  cwd?: string;
  pid?: number;
  surface?: string;
  last_turn?: { outcome: Outcome; ended_at: string };
  model?: string;
  /** Turn, Claude Code only. */
  origin?: "person" | "machine";
  /** Agent. */
  agent_type?: string;
  /** Request. */
  reason?: Reason;
  prompted?: boolean;
  tool_name?: string;
  /** Request, mcp_input only. */
  mcp_server?: string;
  [attribute: string]: unknown;
}

export interface Observes {
  agents: boolean;
  approvals: "actor" | "turn" | "none";
  questions: boolean;
  mcp_input: boolean;
  interrupts: "hook" | "inferred";
}
```

### Rust

The [`eywa-api`](../eywa-api) crate has the same types for Rust. They are the
types Eywa itself writes the API with, and they depend only on `serde` and
`serde_json`. Pin a commit:

```toml
[dependencies]
eywa-api = { git = "https://github.com/samctensen/eywa", rev = "<commit>" }
serde_json = "1"
```

Decode each line as a `Message`:

```rust
use eywa_api::Message;

match serde_json::from_str::<Message>(&line)? {
    Message::Snapshot { rows, .. } => { /* replace every row with `rows` */ }
    Message::Upsert { row, .. } => { /* replace the row with `row.id` */ }
    Message::Remove { id, .. } => { /* drop the row with `id` */ }
    _ => {} // `error`, `stopping`, or a type from a newer Eywa
}
```

`Request::subscribe(path, depth)` and `Request::requests()` build the request,
and `default_dir().join(API_SOCKET)` is the socket.

Decoding doesn't fail on what a newer Eywa adds:

- Every enum has an `Other` variant that stands in for a value this version
  doesn't know, and `Message::Other` stands in for an unknown message type.
  Neither keeps what was sent: encoding one writes `"other"`, so don't relay
  rows by decoding and re-encoding them.
- `Attrs::other` keeps the attributes this version doesn't name, and any
  documented attribute whose value it can't hold.

## Examples

### Python

Standard library only. It keeps the rows current, resubscribes whenever the
connection closes, and calls `on_change` with the full map after every
message.

```python
import json
import os
import socket
import time

DIR = os.environ.get("EYWA_DIR") or os.path.expanduser("~/Library/Application Support/eywa")
SOCKET = os.path.join(DIR, "api.sock")


def subscribe(request, on_change):
    while True:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
                sock.connect(SOCKET)
                sock.sendall((json.dumps(request) + "\n").encode())
                rows = {}
                for line in sock.makefile(encoding="utf-8"):
                    message = json.loads(line)
                    if message["type"] == "snapshot":
                        rows = {row["id"]: row for row in message["rows"]}
                    elif message["type"] == "upsert":
                        rows[message["row"]["id"]] = message["row"]
                    elif message["type"] == "remove":
                        rows.pop(message["id"], None)
                    elif message["type"] == "error":
                        raise ValueError(f"{message['code']}: {message['message']}")
                    else:
                        continue
                    on_change(rows)
        except OSError:
            pass  # Eywa isn't running, or it restarted.
        on_change(None)  # Unavailable until the next snapshot.
        time.sleep(1)


def show(rows):
    if rows is None:
        print("eywa: unavailable", flush=True)
        return
    machine = rows["/"]
    print(machine["state"], machine.get("reason", ""), machine.get("cause", ""), flush=True)


subscribe({"v": 1, "op": "subscribe", "path": "/", "depth": 0}, show)
```

### TypeScript (Node.js)

Uses the [types](#types) above.

```ts
import net from "node:net";
import os from "node:os";
import path from "node:path";
import readline from "node:readline";

const dir = process.env.EYWA_DIR ?? path.join(os.homedir(), "Library/Application Support/eywa");
const socketPath = path.join(dir, "api.sock");

/** Calls onChange with every row after each message, or null while disconnected. */
export function subscribe(request: Request, onChange: (rows: Map<string, Row> | null) => void) {
  const rows = new Map<string, Row>();
  let rejected = false;
  const socket = net.createConnection(socketPath, () => socket.write(JSON.stringify(request) + "\n"));
  const lines = readline.createInterface({ input: socket });
  lines.on("line", (line) => {
    const message = JSON.parse(line) as Message;
    switch (message.type) {
      case "snapshot":
        rows.clear();
        for (const row of message.rows) rows.set(row.id, row);
        break;
      case "upsert":
        rows.set(message.row.id, message.row);
        break;
      case "remove":
        rows.delete(message.id);
        break;
      case "error":
        rejected = true;
        console.error(`eywa: ${message.code}: ${message.message}`);
        return;
      default:
        return;
    }
    onChange(rows);
  });
  // A failed connection ends in "close", which resubscribes.
  socket.on("error", () => {});
  lines.on("error", () => {});
  socket.on("close", () => {
    onChange(null);
    if (!rejected) setTimeout(() => subscribe(request, onChange), 1000);
  });
}

subscribe({ v: 1, op: "subscribe", view: "requests" }, (rows) => {
  if (!rows) return console.log("eywa: unavailable");
  for (const row of rows.values()) console.log(row.attrs.reason, row.id);
});
```
