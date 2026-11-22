//! `eywa top`: a live view of every agent Eywa can see, redrawn in the
//! terminal as it changes.

use crate::tree::Millis;
use serde_json::Value;
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Rows by id, as the subscription last described them.
pub type Rows = BTreeMap<String, Value>;

const FRAME: Millis = 100;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const STATES: [&str; 4] = ["needs_input", "working", "unknown", "idle"];

const PLAIN: &str = "";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// How wide a row's state and age are together, and the gap before them.
const STATUS: usize = 18;
const GAP: usize = 2;

/// Whether to use color at all: not when `NO_COLOR` is set or the terminal
/// is dumb.
static COLOR: LazyLock<bool> = LazyLock::new(|| {
    std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty())
        && std::env::var_os("TERM").is_none_or(|term| term != "dumb")
});

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// Subscribes to everything on `stream` and redraws the view until Ctrl-C,
/// or until the service goes away.
pub fn run(mut stream: UnixStream) -> io::Result<()> {
    writeln!(stream, r#"{{"v":1,"op":"subscribe","path":"/","depth":8}}"#)?;
    let (sender, messages) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if let Ok(message) = serde_json::from_str::<Value>(&line)
                && sender.send(message).is_err()
            {
                break;
            }
        }
    });
    let handler = stop as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: the handler only stores to an atomic, which is signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
    let mut out = io::stdout().lock();
    // Switch to the alternate screen and hide the cursor; both are undone below.
    out.write_all(b"\x1b[?1049h\x1b[?25l")?;
    let result = animate(&messages, &mut out);
    out.write_all(b"\x1b[?25h\x1b[?1049l")?;
    out.flush()?;
    result
}

fn animate(messages: &mpsc::Receiver<Value>, out: &mut impl Write) -> io::Result<()> {
    let mut rows = Rows::new();
    while !STOP.load(Ordering::Relaxed) {
        let wait = FRAME - now() % FRAME;
        match messages.recv_timeout(Duration::from_millis(wait)) {
            Ok(message) => {
                apply(&mut rows, message)?;
                for message in messages.try_iter() {
                    apply(&mut rows, message)?;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "the service stopped",
                ));
            }
        }
        let (width, height) = terminal_size();
        let mut screen = String::from("\x1b[H");
        for (index, line) in render(&rows, now(), width).lines().take(height).enumerate() {
            if index > 0 {
                screen.push_str("\r\n");
            }
            screen.push_str(line);
            screen.push_str("\x1b[K");
        }
        screen.push_str("\x1b[J");
        out.write_all(screen.as_bytes())?;
        out.flush()?;
    }
    Ok(())
}

fn apply(rows: &mut Rows, mut message: Value) -> io::Result<()> {
    match message["type"].as_str() {
        Some("snapshot") => {
            rows.clear();
            if let Value::Array(list) = message["rows"].take() {
                list.into_iter().for_each(|row| insert(rows, row));
            }
        }
        Some("upsert") => insert(rows, message["row"].take()),
        Some("remove") => {
            if let Some(id) = message["id"].as_str() {
                rows.remove(id);
            }
        }
        Some("error") => {
            let reason = message["message"].as_str().unwrap_or("request refused");
            return Err(io::Error::other(reason.to_string()));
        }
        _ => {}
    }
    Ok(())
}

fn insert(rows: &mut Rows, row: Value) {
    if let Some(id) = row["id"].as_str().map(String::from) {
        rows.insert(id, row);
    }
}

/// Draws `rows` as they stand at `now`, each line at most `width` columns.
pub fn render(rows: &Rows, now: Millis, width: usize) -> String {
    let view = View::new(rows, now);
    let mut entries = vec![Entry::from(view.header()), Entry::default()];
    for client in view.children("/") {
        view.client(client, &mut entries);
        entries.push(Entry::default());
    }
    entries.push(Entry::from(Line::default().add(DIM, "  ⌃C to quit")));
    // States and ages line up down the whole screen, and long names give
    // way before they push them off the edge.
    let longest = entries
        .iter()
        .filter(|entry| entry.status.is_some())
        .map(|entry| entry.name.width())
        .max()
        .unwrap_or(0);
    let column = longest.min(width.saturating_sub(STATUS + GAP));
    let mut screen = String::new();
    for entry in entries {
        entry.write(column, width, &mut screen);
        screen.push('\n');
    }
    screen
}

struct View<'a> {
    rows: &'a Rows,
    children: BTreeMap<&'a str, Vec<&'a str>>,
    now: Millis,
    frame: usize,
}

impl<'a> View<'a> {
    fn new(rows: &'a Rows, now: Millis) -> Self {
        let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for id in rows.keys().filter(|id| *id != "/") {
            let parent = match id.rsplit_once('/') {
                Some(("", _)) | None => "/",
                Some((parent, _)) => parent,
            };
            children.entry(parent).or_default().push(id);
        }
        // Clients keep their order. Beneath them, whatever needs you comes
        // first, then the latest to change.
        for (parent, list) in &mut children {
            if *parent != "/" {
                list.sort_by_cached_key(|id| {
                    let row = &rows[*id];
                    (state(row) != "needs_input", Reverse(since(row)), *id)
                });
            }
        }
        View {
            rows,
            children,
            now,
            frame: (now / FRAME) as usize,
        }
    }

    fn children(&self, id: &str) -> &[&'a str] {
        self.children.get(id).map_or(&[], Vec::as_slice)
    }

    fn header(&self) -> Line {
        let line = Line::default().add(BOLD, "  eywa");
        let Some(machine) = self.rows.get("/") else {
            return line.add(DIM, " · connecting…");
        };
        let counts = &machine["counts"];
        let sessions = counts["sessions"].as_u64().unwrap_or(0);
        let plural = if sessions == 1 { "" } else { "s" };
        let mut line = line.add(DIM, format!(" · {sessions} session{plural}"));
        for state in STATES {
            let count = counts[state].as_u64().unwrap_or(0);
            if count > 0 {
                let what = match state {
                    "needs_input" if count > 1 => "need you",
                    _ => word(state),
                };
                line = line
                    .add(PLAIN, "   ")
                    .add(color(state), self.glyph(state))
                    .add(PLAIN, format!(" {count} {what}"));
            }
        }
        line
    }

    fn client(&self, id: &str, entries: &mut Vec<Entry>) {
        let state = state(&self.rows[id]);
        let sessions = self.children(id);
        let summary = match sessions.len() {
            0 => "no sessions".to_string(),
            1 => "1 session".to_string(),
            count => format!("{count} sessions"),
        };
        entries.push(Entry::from(
            Line::default()
                .add(PLAIN, "  ")
                .add(color(state), self.glyph(state))
                .add(BOLD, format!(" {}", id.trim_start_matches('/')))
                .add(DIM, format!("  {summary}")),
        ));
        self.branches(sessions, "  ", entries);
    }

    /// Draws `ids` as siblings in the tree, expanding each that isn't idle.
    fn branches(&self, ids: &[&str], indent: &str, entries: &mut Vec<Entry>) {
        for (index, id) in ids.iter().enumerate() {
            let last = index + 1 == ids.len();
            let row = &self.rows[*id];
            let state = state(row);
            let (name, detail) = label(id, row);
            entries.push(Entry {
                name: Line::default()
                    .add(DIM, format!("{indent}{}", if last { "└─ " } else { "├─ " }))
                    .add(color(state), self.glyph(state))
                    .add(
                        if state == "idle" { DIM } else { PLAIN },
                        format!(" {name}"),
                    )
                    .add(DIM, detail),
                status: Some(
                    Line::default()
                        .add(color(state), format!("{:<9}", word(state)))
                        .add(DIM, format!("  {:>7}", elapsed(row, self.now))),
                ),
            });
            if state != "idle" {
                let indent = format!("{indent}{}", if last { "   " } else { "│  " });
                self.branches(self.children(id), &indent, entries);
            }
        }
    }

    fn glyph(&self, state: &str) -> &'static str {
        match state {
            "needs_input" => "◆",
            "working" => SPINNER[self.frame % SPINNER.len()],
            "unknown" => "?",
            _ => "○",
        }
    }
}

/// What needs you is bold yellow and work is green; everything else is dim.
/// These are the terminal's own colors, so its theme decides the shades.
fn color(state: &str) -> &'static str {
    match state {
        "needs_input" => "\x1b[1;33m",
        "working" => "\x1b[32m",
        _ => DIM,
    }
}

fn state(row: &Value) -> &str {
    row["state"].as_str().unwrap_or("unknown")
}

fn word(state: &str) -> &'static str {
    match state {
        "needs_input" => "needs you",
        "working" => "working",
        "idle" => "idle",
        _ => "unknown",
    }
}

/// A node's name and a dim detail after it, such as a session's folder and
/// short id.
fn label(id: &str, row: &Value) -> (String, String) {
    let segment = id.rsplit('/').next().unwrap_or(id);
    let (kind, key) = segment.split_once(':').unwrap_or((segment, ""));
    let short: String = key.chars().take(8).collect();
    match kind {
        "session" => {
            let folder = row["attrs"]["cwd"]
                .as_str()
                .and_then(|cwd| cwd.rsplit('/').find(|part| !part.is_empty()));
            (folder.unwrap_or("session").to_string(), format!(" {short}"))
        }
        "request" => {
            let reason = row["attrs"]["reason"].as_str().unwrap_or("request");
            (reason.replace('_', " "), String::new())
        }
        _ if short.is_empty() => (kind.to_string(), String::new()),
        _ => (kind.to_string(), format!(" {short}")),
    }
}

/// When the row entered its current state.
fn since(row: &Value) -> Option<Millis> {
    let since = humantime::parse_rfc3339(row["since"].as_str()?).ok()?;
    Some(since.duration_since(UNIX_EPOCH).ok()?.as_millis() as Millis)
}

fn elapsed(row: &Value, now: Millis) -> String {
    let Some(since) = since(row) else {
        return String::new();
    };
    let seconds = now.saturating_sub(since) / 1000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        3600..86400 => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
        _ => format!("{}d {}h", seconds / 86400, seconds % 86400 / 3600),
    }
}

/// Styled pieces of one line of the screen.
#[derive(Default)]
struct Line(Vec<(&'static str, String)>);

impl Line {
    /// Adds text with control characters removed: folder names and ids come
    /// from hook payloads and must not drive the terminal.
    fn add(mut self, style: &'static str, text: impl Into<String>) -> Self {
        let text = text.into().chars().filter(|c| !c.is_control()).collect();
        self.0.push((style, text));
        self
    }

    /// How many columns the line takes up.
    fn width(&self) -> usize {
        self.0.iter().map(|(_, text)| text.chars().count()).sum()
    }

    /// Writes the line in at most `width` columns, ending in an ellipsis
    /// where it's cut short.
    fn write(&self, width: usize, out: &mut String) {
        let cut = self.width() > width;
        let mut room = if cut { width.saturating_sub(1) } else { width };
        for (style, text) in &self.0 {
            let shown: String = text.chars().take(room).collect();
            if shown.is_empty() {
                continue;
            }
            room -= shown.chars().count();
            if style.is_empty() || !*COLOR {
                out.push_str(&shown);
            } else {
                out.push_str(style);
                out.push_str(&shown);
                out.push_str(RESET);
            }
        }
        if cut && width > 0 {
            out.push('…');
        }
    }
}

/// One line of the screen: a name, and for each agent, its state and age
/// in columns on the right.
#[derive(Default)]
struct Entry {
    name: Line,
    status: Option<Line>,
}

impl From<Line> for Entry {
    fn from(name: Line) -> Self {
        Entry { name, status: None }
    }
}

impl Entry {
    /// Writes the entry in `width` columns, its status starting just past
    /// `column`.
    fn write(&self, column: usize, width: usize, out: &mut String) {
        let Some(status) = &self.status else {
            return self.name.write(width, out);
        };
        self.name.write(column, out);
        let shown = self.name.width().min(column);
        out.push_str(&" ".repeat(column - shown + GAP));
        status.write(width.saturating_sub(column + GAP), out);
    }
}

fn now() -> Millis {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as Millis)
}

/// The terminal's width and height, or 80 by 24 when stdout isn't one.
fn terminal_size() -> (usize, usize) {
    // SAFETY: winsize is plain data, so all-zero bytes are valid.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ writes one winsize to the pointer it is given.
    let found = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0;
    if found && size.ws_col > 0 && size.ws_row > 0 {
        (usize::from(size.ws_col), usize::from(size.ws_row))
    } else {
        (80, 24)
    }
}
