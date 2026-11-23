# eywa-api: Eywa's API types as a Rust crate

Agreed with Sam on 2026-11-23 while restarting Wooting Glance on Eywa.

## Why

Sam is rebuilding Wooting Glance, which lights a Wooting keyboard from
Eywa's state, and wants it built on the data structures Eywa defines rather
than a copy of them. Today those structures can't be shared:

- `Row` and `Counts` (`src/row.rs`) and `Kind`, `State`, `Outcome` and `Attrs`
  (`src/tree.rs`) live beside the server's internals and derive only
  `Serialize`. `Attrs` is a plain `serde_json::Map`.
- `src/api.rs` assembles every message with `json!`, so no type describes a
  message.
- Consumers reimplement the contract from `docs/api.md`. That includes
  Eywa's own `eywa top` (`src/top.rs`), which reads rows as
  `serde_json::Value`.

## Goals

- A crate, `eywa-api`, in this repository holding the v1 wire types with
  `Serialize` and `Deserialize`, depending only on `serde` and `serde_json`.
- Eywa's server writes every API message through these types, so the JSON it
  publishes comes from the same definitions consumers decode.
- `eywa top` decodes through them, proving the crate reads what the server
  writes.
- A Rust program can depend on it by git and decode Eywa's output with no
  types of its own.

## Non-goals

- No change to the wire format or its version. Key order inside a JSON object
  may change; no consumer may rely on it, and `docs/api.md` never promised it.
- No subscriber library: no connection, reconnect or row-map helpers beyond
  the socket's location.
- No crates.io release.

## Anti-goals

- Two definitions of the same thing: the server keeping its own `Kind` or
  `State` next to the crate's.
- A newer Eywa breaking an older consumer. Decoding must accept unknown
  message types, fields, attribute keys and enum values, as `docs/api.md`
  (Compatibility) requires.

## Approaches

| Approach | Trade-off | Verdict |
| --- | --- | --- |
| A shared `eywa-api` crate in Eywa | One definition; Eywa changes first | **Picked by Sam** |
| Glance mirrors `docs/api.md` in its own types | No Eywa change; two copies to keep in step | Rejected |
| Glance depends on the `eywa` crate as it is | Types are serialize-only server internals; pulls `libc`, `sha2`, `humantime` | Rejected |

## Decided

- **Layout.** The repository becomes a Cargo workspace. The root package
  `eywa` stays where it is; the new member lives in `eywa-api/` (package
  `eywa-api`, library `eywa_api`, edition 2024, `rust-version = "1.88"`,
  `publish = false`, MIT). `eywa` depends on it by path.
- **Names match the Types section of `docs/api.md`:**
  - `Request` with `Op` (`Get`, `Subscribe`, `Stop`) and `View` (`Tree`,
    `Requests`); `path` and `depth` optional, as on the wire.
  - `Message`, tagged by `type`: `Snapshot { rev, rows }`, `Upsert { rev,
    row }`, `Remove { rev, id }`, `Error { code, message }`, `Stopping`.
    Every message carries `"v": 1` on the wire.
  - `Row { id, kind, state, since, reason, cause, counts, attrs }`. `since`
    is the RFC 3339 string. Optional fields are skipped when absent, never
    `null`.
  - `Kind`, `State`, `Reason`, `Outcome`, plus `Origin` (`person`,
    `machine`), `Approvals` (`actor`, `turn`, `none`) and `Interrupts`
    (`hook`, `inferred`).
  - `Counts { sessions, needs_input, working, unknown, idle }`.
  - `Attrs` as a struct: every attribute in the Attributes table as an
    optional field (`observes`, `cwd`, `pid`, `surface`, `model`,
    `last_turn`, `origin`, `agent_type`, `reason`, `prompted`, `tool_name`,
    `mcp_server`), plus a flattened map that keeps any attribute this version
    doesn't name. `Observes` and `LastTurn { outcome, ended_at }` are structs.
- **Forward compatibility.** Every enum has an `Other` variant marked
  `#[serde(other)]` for values this version doesn't know, and `Message` has
  one for unknown message types. `State::Unknown` keeps its documented
  meaning ("can't confirm"); `Other` is the catch-all. The server never
  produces `Other`.
- **Socket location.** The crate exports `API_SOCKET` (`"api.sock"`) and a
  `default_dir()` returning `$EYWA_DIR`, else
  `~/Library/Application Support/eywa`. The server's `hook::default_dir` uses
  it.
- **The server uses the crate's types directly.** `tree.rs` uses the crate's
  `Kind`, `State` and `Outcome`; match arms for `Other` are unreachable on the
  server and say so. The same goes for every type the crate defines that the
  server already has under another name. `tree.rs`'s `Observes`,
  `Attribution` and `Detection` become the crate's `Observes`, `Approvals` and
  `Interrupts`, and `adapter/mod.rs`'s `Reason` becomes the crate's `Reason`.
  That touches each adapter's `Client` constant, request tracking and the
  `CLAUDE` fixture in `tests/common/mod.rs`. *(My call, following the
  anti-goal.)* `row.rs` builds the crate's `Row` and `Counts`. `api.rs`
  serializes `Message` values instead of using `json!`. Nodes may keep a JSON
  map internally and convert it into `Attrs` when a row is published, keeping
  unknown keys.
- **`eywa top`** decodes `Message` and `Row` instead of `Value`.
- **Tests.**
  - Everything the server writes decodes back to an equal value.
  - JSON with an unknown message type, field, attribute and enum value
    decodes without error.
  - The existing service tests pass, comparing output as JSON values rather
    than bytes where key order changed.
- **Docs.** `docs/api.md` gains a Rust subsection under Types, showing the git
  dependency and decoding a line with
  `serde_json::from_str::<eywa_api::Message>`. The README's API section gets
  one line pointing to it.
- **Delivery.** A pull request to `samctensen/eywa` from `feat/api-crate`.
  Sam merges it, and Glance pins the merge commit by `rev`.

## Left to the builder

- Whether `v` sits on each `Message` variant or in a wrapper, as long as
  every message carries `"v": 1`.
- How request parsing uses `Request`, as long as every error code and message
  stays exactly as it is today.
- Module layout inside the crate.
- Where the server's millisecond-to-RFC 3339 formatting lives.
