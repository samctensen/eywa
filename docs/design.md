# Eywa

**Design · Draft v0.2 · 18 November 2026**

Eywa is a per-user background service that watches the coding agents running on
your machine and answers one question for any program that asks: *which of my
agents needs me?*

You keep launching Claude Code, Codex, Grok Build and Muse Code the way you do today. Their hooks report
lifecycle events to Eywa. Eywa keeps one live picture of everything that is
running and publishes it over a local socket. Consumers such as the `eywa`
command line, keyboard or desk lights, a menu bar app or a Stream Deck plugin
subscribe to that picture and decide how to show it. Eywa defines the data and
the API. It has no UI and knows nothing about any consumer.

The name comes from Eywa, the great tree in Avatar that connects every living
thing on Pandora. Here every agent, session, turn and request is a node in one
tree, and each node's state sums up everything beneath it.

## First demonstration

1. Claude Code and Codex run at the same time.
2. Claude's main turn finishes while one of its subagents waits for tool approval.
3. Start `eywa watch --json`. Its first message shows `claude` as `needs_input`,
   with `cause` pointing at the subagent's request, and `codex` as `working`.
4. Approve the subagent and let it finish. Only the `claude` row changes, to
   `idle`.

## Vocabulary

| Term | Meaning |
| --- | --- |
| client | A program that runs agents: `claude`, `codex`, `grok`, `muse`. |
| session | One conversation in a client. |
| turn | One prompt and the work done in response to it. |
| agent | A subagent started inside a turn. It can outlive the turn. |
| request | Something the agent is waiting on you for: a tool approval, a question or an MCP input request. |
| hook command | `eywa hook <client>`, which each client's hooks run once per event. |
| adapter | Client-specific code inside Eywa that maps a client's hook records onto a shared per-session tracker, which turns them into Eywa events. |
| consumer | Any program that reads from Eywa's API. |

## Data model

### One tree of nodes

Eywa stores what it has observed, never conclusions. It does not store "Claude
is working." It stores "turn 12 in session 7f3a is open" and "subagent a2 has an
open approval request." Every higher-level answer is computed from that.

```
/                                  machine
└─ /claude                         client
   ├─ /claude/process:4321         running process not yet linked to a session
   └─ /claude/session:7f3a         session (attrs: cwd, pid, surface)
      └─ …/turn:12                 turn
         └─ …/agent:a2             subagent (attrs: agent_type)
            └─ …/request:t9        approval, question or MCP input request
                                   (attrs: reason, tool_name, mcp_server)
```

Each node has the same shape:

```json
{
  "id": "/claude/session:7f3a/turn:12/agent:a2/request:t9",
  "kind": "request",
  "open": true,
  "confirmed": true,
  "opened_at": "2026-11-18T20:41:07Z",
  "closed_at": null,
  "outcome": null,
  "attrs": { "reason": "approval", "tool_name": "Bash" }
}
```

The id is the node's path, so the parent is implied. `outcome` is set on close:
`ok`, `error` or `cancelled`. `confirmed` is false when no source can currently
vouch for an open node.

Node kinds fall into three groups:

| Group | Kinds | An open node means |
| --- | --- | --- |
| container | client, process, session | It exists and is alive. Not that it is busy. |
| work | turn, agent (later: tool) | The agent has the ball. |
| attention | request | You have the ball. |

### Derived state

Every node, at every level, gets a state computed from its own subtree:

| State | Rule, checked in this order |
| --- | --- |
| `needs_input` | A confirmed open request exists in the subtree. |
| `working` | A confirmed open work node exists in the subtree. |
| `unknown` | An unconfirmed open node exists in the subtree. |
| `idle` | None of the above. |

One subagent's progress can't clear a sibling's approval, because each request is
its own node. A turn ending doesn't hide a subagent that keeps running, because
the subagent is still an open node beneath it. Elapsed time alone never produces
`idle`. It can only make an open node unconfirmed.

"Is Claude running?", "is this session busy?" and "is that subagent waiting?" are
the same question asked at different depths of the tree.

**Implementation.** When a node changes, Eywa recomputes the state of that node
and each of its ancestors from their children. Trees hold a few dozen nodes, so
this costs nothing and leaves no counters to drift. Eywa publishes a change only
for rows whose published fields actually changed, so detail deep in a session
doesn't create traffic at the client level.

### Closing rules

- Closing a node closes its open **requests**. A request can't outlive the actor
  that asked for it.
- Closing a turn does **not** close its subagents. Background subagents really do
  outlive their turn, and the first demonstration depends on that.
- Closing a session, or the session's process exiting, closes everything beneath it.

### Clients that report less

Clients differ in what they report. A client that only reports turn start and end
produces shallower trees, and the same rules still work. Each client row publishes
an `observes` record, so a consumer can tell "really idle" from "can't see this":

```json
{ "id": "/grok", "attrs": { "observes": { "agents": true, "approvals": "turn", "questions": true, "mcp_input": false, "interrupts": "hook" } } }
```

| Field | Meaning |
| --- | --- |
| `agents` | Subagents are visible. |
| `approvals` | `actor`: attributed to the subagent or main thread that asked. `turn`: only to the turn. `none`: not visible. |
| `questions` | Questions to the user are visible. |
| `mcp_input` | MCP input requests are visible. |
| `interrupts` | `hook`: the client reports them. `inferred`: Eywa infers them from what happens next. |

## Event flow

```
 Claude Code, Codex, ─┐
 Grok, Muse hooks ────┴─► eywa hook <client>   parses the client's payload and environment,
                                 │              keeps allowed fields, hashes tool input,
                                 ▼  ingest.sock sends one datagram, exits
                          adapter, per client   maps events onto a per-session tracker
                                 │
 process table ─► liveness ──────┤
 Claude session files ─► watch ──┤  open · close · attr · lost · sync
                                 ▼
                                core            tree → derived states → changed rows
                                 │
                                 ▼  api.sock
           eywa status / watch · status lights · menu bar (requests view)
```

### Capture

Each client's hook configuration runs `eywa hook <client>`. Where a client can
run a command with no shell (Claude Code and Grok Build, through `args`),
`eywa setup` writes that form; Codex and Muse Code run the command through a
shell. Run with no arguments and piped input, `eywa` exits 0 silently, so a
client that ignores `args` can't fail a hook. The hook command
parses the client's payload and hook environment into one record shape, keeps only
the allowed fields, sends them as one datagram to Eywa's ingest socket and exits.
If Eywa isn't running, it exits immediately with status 0. It never prints: every
client feeds some hook output back to the agent. The agent never waits on Eywa.

| Client | Payload | Environment |
| --- | --- | --- |
| Claude Code | snake_case; `prompt_id` is the turn; `agent_id` inside subagents | `CLAUDE_PID` (the process), `CLAUDE_CODE_ENTRYPOINT` (the surface) |
| Codex | snake_case; `turn_id`; `agent_id` inside subagents | none |
| Grok Build | camelCase fields; snake_case `hookEventName`, mapped to Claude-style names; `subagentId` names a child session | `GROK_HOOK_EVENT` |
| Muse Code | snake_case; `turn_id`; `child_session_id` on subagent events | `MUSE_SESSION_ID` (the root session), `MUSE_THREAD_ID` (the acting thread) |

Grok also runs Claude Code's hooks, with Grok's payload. When `GROK_HOOK_EVENT` is
set, `eywa hook claude` relabels the record as `grok`. The copy that arrives
through Grok's own hook file is harmless, because every adapter step is idempotent.

The hook command does no mapping. Mapping needs memory across events, such as
which tool call an approval belongs to and whether the previous turn is still
open, so it runs in the adapter inside Eywa.

**Allowed fields:** event name, session id, turn or prompt id, agent id and type,
tool use id (and the ids in a batch), tool name, notification type, source,
elicitation id, MCP server name, `StopFailure`'s error code, background
task ids, types and statuses, `cwd`, the transcript path and the client's
timestamp. A `reason` is kept only when it is `channel_closed` or `shutdown`,
which Grok's `Stop` sends at session end; any other value is dropped, because
some events put free text there, such as why Claude Code's auto mode denied a
tool call. Prompts, responses, commands, tool arguments and tool output never
cross the socket or touch disk. Alongside ids and `cwd`, Eywa publishes tool
names, subagent types and MCP server names: each on the request or agent it
describes, and only where the client states it for that node. Eywa never fills
one in from a guess, such as the call an approval is matched to.

**Tool input digest.** To match an approval with its tool call, the hook command
computes SHA-256 over the tool name and the tool input re-serialized with sorted
keys, and sends the first 16 bytes as hex. The input itself stays in the hook
command. Codex adds the model's justification (`description`) to a shell
command's input on approval requests only, so it is left out of Codex digests.

**Process id.** Claude Code passes it as `CLAUDE_PID`. For other clients, the hook
command walks up its parent processes to the client's process.

### Processes

Each client has a process rule: how its executable is named, and whether its
processes run one session or host many.

| Client | Executable | Hosting |
| --- | --- | --- |
| `claude` | `claude`, also matched as the launch name, since the native install runs `~/.local/bin/claude` → `…/versions/<version>` | One session per process, in the CLI and in Desktop |
| `codex` | `codex` | Host: the CLI and the desktop app both run hooks from a shared app server |
| `grok` | `grok` | Host when run as `grok agent` (Grok.app's pool) |
| `muse` | `muse-bin-*`, behind a launcher script | Host for `serve` and `acp` |

A host's exit closes every session it ran, but a running host is not evidence of
an unreported session. Only one-session processes stand for unreported sessions at
startup (see Recovery).

### Normalized events

Adapters produce seven event types. The core knows nothing about any specific
client.

| Event | Effect |
| --- | --- |
| `open {node}` | A node appeared. |
| `close {id, outcome}` | A node ended: `ok`, `error` or `cancelled`. |
| `end {id}` | A node ended and its outcome isn't known yet. It closes as `close` does. A turn's `end` clears its session's `last_turn`; the outcome follows as an `attr`. |
| `retract {id}` | A node closes without having ended: it stood in for another node, or only what is beneath it is running. It closes as `close` does and leaves its session's `last_turn` as it was. |
| `attr {id, attrs}` | A node's attributes changed. |
| `lost {scope}` | The open nodes in `scope` become unconfirmed. |
| `sync {scope, nodes}` | Authoritative replacement of a subtree, used for recovery. Eywa diffs it against what it has and publishes the changes. |

Any event that mentions a node also confirms that node and its ancestors. An event
for a node Eywa hasn't seen opens it implicitly, so a session or turn already
running when Eywa starts shows as `working` from its first tool event.

**Order.** Synchronous hooks block the agent, so one thread's events arrive in
order; only different threads interleave. Activity under a closed node's own id
therefore reopens it: a blocked `Stop` continues its turn, a resumed subagent runs
again, and a Codex subagent runs several turns. An ended session reopens only on a
new `SessionStart` or prompt, since a resumed session reuses its id. Any other
record for an ended session is ignored.

### The session tracker

Every adapter maps its client's events onto one tracker per session:

- **Turns.** One root turn is open at a time. A new prompt with a different id
  means the open turn was interrupted, and it closes as `cancelled`; the same id
  (steering, a repeated prompt) only confirms it.
- **Subagents** attach to the root turn that started them, whatever turn ids their
  own events carry, and keep running when that turn ends. Their activity after
  that doesn't reopen the turn (it is `retract`ed if opening the subagent brought
  it back) and says nothing about how it ended. A subagent's type, once a hook
  names it, stays with it when it runs again.
- **Tool calls** are remembered per actor (the main thread or a subagent) with
  their input digest.
- **Requests.** No client puts a call id on its approval request, so an approval
  is keyed by the actor's newest open call with the same input digest, else by its
  only open call, else by the digest. The call finishing closes it. A new tool call
  from the same actor also answers it, but only once it has been open for a
  second: parallel calls run their hooks together, and a sibling's `PreToolUse`
  can arrive just after the approval request. A request names its tool only when
  the hook that opens it does; the call it is keyed to never supplies one.

## Adapters

Hook behavior below comes from the briefings in `docs/research/`, read on 18
November 2026. Items marked **verify** aren't documented and need a recorded
session to confirm.

### Claude Code

| Hook | Eywa event |
| --- | --- |
| `SessionStart` | `open` session. A different session from the same process (`/clear`, resuming in a running process, a fork) ends the old one. |
| `UserPromptSubmit` | `open` turn, keyed by `prompt_id`. `source` sets the turn's `origin`: `person` for `user` and `sdk`, `machine` otherwise, for example a background task reporting back. |
| `PreToolUse` | Remember the call. `AskUserQuestion` opens a `question` request keyed by `tool_use_id`, with that `tool_name`. |
| `PermissionRequest` | `open` an `approval` request under the asking actor, with the `tool_name` the hook names. |
| `PostToolUse`, `PostToolUseFailure` | `close` the call's request. An MCP tool finishing closes the actor's `mcp_input` requests. |
| `PostToolBatch` | `close` the requests of every call in the batch: they are resolved. |
| `PermissionDenied` | `close` the call's request as `cancelled`. Auto mode only; a person's denial sends nothing. |
| `Elicitation` / `ElicitationResult` | `open` / `close` an `mcp_input` request, keyed by `elicitation_id`, or by the server name when there is none. It carries the server name as `mcp_server`. |
| `Notification` | `idle_prompt` closes the open turn as `cancelled`. `permission_prompt` marks open approvals `prompted`, or opens one if none is open (sandbox network prompts), which names no tool. `elicitation_complete` and `elicitation_response` close `mcp_input` requests. |
| `SubagentStart` | `open` (or reopen) the agent, keyed by `agent_id`, under its original turn, with its `agent_type`. Only this hook sets the type: `SubagentStop` can name the session's agent instead, for Claude's internal agents. |
| `SubagentStop`, `TeammateIdle` | `close` the agent. |
| `Stop` | `close` turn as `ok`. Reconcile agents against `background_tasks`, whose ids are agent ids: unlisted agents close unless waiting on the user, and a list with an unreadable entry is ignored. `SubagentStop` carries the same list. |
| `StopFailure` | `close` turn as `error`, with the error code. |
| `SessionEnd` | `close` session. |

Claude has no interrupt hook. An interrupted turn closes on the next prompt, an
`idle_prompt` notification, the session registry going idle (see Recovery), or
`SessionEnd`. The registry also reports a granted approval, which no hook does.
A request the registry opens names no tool or server: it says only what kind
of input Claude waits for.

### Codex

`session_id` names the root thread for a whole agent tree. Inside a subagent,
`agent_id` names the subagent and `turn_id` is the subagent's own turn.

| Hook | Eywa event |
| --- | --- |
| `SessionStart` | `open` session. Root only, just before the first turn. |
| `UserPromptSubmit` | Root: `open` turn keyed by `turn_id`. Subagent: confirm the agent. |
| `PreToolUse` | Remember the call. `request_user_input` opens a `question` (**verify** the tool name), with that `tool_name`. |
| `PermissionRequest` | `open` an `approval` request under the actor, with the `tool_name` the hook names. May also fire when Codex's automatic reviewer will decide. |
| `PostToolUse` | `close` the call's request. Fires only when the tool succeeds. |
| `SubagentStart` / `SubagentStop` | `open` / `close` the agent under the running root turn. `SubagentStop` fires after each of the subagent's turns; it can run again. Every hook from inside a subagent carries its role as `agent_type`. |
| `Stop` | Root only: `close` turn as `ok`. |
| `Interrupt` | `close` turn as `cancelled`. |
| `SessionEnd` | `close` session. Fires on archive, delete, normal exit, or after 30 idle minutes with no connected client. |

There is no `StopFailure`; a failed turn closes on the next prompt, `SessionEnd`
or staleness. MCP input requests have no hook. Codex skips hooks until you trust
them in `/hooks`, and keys that trust by position (file, event, group, handler),
so `eywa setup` updates its entry where it sits and only ever appends new ones.

### Grok Build

A subagent is a child session: `SubagentStart` names it, and its own events carry
its session id. The adapter maps each child to the root session that spawned it,
flattening nested subagents under the root turn.

| Hook | Eywa event |
| --- | --- |
| `UserPromptSubmit` | Root: `open` turn keyed by `promptId`. Child: confirm the agent. |
| `PreToolUse` | Remember the call. `ask_user_question` and `AskQuestion` open a `question`. |
| `Notification` | `permission_prompt` opens an `approval` under the turn, keyed by the newest call. `idle_prompt` closes the open turn as `cancelled`. |
| `PostToolUse`, `PostToolUseFailure` | `close` the call's request. |
| `PermissionDenied` | `close` the call's request as `cancelled`. |
| `SubagentStart` / `SubagentStop` | `open` / `close` the child as an agent. Its type comes from the child's own hooks, whose `subagentType` is its own: a `SubagentStart` raised in a child, or in a child Eywa never saw start and takes for a root, may name either agent. |
| `Stop` | `close` turn as `ok` (`channel_closed` and `shutdown` close it as `cancelled`). Reconcile subagent tasks in `backgroundTasks`. |
| `StopCancelled` | Root: `close` turn as `cancelled` (an interrupt, or a rejected or dismissed approval). Child: `close` the agent. |
| `StopFailure` | Root: `close` turn as `error`. Child: `close` the agent. |
| `SessionEnd` | Root: `close` session. Child: `close` the agent. |

Approvals carry no ids and no tool name, so they attach to the turn and name no
tool: the newest call they are keyed to is only a guess. Tool events carry no
prompt id, so the root's tool activity belongs to the open turn. MCP input
requests aren't reported. Hooks in an untrusted folder are skipped silently.

### Muse Code

Records arrive with the root session as `session_id` and, inside a subagent thread,
that thread as `agent_id`, both from the hook environment.

| Hook | Eywa event |
| --- | --- |
| `UserPromptSubmit` | Main thread: `open` turn keyed by `turn_id`. Subagent thread: confirm the agent. |
| `PreToolUse` | Remember the call. |
| `PermissionRequest` | `open` an `approval` request under the actor, with the `tool_name` the hook names. |
| `Notification` (`permission_prompt`) | Mark open approvals `prompted`, or open one if none is open, which names no tool. |
| `ToolUseStart` | `close` the call's approval: it was granted. |
| `PostToolUse`, `PostToolUseFailure` | `close` the call's request. |
| `SubagentStart` / `SubagentStop` | `open` / `close` the child thread as an agent. |
| `Stop` | Main thread: `close` turn as `ok`. Subagent thread: confirm the agent. |
| `StopFailure` | `close` the turn, or the agent, as `error`. |
| `SessionEnd` | Main thread: `close` session. |

There is no interrupt hook; an interrupted turn closes on the next prompt or
staleness. A question tool hasn't been identified (**verify**). Muse names a
subagent's type only on `PostToolBatch` and `StopFailure`, so agents carry no
`agent_type`. One wrong-typed field disables every hook in Muse's settings
file, so `eywa setup` writes exact JSON and seeds a new file with
`schema_version`.

## Local API v1

### Sockets

Eywa listens on two Unix sockets in `~/Library/Application Support/eywa/`. The
directory is readable only by the current user.

- `ingest.sock`: datagram socket for the hook command. One JSON record per datagram.
- `api.sock`: stream socket for consumers. One UTF-8 JSON object per line.

### Requests

Consumers select part of the tree by path and depth. Depth counts levels below
the path: `0` means only that node.

```jsonl
{"v":1,"op":"subscribe","path":"/","depth":1}
{"v":1,"op":"get","path":"/claude","depth":2}
{"v":1,"op":"get","view":"requests"}
```

| Operation | Behavior |
| --- | --- |
| `get` | Send one snapshot, then close the connection. Used by `eywa status` and scripts. |
| `subscribe` | Send a snapshot, then stream changes until the consumer disconnects. |

| View | Contents |
| --- | --- |
| `tree` (default) | Nodes under `path`, down to `depth`. |
| `requests` | Every open request across all clients, as a flat list. This is the direct answer to "which agents need me?" |

### Rows

Every row summarizes its entire subtree, whatever depth the consumer asked for.
Depth only limits which rows are sent.

```json
{
  "id": "/claude",
  "kind": "client",
  "state": "needs_input",
  "since": "2026-11-18T20:41:07Z",
  "reason": "approval",
  "cause": "/claude/session:7f3a/turn:12/agent:a2/request:t9",
  "counts": { "sessions": 4, "needs_input": 1, "working": 2, "unknown": 0, "idle": 1 },
  "attrs": { "observes": { "agents": true, "approvals": "actor", "questions": true, "mcp_input": true, "interrupts": "inferred" } }
}
```

- `state` is the derived state. `since` is when it last changed.
- `cause` is the deepest node responsible for the current state. One `get` on it
  answers "which session, which subagent and why."
- `counts` appears on machine and client rows. It counts the open sessions beneath
  the row, in total and by state. `counts.sessions` answers "is it running at
  all?" and `state` answers "is it busy?"
- Session rows include `cwd`, `pid`, `surface` and
  `last_turn: { outcome, ended_at }`. Session titles are left out because clients
  generate them from prompts.
- Request rows include `reason`, and `tool_name` and (for MCP input)
  `mcp_server` when the client names them. Agent rows include `agent_type` once
  the client names it, which can be after the row first appears.

### Messages

```jsonl
{"v":1,"type":"snapshot","rev":41,"rows":[ … ]}
{"v":1,"type":"upsert","rev":44,"row":{ … }}
{"v":1,"type":"remove","rev":45,"id":"/claude/session:7f3a"}
{"v":1,"type":"error","code":"bad_request","message":"unknown view"}
```

- The snapshot's revision and the subscriber's registration happen in one step of
  the core, so no change falls between them.
- Revisions increase with every published change during one run of Eywa. A
  filtered subscriber sees gaps. After any reconnect, including after Eywa
  restarts, the new snapshot replaces the consumer's view entirely.
- Each subscriber has a bounded queue. If a subscriber falls behind, Eywa closes
  its connection and the subscriber resubscribes. Hooks and other subscribers are
  never held up.

## Recovery

Eywa holds no state across restarts. On startup it establishes what it can.

**Processes.** It lists the running processes of each client by its process rule:

- **No process for a client:** that client is `idle`, with no sessions. A missing
  process is proof that nothing is running.
- **A one-session process:** becomes an unconfirmed `process` node under its
  client, so the client shows `unknown` until a session from that process reports,
  or the process exits.
- **Hosts and supervisors:** get no node. A host's sessions report through hooks,
  and a supervisor is never claimed, because hooks claim the nearest matching
  ancestor.

**Claude Code's session registry.** Claude keeps one JSON file per running process
in `~/.claude/sessions/`, with the session's id, process id and status. Eywa
watches the folder and every file with kqueue (Claude rewrites the files in place)
and applies each status change:

| Status | Effect |
| --- | --- |
| `busy` | Open a turn if hooks haven't reported one: a placeholder named `running`. Eywa may have started mid-turn, or the registry reported a new turn before its `UserPromptSubmit` hook arrived. The first hook naming the real turn renames the placeholder: `open` the real turn, `retract` the placeholder. It is one turn and nothing ended, so `last_turn` still describes the turn before. Close any open request: going busy again after waiting means it was answered. |
| `waiting` | Open a request if none is open, else mark the open ones `prompted`. |
| `idle` | Close a turn that is still open at once, with no outcome yet (`end`). For two seconds the outcome is unsettled and the session has no `last_turn`: a `Stop` hook makes it `ok`, `StopFailure` makes it `error`, and a new prompt or two seconds of silence makes it `cancelled`. Until it settles no other main-thread hook reopens the turn; the registry going `busy` or `waiting` does, and then nothing is recorded. |

At startup this states every Claude session at once, with no `unknown`.

**During normal running:**

- A session whose process exits is closed.
- An open turn, agent or request that has heard nothing for an hour while its
  process is alive becomes unconfirmed. Sessions with nothing open never go stale,
  because sitting at the prompt is genuinely idle.

**Later sources**, each a `sync` feed like the Claude registry: Codex's
`thread_history_1.sqlite` and rollout files (open turns and subagents, not pending
approvals), Muse's session registry and `session.jsonl`, and the Antigravity CLI's
status line. If persistence turns out to be needed, Eywa can append normalized
events to a log and replay them on start; it would store events, never derived
state.

## Scope

Eywa blocks on its sockets and wakes only for real work: an incoming event, a
subscriber, a process exit, a registry change, or a deadline. It runs no polling
loop.

**v0.1:** the tree, the five events, the core, the Claude Code and Codex adapters,
process liveness, `api.sock` with `get` and `subscribe`, the `tree` and `requests`
views, `eywa status`, `eywa watch` and `eywa setup`.

**v0.2:** per-client hook parsing; the shared session tracker and the ordering
rule; the `observes` record; the Grok Build and Muse Code adapters; process rules
with hosts; the Claude session registry; setup for all four clients.

**Acceptance:**

1. The first demonstration passes.
2. With agents active, disconnect a subscriber, trigger an approval and
   reconnect. The first snapshot shows the waiting client.
3. Restart Eywa during an approval. The client shows `needs_input` or `unknown`,
   never `idle`. A false `idle` fails.
4. Interrupt a Claude turn. It closes as `cancelled` within seconds, through the
   session registry.
5. Run two tools in parallel in one turn, one needing approval. The other tool
   finishing does not clear the approval.
6. Hook sequences replay as tests without a live agent.

**Distribution:** a Homebrew tap (`brew install samctensen/tap/eywa`) builds
from the tagged source; `cargo install --git` is the path without Homebrew.
`eywa start` loads a per-user launchd job that runs `eywa serve` and restarts
it unless it exits cleanly, which `eywa stop` makes it do before unloading the
job. `eywa autostart on` copies the job's plist into `~/Library/LaunchAgents`,
so launchd also loads it at login. `eywa setup` adds or removes the hooks.

**Next:** live verification of every **verify** item with recorded sessions; the
Antigravity CLI adapter with its status line; Codex and Muse state sources;
`eywa doctor` (hook trust, folder trust, silent hooks); prebuilt release
binaries; measured latency and idle-overhead targets.

**Later:** Gemini CLI, OpenCode, Cursor, tool nodes, a `group_by: cwd` view
("which repo is blocked?"), Linux.

## Open questions

1. **"Finished, unseen."** An agent needs you when it's blocked mid-turn and also
   when it has finished and is waiting for your next prompt. Eywa publishes
   `last_turn`, and Claude turns carry an `origin` that tells your prompts from the
   machine's. A consumer `ack` operation would make "finished and not yet seen" a
   real state. Is `ack` worth adding to the core, or should consumers decide by
   time?
2. **Tool nodes.** `PreToolUse` and `PostToolUse` would give dashboards per-tool
   detail, at the cost of much more event traffic. Worth it, and for which views?
3. **Reviewer-decided approvals.** Codex's automatic reviewer and Muse's model
   reviewer decide some approvals after the approval hook fires, so Eywa briefly
   shows `needs_input` with nobody being asked. Where a "prompt shown" notification
   exists, it confirms a person is asked (`prompted`). Should consumers wait for
   it?
