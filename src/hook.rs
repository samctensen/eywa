//! The hook command: turns one hook payload into one datagram for Eywa.
//!
//! It never writes to stdout, because Claude Code reads hook stdout on some
//! events, and it never fails or waits: if Eywa isn't running, the record is
//! dropped.

use crate::process;
use crate::record::HookRecord;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};

pub const INGEST_SOCKET: &str = "ingest.sock";

/// Hook payloads include tool input, so a request to write a large file is
/// large. The cap only guards against unbounded input.
const MAX_PAYLOAD_BYTES: u64 = 64 << 20;
const SEND_BUFFER_BYTES: libc::c_int = 65_536;

/// `$EYWA_DIR`, or `~/Library/Application Support/eywa`.
pub fn default_dir() -> PathBuf {
    eywa_api::default_dir()
}

/// Sends a record to the ingest socket without blocking. A record that the
/// sender (`EMSGSIZE`) or the receiver's buffer (`ENOBUFS`) can't take is
/// resent without its background task list.
pub fn send_record(dir: &Path, record: &HookRecord) -> io::Result<()> {
    let socket = UnixDatagram::unbound()?;
    socket.set_nonblocking(true)?;
    raise_send_buffer(&socket);
    let target = dir.join(INGEST_SOCKET);
    match socket.send_to(&serde_json::to_vec(record)?, &target) {
        Err(error)
            if matches!(error.raw_os_error(), Some(libc::EMSGSIZE | libc::ENOBUFS))
                && record.background_tasks.is_some() =>
        {
            let smaller = HookRecord {
                background_tasks: None,
                ..record.clone()
            };
            socket
                .send_to(&serde_json::to_vec(&smaller)?, &target)
                .map(drop)
        }
        result => result.map(drop),
    }
}

/// Reads one hook payload and forwards it. Every failure is silent.
pub fn run_hook(client: &str, input: impl Read, dir: &Path) {
    let mut payload = Vec::new();
    if input
        .take(MAX_PAYLOAD_BYTES)
        .read_to_end(&mut payload)
        .is_err()
    {
        return;
    }
    let Ok(payload) = serde_json::from_slice(&payload) else {
        return;
    };
    // Grok runs Claude Code's hooks too, with Grok's payload.
    let client = if client == "claude" && std::env::var_os("GROK_HOOK_EVENT").is_some() {
        "grok"
    } else {
        client
    };
    let env = |name: &str| std::env::var(name).ok();
    let Ok(mut record) = HookRecord::from_hook(client, &payload, &env) else {
        return;
    };
    if record.pid.is_none()
        && let Some(rule) = process::ProcessRule::for_client(client)
        && let Some(pid) = process::find_ancestor(std::process::id(), &rule)
    {
        record.pid = Some(pid);
        record
            .surface
            .get_or_insert_with(|| process::surface(pid).to_string());
    }
    if let Some(path) = std::env::var_os("EYWA_RECORD") {
        let _ = append_line(Path::new(&path), &record);
    }
    let _ = send_record(dir, &record);
}

fn append_line(path: &Path, record: &HookRecord) -> io::Result<()> {
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(&line)
}

fn raise_send_buffer(socket: &UnixDatagram) {
    let size = SEND_BUFFER_BYTES;
    // SAFETY: the option value points at a live c_int of the stated size.
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&size as *const libc::c_int).cast::<libc::c_void>(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}
