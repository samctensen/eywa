//! The service: one core thread owns all state; other threads move bytes.
//!
//! - The ingest thread turns datagrams into records.
//! - The accept thread starts one thread per API connection.
//! - The core thread applies records to the tree and publishes changes. It
//!   sleeps on its channel until the next input or staleness deadline.

use crate::adapter::Adapter;
use crate::adapter::claude::ClaudeAdapter;
use crate::adapter::codex::CodexAdapter;
use crate::adapter::grok::GrokAdapter;
use crate::adapter::muse::MuseAdapter;
use crate::api::{
    ApiRequest, Op, Subscribers, error_line, parse_request, snapshot_line, stopping_line,
};
use crate::process;
use crate::record::HookRecord;
use crate::registry::{self, SessionState, Status};
use crate::row::{Change, Row};
use crate::tree::{Attrs, Event, Millis, State, Tree};
use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use crate::hook::{INGEST_SOCKET, default_dir};
pub use eywa_api::API_SOCKET;

pub const SUBSCRIBER_QUEUE: usize = 256;

const RECEIVE_BUFFER_BYTES: libc::c_int = 262_144;
const MAX_DATAGRAM_BYTES: usize = 1 << 20;
const MAX_REQUEST_BYTES: u64 = 64 * 1024;

pub struct Config {
    pub dir: PathBuf,
    pub stale_after: Millis,
    pub subscriber_queue: usize,
    /// How to recognize each client's processes when scanning at startup.
    pub executables: Vec<process::ProcessRule>,
    /// Claude Code's session registry folder to watch, if any.
    pub claude_sessions: Option<PathBuf>,
}

/// How long the outcome of a turn closed by the registry's `idle` may wait
/// for a `Stop` hook. The turn itself closes at once; with no `Stop` in this
/// time it was interrupted, and its outcome is `cancelled`.
const IDLE_SETTLE: Millis = 2_000;

enum Input {
    Record(Box<HookRecord>),
    Request {
        request: ApiRequest,
        respond: Sender<Receiver<String>>,
    },
    ProcessExited(u32),
    ClaudeSessions(Vec<SessionState>),
    Shutdown,
}

pub struct Server {
    dir: PathBuf,
    stop: Arc<AtomicBool>,
    inputs: Sender<Input>,
    threads: Vec<JoinHandle<()>>,
}

/// Refuses a directory other users could reach the sockets through. Eywa
/// creates its own directory as 0700; it never changes the permissions of a
/// directory it didn't create.
fn check_private(dir: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(dir)?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let owner = unsafe { libc::geteuid() };
    let reason = if !metadata.is_dir() {
        Some("is not a directory")
    } else if metadata.uid() != owner {
        Some("is owned by another user")
    } else if metadata.mode() & 0o077 != 0 {
        Some("is accessible to other users (expected mode 0700)")
    } else {
        None
    };
    match reason {
        None => Ok(()),
        Some(reason) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} {reason}", dir.display()),
        )),
    }
}

/// Creates Eywa's directory, readable only by the user.
pub fn create_dir(dir: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

pub fn serve(config: Config) -> io::Result<Server> {
    create_dir(&config.dir)?;
    check_private(&config.dir)?;
    let api_path = config.dir.join(API_SOCKET);
    let ingest_path = config.dir.join(INGEST_SOCKET);
    if UnixStream::connect(&api_path).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("another eywa is serving {}", api_path.display()),
        ));
    }
    // Nothing answered, so any socket files left here are from a crash.
    let _ = std::fs::remove_file(&api_path);
    let _ = std::fs::remove_file(&ingest_path);
    let listener = UnixListener::bind(&api_path)?;
    let ingest = UnixDatagram::bind(&ingest_path)?;
    raise_receive_buffer(&ingest);

    let dir = config.dir.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let (inputs, received) = mpsc::channel();
    if let Some(sessions) = config.claude_sessions.clone() {
        let sender = inputs.clone();
        registry::watch(sessions, move |states| {
            let _ = sender.send(Input::ClaudeSessions(states));
        });
    }
    let threads = vec![
        spawn_core(config, received, inputs.clone()),
        spawn_ingest(ingest, inputs.clone(), stop.clone()),
        spawn_accept(listener, inputs.clone(), stop.clone()),
    ];
    Ok(Server {
        dir,
        stop,
        inputs,
        threads,
    })
}

impl Server {
    /// Stops the service and removes its sockets.
    pub fn shutdown(self) {
        drop(self);
    }

    /// Blocks until the core thread stops.
    pub fn wait(mut self) {
        if !self.threads.is_empty() {
            let _ = self.threads.remove(0).join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.inputs.send(Input::Shutdown);
        // Wake the threads blocked on their sockets so they see `stop`.
        if let Ok(socket) = UnixDatagram::unbound() {
            let _ = socket.send_to(&[], self.dir.join(INGEST_SOCKET));
        }
        let _ = UnixStream::connect(self.dir.join(API_SOCKET));
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(self.dir.join(API_SOCKET));
        let _ = std::fs::remove_file(self.dir.join(INGEST_SOCKET));
    }
}

fn now() -> Millis {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as Millis)
}

struct Core {
    tree: Tree,
    adapters: Vec<Box<dyn Adapter>>,
    subscribers: Subscribers,
    rev: u64,
    /// Where exit watchers report; a clone of the core's own input channel.
    exits: Sender<Input>,
    watched: HashSet<u32>,
    /// The registry status last applied to each Claude session.
    applied: HashMap<String, Status>,
    /// When each session that went idle settles: by then the outcome of the
    /// turn that idle closed is decided.
    settling: HashMap<String, Millis>,
}

fn spawn_core(config: Config, inputs: Receiver<Input>, exits: Sender<Input>) -> JoinHandle<()> {
    thread::spawn(move || {
        let adapters: Vec<Box<dyn Adapter>> = vec![
            Box::new(ClaudeAdapter::new()),
            Box::new(CodexAdapter::new()),
            Box::new(GrokAdapter::new()),
            Box::new(MuseAdapter::new()),
        ];
        let clients: Vec<_> = adapters.iter().map(|adapter| adapter.client()).collect();
        let mut core = Core {
            tree: Tree::new(&clients, config.stale_after, now()),
            adapters,
            subscribers: Subscribers::new(config.subscriber_queue),
            rev: 0,
            exits,
            watched: HashSet::new(),
            applied: HashMap::new(),
            settling: HashMap::new(),
        };
        core.scan_processes(&config.executables);
        core.tree.take_changes();
        core.run(&inputs);
    })
}

impl Core {
    fn run(&mut self, inputs: &Receiver<Input>) {
        loop {
            let settles = self.settling.values().copied().min();
            let next = [self.tree.next_deadline(), settles]
                .into_iter()
                .flatten()
                .min();
            let input = match next {
                Some(deadline) => {
                    let wait = Duration::from_millis(deadline.saturating_sub(now()));
                    inputs.recv_timeout(wait)
                }
                None => inputs.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match input {
                Ok(Input::Record(record)) => self.apply_record(&record),
                Ok(Input::Request { request, respond }) => {
                    let _ = respond.send(self.answer(request));
                }
                Ok(Input::ProcessExited(pid)) => self.process_exited(pid),
                Ok(Input::ClaudeSessions(states)) => self.observe_sessions(states),
                Ok(Input::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.apply_settled(now());
            self.tree.expire(now());
            self.publish();
        }
    }

    /// Applies Claude registry statuses that changed, each at once. Going
    /// idle also starts the settle time for the turn it closed. Every file
    /// change re-reads every session, so a status already applied is skipped
    /// and its settle time left running.
    fn observe_sessions(&mut self, states: Vec<SessionState>) {
        let at = now();
        for state in states {
            self.watch(state.pid);
            if self.applied.get(&state.session_id) == Some(&state.status) {
                continue;
            }
            if state.status == Status::Idle {
                self.settling
                    .insert(state.session_id.clone(), at + IDLE_SETTLE);
            } else {
                self.settling.remove(&state.session_id);
            }
            self.apply_state(&state, at);
        }
    }

    /// Tells the Claude adapter which idle sessions' settle time has passed.
    fn apply_settled(&mut self, at: Millis) {
        let due: Vec<String> = self
            .settling
            .iter()
            .filter(|(_, when)| **when <= at)
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            self.settling.remove(&id);
            let Some(claude) = self
                .adapters
                .iter_mut()
                .find(|adapter| adapter.client().name == "claude")
            else {
                continue;
            };
            for event in claude.settle(&id, at) {
                self.tree.apply(event, at);
            }
        }
    }

    fn apply_state(&mut self, state: &SessionState, at: Millis) {
        self.applied
            .insert(state.session_id.clone(), state.status.clone());
        let Some(claude) = self
            .adapters
            .iter_mut()
            .find(|adapter| adapter.client().name == "claude")
        else {
            return;
        };
        for event in claude.observe(state, at) {
            self.tree.apply(event, at);
        }
        if let Some(status_at) = state.status_at {
            let derived = match state.status {
                Status::Busy => State::Working,
                Status::Waiting(_) => State::NeedsInput,
                Status::Idle => State::Idle,
            };
            let session = crate::adapter::session_path("claude", &state.session_id);
            self.tree.backdate(&session, derived, status_at);
        }
    }

    fn apply_record(&mut self, record: &HookRecord) {
        let at = now();
        let Some(adapter) = self
            .adapters
            .iter_mut()
            .find(|adapter| adapter.client().name == record.client)
        else {
            return;
        };
        for event in adapter.handle(record, at) {
            self.tree.apply(event, at);
        }
        if let Some(pid) = record.pid {
            self.watch(pid);
        }
    }

    /// Adds a process node for each running agent process, so its client
    /// shows `unknown` until one of its sessions reports or it exits.
    fn scan_processes(&mut self, executables: &[process::ProcessRule]) {
        let at = now();
        for (client, pid) in process::list_processes(executables) {
            self.tree.apply(
                Event::Open {
                    id: format!("/{client}/process:{pid}"),
                    attrs: Attrs::new(),
                },
                at,
            );
            self.watch(pid);
        }
    }

    fn watch(&mut self, pid: u32) {
        if self.watched.insert(pid) {
            let exits = self.exits.clone();
            process::watch_exit(pid, move || {
                let _ = exits.send(Input::ProcessExited(pid));
            });
        }
    }

    fn process_exited(&mut self, pid: u32) {
        self.watched.remove(&pid);
        for session in self.tree.process_exited(pid, now()) {
            let Some((client, session_id)) = session[1..].split_once("/session:") else {
                continue;
            };
            self.applied.remove(session_id);
            self.settling.remove(session_id);
            if let Some(adapter) = self
                .adapters
                .iter_mut()
                .find(|adapter| adapter.client().name == client)
            {
                adapter.forget_session(session_id);
            }
        }
    }

    /// Answers a request with the stream of lines to send. A `get` stream
    /// holds only the snapshot; a `subscribe` stream stays open.
    fn answer(&mut self, request: ApiRequest) -> Receiver<String> {
        let rows = self.tree.rows();
        match request.op {
            Op::Subscribe => self.subscribers.add(request.selection, self.rev, &rows),
            Op::Get => {
                let (sender, receiver) = mpsc::channel();
                let selected: Vec<Row> = rows
                    .into_iter()
                    .filter(|row| request.selection.includes(&row.id))
                    .collect();
                let _ = sender.send(snapshot_line(self.rev, selected));
                receiver
            }
            Op::Stop => {
                let (sender, receiver) = mpsc::channel();
                let _ = sender.send(stopping_line());
                receiver
            }
            Op::Other => unreachable!("the server never produces Other"),
        }
    }

    fn publish(&mut self) {
        let changes: Vec<(u64, Change)> = self
            .tree
            .take_changes()
            .into_iter()
            .map(|change| {
                self.rev += 1;
                (self.rev, change)
            })
            .collect();
        if !changes.is_empty() {
            self.subscribers.publish(&changes);
        }
    }
}

fn spawn_ingest(
    socket: UnixDatagram,
    inputs: Sender<Input>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut buffer = vec![0u8; MAX_DATAGRAM_BYTES];
        while let Ok(length) = socket.recv(&mut buffer) {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            let Ok(record) = serde_json::from_slice::<HookRecord>(&buffer[..length]) else {
                continue;
            };
            if record.has_valid_ids() && inputs.send(Input::Record(Box::new(record))).is_err() {
                return;
            }
        }
    })
}

fn spawn_accept(
    listener: UnixListener,
    inputs: Sender<Input>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        for stream in listener.incoming() {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            if let Ok(stream) = stream {
                let inputs = inputs.clone();
                thread::spawn(move || serve_connection(stream, &inputs));
            }
        }
    })
}

/// Reads one request line, then writes every line of the answer.
fn serve_connection(stream: UnixStream, inputs: &Sender<Input>) {
    let Ok(reader) = stream.try_clone() else {
        return;
    };
    let mut line = String::new();
    if BufReader::new(reader.take(MAX_REQUEST_BYTES))
        .read_line(&mut line)
        .is_err()
    {
        return;
    }
    let mut writer = stream;
    let request = match parse_request(line.trim()) {
        Err(error) => {
            let _ = writeln!(writer, "{}", error_line(&error));
            let _ = writer.shutdown(Shutdown::Both);
            return;
        }
        Ok(request) => request,
    };
    let stop = request.op == Op::Stop;
    let (respond, response) = mpsc::channel();
    if inputs.send(Input::Request { request, respond }).is_err() {
        return;
    }
    let Ok(lines) = response.recv() else {
        return;
    };
    for line in lines {
        if writeln!(writer, "{line}").is_err() {
            break;
        }
    }
    let _ = writer.shutdown(Shutdown::Both);
    // Shut down only after answering: the process exits soon after the core stops.
    if stop {
        let _ = inputs.send(Input::Shutdown);
    }
}

fn raise_receive_buffer(socket: &UnixDatagram) {
    let size = RECEIVE_BUFFER_BYTES;
    // SAFETY: the option value points at a live c_int of the stated size.
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            (&size as *const libc::c_int).cast::<libc::c_void>(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}
