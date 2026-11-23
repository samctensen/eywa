#![allow(dead_code)]

use eywa::tree::{
    Attribution, Client, Detection, Event, Millis, Observes, Outcome, STALE_AFTER, Tree,
};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub const WAIT: Duration = Duration::from_secs(5);

pub const CLAUDE: Client = Client {
    name: "claude",
    observes: Observes {
        agents: true,
        approvals: Attribution::Actor,
        questions: true,
        mcp_input: true,
        interrupts: Detection::Inferred,
    },
};

pub fn tree() -> Tree {
    Tree::new(&[CLAUDE], STALE_AFTER, 0)
}

pub fn open(t: &mut Tree, id: &str, at: Millis) {
    open_with(t, id, serde_json::json!({}), at);
}

pub fn open_with(t: &mut Tree, id: &str, attrs: serde_json::Value, at: Millis) {
    let attrs = attrs.as_object().cloned().unwrap_or_default();
    t.apply(
        Event::Open {
            id: id.into(),
            attrs,
        },
        at,
    );
}

pub fn close(t: &mut Tree, id: &str, outcome: Outcome, at: Millis) {
    t.apply(
        Event::Close {
            id: id.into(),
            outcome,
        },
        at,
    );
}

/// A short directory under `/tmp`, removed on drop. Unix socket paths must
/// stay under 104 bytes, which the system temp directory can exceed.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn temp_dir(tag: &str) -> TempDir {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let path = PathBuf::from(format!("/tmp/eywa-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    TempDir(path)
}

pub fn config(dir: &Path) -> eywa::server::Config {
    eywa::server::Config {
        dir: dir.to_path_buf(),
        stale_after: 2_000,
        subscriber_queue: 256,
        executables: Vec::new(),
        claude_sessions: None,
    }
}

pub fn start(dir: &Path) -> eywa::server::Server {
    eywa::server::serve(config(dir)).unwrap()
}

/// Connects to the API socket and sends one request line.
pub fn connect(dir: &Path, request: &str) -> BufReader<UnixStream> {
    let mut stream = UnixStream::connect(dir.join("api.sock")).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    writeln!(stream, "{request}").unwrap();
    BufReader::new(stream)
}

pub fn next_json(reader: &mut impl BufRead) -> serde_json::Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error}: {line:?}"))
}

/// Reads messages, tracking each row's state, until `done` holds.
pub fn wait_for_states(
    reader: &mut impl BufRead,
    done: impl Fn(&std::collections::HashMap<String, String>) -> bool,
) {
    let deadline = Instant::now() + WAIT;
    let mut states = std::collections::HashMap::new();
    loop {
        assert!(Instant::now() < deadline, "timed out: {states:?}");
        let message = next_json(reader);
        let rows = match message["type"].as_str().unwrap() {
            "snapshot" => message["rows"].as_array().unwrap().clone(),
            "upsert" => vec![message["row"].clone()],
            _ => Vec::new(),
        };
        for row in rows {
            states.insert(
                row["id"].as_str().unwrap().to_string(),
                row["state"].as_str().unwrap().to_string(),
            );
        }
        if done(&states) {
            return;
        }
    }
}

/// Reads messages, keeping each row by id, until `done` holds. Only rows
/// that arrive during this call are kept.
pub fn wait_for_rows(
    reader: &mut impl BufRead,
    done: impl Fn(&std::collections::HashMap<String, serde_json::Value>) -> bool,
) -> std::collections::HashMap<String, serde_json::Value> {
    let deadline = Instant::now() + WAIT;
    let mut rows = std::collections::HashMap::new();
    let id = |row: &serde_json::Value| row["id"].as_str().unwrap().to_string();
    loop {
        assert!(Instant::now() < deadline, "timed out: {rows:?}");
        let message = next_json(reader);
        match message["type"].as_str().unwrap() {
            "snapshot" => {
                let snapshot = message["rows"].as_array().unwrap();
                rows = snapshot.iter().map(|row| (id(row), row.clone())).collect();
            }
            "upsert" => {
                rows.insert(id(&message["row"]), message["row"].clone());
            }
            "remove" => {
                rows.remove(message["id"].as_str().unwrap());
            }
            _ => {}
        }
        if done(&rows) {
            return rows;
        }
    }
}

/// Repeats a `get` until its snapshot satisfies `done`.
pub fn wait_for_snapshot(
    dir: &Path,
    request: &str,
    done: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let snapshot = next_json(&mut connect(dir, request));
        if done(&snapshot) {
            return snapshot;
        }
        assert!(Instant::now() < deadline, "timed out: {snapshot}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Runs the `eywa` binary with `EYWA_DIR` set to `dir`.
pub fn run_eywa(args: &[&str], dir: &Path, stdin: &str) -> Output {
    run_eywa_env(args, dir, &[], stdin)
}

pub fn run_eywa_env(args: &[&str], dir: &Path, envs: &[(&str, &Path)], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_eywa"))
        .args(args)
        .env("EYWA_DIR", dir)
        .envs(envs.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// Copies a system binary to `path` and signs the copy ad hoc; macOS kills an
/// unsigned copy. Only the bytes are copied, and signing is retried, because
/// codesign can fail when macOS updates a copy's extended attributes mid-sign.
pub fn signed_copy(source: &str, path: &Path) {
    std::fs::write(path, std::fs::read(source).unwrap()).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    for _ in 0..3 {
        let signed = Command::new("codesign")
            .args(["-f", "-s", "-"])
            .arg(path)
            .output()
            .unwrap();
        if signed.status.success() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("codesign failed for {}", path.display());
}
