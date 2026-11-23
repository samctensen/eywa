# Eywa

<p align="center">
  <img src="docs/assets/eywa-halftone.png" alt="A glowing willow-like tree rendered as a grid of halftone dots" width="100%">
</p>

Eywa is a macOS background service that tracks what your coding agents are
doing and publishes one live answer to "which of them needs me?" over a local
socket.

Claude Code, Codex, Grok Build and Muse Code report to it through their hooks.
Eywa merges those reports into one tree of everything that is running, gives
every node a state, and streams the tree as JSON lines to any program that
connects. It is a layer to build on: a status light, a menu bar item, a
notifier or a script reads the same data whichever agent produced it. Eywa
has no UI or notifier of its own, and it is read-only: you still answer in
the agent.

```console
$ eywa status --requests | jq -r '.rows[] | [.attrs.reason, .attrs.tool_name // "-", .id] | @tsv' | column -t
approval  Bash             /claude/session:7f3a/turn:12/agent:a2/request:t9
question  AskUserQuestion  /claude/session:b41c/turn:3/request:t4
approval  -                /grok/session:5e0d/turn:7/request:c2
```

[How it works](#how-it-works) · [Install](#install) · [The API](#the-api) ·
[Command line](#command-line) · [Limits](#what-eywa-cant-see-or-do)

## How it works

```
Claude Code ─┐                                       ┌─► eywa status, watch, top
Codex ───────┤  eywa hook <client>   ┌────────────┐  ├─► a status light
Grok Build ──┼──────────────────────►│ eywa serve │──┼─► a menu bar item
Muse Code ───┘  one record per event │    tree    │  └─► a notifier
                                     │  + states  │
  process table ────────────────────►│            │  api.sock
  Claude's session files ───────────►└────────────┘  JSON lines
```

1. **Hooks in.** `eywa setup` adds one hook command, `eywa hook <client>`, to
   each agent's settings. On every lifecycle event it keeps a short list of
   allowed fields, sends them to Eywa as one datagram and exits. It never
   waits for Eywa, and exits 0 even when Eywa isn't running.
2. **A tree with derived states.** Eywa maps every agent's events onto one
   shape: the machine, a client (an agent app such as Claude Code), a session
   (one conversation), a turn (one prompt and the work it starts), an agent
   (a subagent) and a request (something waiting on you). Each node's state
   is computed from everything beneath it, so "is Codex busy?" and "is that
   subagent waiting?" are the same question asked at different depths. Eywa
   also watches agent processes and Claude Code's session files, so a session
   whose process exits is closed, and Claude Code's sessions are known as soon
   as Eywa starts.
3. **A socket out.** A program connects to a Unix socket, asks for part of
   the tree, and gets a snapshot followed by every change.

## Install

Eywa needs macOS and Rust 1.88 or newer:

```sh
cargo install --git https://github.com/samctensen/eywa
eywa start          # runs Eywa in the background now
eywa autostart on   # and at every login
eywa setup          # adds Eywa's hook to every agent installed
```

Then, depending on your agents:

- **Codex:** open Codex, run `/hooks` and trust the new hooks. Codex skips
  them until you do.
- **Grok Build:** trust the folders you work in. Grok silently skips hooks in
  an untrusted folder.
- **Every agent:** start a new session, so it loads the hooks.

To check that hooks are arriving, run `eywa top` and start a new agent
session: it should appear within a moment. `eywa setup` changes only its own
entries in `~/.claude/settings.json`, `~/.codex/hooks.json`,
`~/.grok/hooks/eywa.json` and `~/.config/muse/settings.json` (honoring
`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GROK_HOME` and `XDG_CONFIG_HOME`), and is
safe to run again. `eywa setup claude codex` sets up only those two.

To update, run the same `cargo install` command, then `eywa stop && eywa start`.
To uninstall, run `eywa setup --remove`, `eywa autostart off`, `eywa stop` and
`cargo uninstall eywa`. Eywa's directory, `~/Library/Application Support/eywa`,
keeps its log and launchd plist until you delete it.

## The API

### Subscribe

Connect to `~/Library/Application Support/eywa/api.sock`, write one JSON
request on one line, and read JSON lines back:

```sh
echo '{"v":1,"op":"subscribe","path":"/","depth":1}' \
  | nc -U "$HOME/Library/Application Support/eywa/api.sock"
```

```jsonl
{"v":1,"type":"snapshot","rev":41,"rows":[{"id":"/","kind":"machine","state":"needs_input",…},{"id":"/claude",…},{"id":"/codex",…},{"id":"/grok",…},{"id":"/muse",…}]}
{"v":1,"type":"upsert","rev":44,"row":{"id":"/claude","kind":"client","state":"working",…}}
```

The output is abridged: `…` stands for fields left out. A request has these
fields:

| Field | Default | Meaning |
| --- | --- | --- |
| `v` | required | Always `1`. |
| `op` | required | `get`: one snapshot, then the connection closes. `subscribe`: a snapshot, then every change. `stop`: shut Eywa down. |
| `view` | `"tree"` | `tree`: the nodes at and below `path`. `requests`: every open request, as a flat list. |
| `path` | `"/"` | Where the tree view starts, for example `/claude`. A path that names nothing gives an empty snapshot, not an error. |
| `depth` | `1` | Levels below `path` to include. `0` is the node alone. There is no unlimited depth; ask for a large one such as `64`. |

### The tree

Every node is published as a row, and every row's state summarizes its whole
subtree:

```
/                             machine   needs_input
├─ /claude                    client    needs_input
│  └─ …/session:7f3a          session   needs_input
│     └─ …/turn:12            turn      needs_input
│        └─ …/agent:a2        agent     needs_input   agent_type: Explore
│           └─ …/request:t9   request   needs_input   tool_name: Bash
├─ /codex                     client    working
├─ /grok                      client    idle
└─ /muse                      client    idle
```

An id is the node's path: `/` for the machine, `/claude`, `/codex`, `/grok`
or `/muse` for a client, then one `<kind>:<name>` segment per level. A request
sits under the turn when the main thread asked, or under the agent that
asked. A `process` row is a running agent process that hasn't reported a
session yet. Treat ids as opaque keys and read `kind` rather than parsing
them.

### Rows

```json
{
  "id": "/claude/session:7f3a/turn:12/agent:a2/request:t9",
  "kind": "request",
  "state": "needs_input",
  "since": "2026-11-18T20:41:07.000Z",
  "reason": "approval",
  "cause": "/claude/session:7f3a/turn:12/agent:a2/request:t9",
  "attrs": { "reason": "approval", "prompted": true, "tool_name": "Bash" }
}
```

| Field | Present | Meaning |
| --- | --- | --- |
| `id` | always | The node's path. |
| `kind` | always | `machine`, `client`, `process`, `session`, `turn`, `agent` or `request`. |
| `state` | always | The derived state. See [States](#states). |
| `since` | always | When `state` last changed: RFC 3339, UTC, with milliseconds. |
| `reason` | when `needs_input` | `approval`, `question` or `mcp_input`. |
| `cause` | unless `idle` | The id of the node responsible for the state, possibly the row's own. |
| `counts` | machine and client rows | Open sessions beneath: `sessions`, and how many are `needs_input`, `working`, `unknown` and `idle`. |
| `attrs` | always, possibly `{}` | Facts about the node, by kind. See [Attributes](#attributes). |

Fields that don't apply are left out, never `null`.

### States

The first rule that matches wins:

| State | Rule |
| --- | --- |
| `needs_input` | An open request exists beneath the node. |
| `working` | An open turn or subagent exists beneath it. |
| `unknown` | Something is open that Eywa can't confirm: it started before Eywa, or it has been silent for an hour. |
| `idle` | None of the above. |

Elapsed time never makes anything `idle`; silence only makes it `unknown`.
A turn that has finished and is waiting for your next prompt is `idle`. The
session's `last_turn` says how and when that turn ended, so a consumer can
show "finished, not yet seen" by its own rules.

### Attributes

| Kind | Attributes |
| --- | --- |
| `client` | `observes`: what the client lets Eywa see (`agents`, `approvals`, `questions`, `mcp_input`, `interrupts`). Always present. |
| `session` | `cwd`, `pid`, `surface` (usually `cli`, `desktop` or `ide`), and `last_turn` with `outcome` (`ok`, `error` or `cancelled`) and `ended_at`. |
| `turn` | `origin`: `person` or `machine`. Claude Code only. |
| `agent` | `agent_type`, such as `Explore` or the name of an agent you defined. It can arrive in a later `upsert`. |
| `request` | `reason` (always), `prompted` once the agent shows the request to you, `tool_name`, and `mcp_server` on `mcp_input` requests. |
| `machine`, `process` | None. |

Apart from `observes` and a request's `reason`, each attribute is present
only when the agent states it; Eywa never guesses one. Which agent states
what differs: see the tables for [clients](docs/api.md#client),
[agents](docs/api.md#agent) and [requests](docs/api.md#request).

### Messages

| `type` | Fields | Meaning |
| --- | --- | --- |
| `snapshot` | `rev`, `rows` | Every row in your selection, sorted by `id`. Always first. |
| `upsert` | `rev`, `row` | A row appeared or changed. Replace the whole row. |
| `remove` | `rev`, `id` | A row disappeared. |
| `error` | `code`, `message` | The request was rejected: `bad_request` or `unsupported_version`. The connection then closes. |
| `stopping` | none | The answer to `stop`. |

`rev` rises with every change anywhere in the tree, so a filtered
subscription sees gaps. It restarts at zero when Eywa does, so don't compare
it across connections.

### A minimal client

Keep a map of rows by `id`: a snapshot replaces it, `upsert` sets a row and
`remove` deletes one. This status light reads only the machine row:

```python
import json
import os
import socket

path = os.path.expanduser("~/Library/Application Support/eywa/api.sock")
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(path)
sock.sendall(b'{"v":1,"op":"subscribe","path":"/","depth":0}\n')

rows = {}
for line in sock.makefile(encoding="utf-8"):
    message = json.loads(line)
    if message["type"] == "snapshot":
        rows = {row["id"]: row for row in message["rows"]}
    elif message["type"] == "upsert":
        rows[message["row"]["id"]] = message["row"]
    elif message["type"] == "remove":
        rows.pop(message["id"], None)
    else:
        continue
    machine = rows["/"]
    print(machine["state"], machine.get("reason", ""), flush=True)
```

A real consumer also reconnects. The connection closes when Eywa restarts or
when you fall more than 256 messages behind; wait a second, subscribe again,
and show your data as unavailable until the new snapshot arrives. There is no
heartbeat. Ignore message types, fields and enum values you don't recognize,
since version 1 can add them.

[docs/api.md](docs/api.md) is the full reference: every field and when it is
present, TypeScript types, and complete subscribers in Python and TypeScript.
[docs/api-v1.schema.json](docs/api-v1.schema.json) is the same contract as a
JSON Schema, for validating messages or generating types.

## Command line

| Command | What it does |
| --- | --- |
| `eywa setup [agent…]` | Adds Eywa's hook to the named agents (`claude`, `codex`, `grok`, `muse`), or to every one installed. |
| `eywa setup --remove [agent…]` | Takes the hooks out again. |
| `eywa start` | Runs Eywa in the background through launchd, restarting it if it crashes, until `eywa stop` or logout. |
| `eywa stop` | Stops Eywa. With autostart on, it starts again at your next login. |
| `eywa autostart on` or `off` | Turns starting at login on or off, without starting or stopping Eywa now. With neither, prints `on` or `off`. |
| `eywa serve` | Runs Eywa in the foreground. `eywa start` does this for you. |
| `eywa status` | Prints one snapshot as a JSON line and exits. |
| `eywa watch` | Prints a snapshot, then every change, until ⌃C or Eywa stops. |
| `eywa top` | A live, full-screen view of every agent. ⌃C quits. |
| `eywa hook <client>` | What the agents' hooks run. Not for running by hand. |
| `eywa --version` | Prints the version. |

`status` and `watch` print messages in the API's format. They take
`--path <id>`, `--depth <n>` and `--requests`, which set the request's `path`,
`depth` and `view` [described above](#subscribe). Commands exit with `1` on
failure (Eywa isn't running, it rejects the request, or `setup` finds no
agents) and with `2` on an unknown command, option or agent name. Under
`eywa start`, Eywa logs to `eywa.log` in its directory.

### Recipes

These use [jq](https://jqlang.org). The example at the top of this page is
one more.

```sh
# One line for a status bar
eywa status --depth 0 | jq -r '.rows[0].counts | "\(.needs_input) waiting · \(.working) working"'

# Every Claude Code session, its state and its folder
eywa status --path /claude | jq -r '.rows[] | select(.kind == "session") | "\(.state)\t\(.attrs.cwd // "-")"'

# Follow Codex as it changes
eywa watch --path /codex --depth 2
```

## What Eywa can't see or do

- **macOS only.** It runs under launchd and watches files with kqueue.
- **Read-only.** A consumer can see that an approval or question is waiting,
  but can't answer it through Eywa. You answer in the agent.
- **Agents report different things.** Codex, Grok Build and Muse Code don't
  report MCP input requests, and Muse Code doesn't report questions. Each
  client row's `observes` record states this, so a consumer doesn't have to
  hard-code it. Grok Build attaches approvals to the turn and doesn't name
  their tool, and Muse Code never names a subagent's type, so `tool_name` or
  `agent_type` is simply absent there.
- **No history.** Eywa holds no state across restarts. When it starts, it
  recovers Claude Code's sessions from their session files. Another agent's
  session that was already running shows as a `process` row in `unknown`, or,
  in a process that hosts many sessions such as Codex's, not at all until it
  next reports an event.
- **Not always a person.** Some agents run an automatic reviewer before asking
  you, so an approval can be `needs_input` briefly without anyone being asked.
  A request's `prompted` is set once the agent confirms it is showing it to you.

## Privacy

Prompts, responses, commands, tool arguments and tool output never leave the
hook command. Eywa receives ids, event names, tool names, subagent types, MCP
server names, the working directory, the transcript path, the process id, a
timestamp, and the notification type, error code and background task statuses
the agent attaches to an event. An event's `reason` is sent only when it is
`channel_closed` or `shutdown`, which mark a session ending; any other reason
is dropped, because it can be free text. Tool input is reduced to a
hash, used only to match an approval with the tool call it belongs to. To see
the exact record, set `EYWA_RECORD` as described under
[Development](#development). Eywa also reads Claude Code's session files in
`~/.claude/sessions` (session ids, process ids and status), never
transcripts. Everything stays on your machine, in a directory only you can
read.

## Development

```sh
cargo test
cargo clippy --all-targets
cargo fmt --check
```

[docs/design.md](docs/design.md) is the design, including how each agent's
hooks map onto the tree. To see exactly what your agents send, set
`EYWA_RECORD=/path/to/file.jsonl` in the environment they run in; the hook
command appends each record (allowed fields only) to that file. To run a
second copy of Eywa alongside your own, point both it and your agents at
another directory with `EYWA_DIR` and run it with `eywa serve`. With
`EYWA_DIR` set, `start` and `autostart on|off` refuse to run and `stop` stops
only that copy, so your own is left alone.

## License

[MIT](LICENSE)
