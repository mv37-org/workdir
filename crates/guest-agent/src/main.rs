//! Guest agent — runs as PID-adjacent init helper inside the Firecracker
//! microVM. It speaks a newline-delimited JSON protocol over its stdio, which
//! the in-VM init wraps onto an `AF_VSOCK` port so the host agent can reach it
//! without any guest networking (spec §13.2 step 8, §6.1 "guest agent").
//!
//! Keeping the transport at stdio means this binary compiles and unit-tests on
//! any platform; the Linux init shim (`vsock <-> stdio`) is what makes it a
//! vsock service in production. The protocol here is the contract the host
//! `runtime` module drives.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::Path;
use std::process::Command;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Request {
    Ping,
    Exec { cmd: String, cwd: Option<String>, env: Option<Vec<(String, String)>>, background: Option<bool> },
    WriteFile { path: String, content_b64: String },
    ReadFile { path: String },
    ListDir { path: String },
    ReadyHttp { url: String, timeout_seconds: u64 },
    /// Streaming op: after a single Ok response line, this connection becomes
    /// the raw byte stream of a real TTY (openpty + shell on the slave side).
    /// One agent process serves one connection (socat fork), so the session
    /// ends — and the shell gets SIGHUP — when the host closes the stream.
    Pty { cols: Option<u16>, rows: Option<u16>, cmd: Option<String> },
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Response {
    Ok { result: serde_json::Value },
    Error { message: String },
}

fn handle(req: Request) -> Response {
    match req {
        // Pty is handled in main() — it takes over the whole connection.
        Request::Pty { .. } => Response::Error { message: "pty must be the first and only request on its connection".into() },
        Request::Ping => Response::Ok { result: serde_json::json!({"agent": "ready"}) },
        Request::Exec { cmd, cwd, env, background } => exec(cmd, cwd, env, background.unwrap_or(false)),
        Request::WriteFile { path, content_b64 } => write_file(path, content_b64),
        Request::ReadFile { path } => read_file(path),
        Request::ListDir { path } => list_dir(path),
        Request::ReadyHttp { url, timeout_seconds } => ready_http(url, timeout_seconds),
    }
}

fn exec(cmd: String, cwd: Option<String>, env: Option<Vec<(String, String)>>, background: bool) -> Response {
    let mut c = Command::new("/bin/sh");
    c.arg("-lc").arg(&cmd);
    if let Some(dir) = cwd {
        c.current_dir(dir);
    }
    if let Some(vars) = env {
        for (k, v) in vars {
            c.env(k, v);
        }
    }
    if background {
        match c.spawn() {
            Ok(child) => Response::Ok { result: serde_json::json!({"pid": child.id(), "background": true}) },
            Err(e) => Response::Error { message: e.to_string() },
        }
    } else {
        match c.output() {
            Ok(out) => Response::Ok {
                result: serde_json::json!({
                    "exit_code": out.status.code().unwrap_or(-1),
                    "stdout": String::from_utf8_lossy(&out.stdout),
                    "stderr": String::from_utf8_lossy(&out.stderr),
                }),
            },
            Err(e) => Response::Error { message: e.to_string() },
        }
    }
}

fn write_file(path: String, content_b64: String) -> Response {
    let bytes = match b64_decode(&content_b64) {
        Ok(b) => b,
        Err(e) => return Response::Error { message: e },
    };
    if let Some(parent) = Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&path, bytes) {
        Ok(()) => Response::Ok { result: serde_json::json!({"written": true, "path": path}) },
        Err(e) => Response::Error { message: e.to_string() },
    }
}

fn read_file(path: String) -> Response {
    match std::fs::read(&path) {
        Ok(bytes) => Response::Ok { result: serde_json::json!({"content_b64": b64_encode(&bytes)}) },
        Err(e) => Response::Error { message: e.to_string() },
    }
}

fn list_dir(path: String) -> Response {
    match std::fs::read_dir(&path) {
        Ok(entries) => {
            let mut names = vec![];
            for e in entries.flatten() {
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                names.push(serde_json::json!({
                    "name": e.file_name().to_string_lossy(),
                    "dir": is_dir,
                }));
            }
            Response::Ok { result: serde_json::json!({"entries": names}) }
        }
        Err(e) => Response::Error { message: e.to_string() },
    }
}

/// Minimal HTTP readiness poll using the system `curl` available in the base
/// image. Avoids pulling an HTTP client into the guest agent.
fn ready_http(url: String, timeout_seconds: u64) -> Response {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_seconds);
    loop {
        let ok = Command::new("curl")
            .args(["-fsS", "-o", "/dev/null", "--max-time", "2", &url])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Response::Ok { result: serde_json::json!({"ready": true}) };
        }
        if std::time::Instant::now() >= deadline {
            return Response::Error { message: format!("ready check timed out after {timeout_seconds}s") };
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

// --- tiny base64 (avoid a dependency in the guest) ---------------------------

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(input: &[u8]) -> String {
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        out.push(B64[(b[0] >> 2) as usize] as char);
        out.push(B64[(((b[0] & 0x03) << 4) | (b[1] >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64[(((b[1] & 0x0f) << 2) | (b[2] >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64[(b[2] & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn b64_decode(input: &str) -> Result<Vec<u8>, String> {
    let mut table = [255u8; 256];
    for (i, &c) in B64.iter().enumerate() {
        table[c as usize] = i as u8;
    }
    let clean: Vec<u8> = input.bytes().filter(|&b| b != b'=' && !b.is_ascii_whitespace()).collect();
    let mut out = Vec::new();
    for chunk in clean.chunks(4) {
        let mut acc = 0u32;
        let mut bits = 0;
        for &c in chunk {
            let v = table[c as usize];
            if v == 255 {
                return Err("invalid base64".to_string());
            }
            acc = (acc << 6) | v as u32;
            bits += 6;
        }
        let bytes = (bits / 8) as usize;
        acc <<= 24 - bits;
        for i in 0..bytes {
            out.push((acc >> (16 - i * 8)) as u8);
        }
    }
    Ok(out)
}

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    // Announce readiness on the console (stderr), NOT stdout — stdout is the
    // vsock response stream (one agent process per connection via socat), so a
    // banner there would corrupt the first response the host reads.
    eprintln!("{}", serde_json::json!({"event": "agent_ready"}));
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(Request::Pty { cols, rows, cmd }) => {
                // Set the TTY up FIRST so a failure is reported as a normal
                // error response; only then acknowledge and switch this
                // connection to the raw byte stream.
                match pty::spawn(cols.unwrap_or(120), rows.unwrap_or(32), cmd) {
                    Ok(session) => {
                        let ack = Response::Ok { result: serde_json::json!({"pty": true}) };
                        let _ = writeln!(stdout, "{}", serde_json::to_string(&ack).unwrap());
                        let _ = stdout.flush();
                        let code = pty::bridge(session);
                        std::process::exit(code);
                    }
                    Err(e) => Response::Error { message: format!("pty: {e}") },
                }
            }
            Ok(req) => handle(req),
            Err(e) => Response::Error { message: format!("bad request: {e}") },
        };
        let _ = writeln!(stdout, "{}", serde_json::to_string(&resp).unwrap());
        let _ = stdout.flush();
    }
}

/// Real-TTY support for the `pty` op: allocate a pseudo-terminal, run the
/// shell on its slave side, and pump bytes between this process's stdio (the
/// vsock connection, via the init shim) and the master side.
#[cfg(unix)]
mod pty {
    use std::io::{Read, Write};
    use std::os::unix::io::FromRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};

    pub struct Session {
        child: Child,
        /// PTY master, duplicated for the two pump directions.
        master_read: std::fs::File,
        master_write: std::fs::File,
    }

    pub fn spawn(cols: u16, rows: u16, cmd: Option<String>) -> Result<Session, String> {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let mut ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        let rc = unsafe {
            libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &mut ws)
        };
        if rc != 0 {
            return Err(format!("openpty failed: {}", std::io::Error::last_os_error()));
        }

        let mut c = match &cmd {
            Some(cmd) => {
                let mut c = Command::new("/bin/sh");
                c.args(["-lc", cmd]);
                c
            }
            None => {
                let mut c = Command::new("/bin/sh");
                c.arg("-l"); // interactive: stdin IS a tty
                c
            }
        };
        c.current_dir("/workspace");
        c.env("TERM", "xterm-256color");
        unsafe {
            c.stdin(Stdio::from_raw_fd(libc::dup(slave)));
            c.stdout(Stdio::from_raw_fd(libc::dup(slave)));
            c.stderr(Stdio::from_raw_fd(libc::dup(slave)));
            c.pre_exec(|| {
                // New session with the PTY slave (now fd 0) as controlling TTY,
                // so ^C and job control reach the shell.
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = c.spawn().map_err(|e| format!("spawn shell: {e}"))?;
        unsafe { libc::close(slave) };
        let (master_read, master_write) = unsafe {
            (std::fs::File::from_raw_fd(libc::dup(master)), std::fs::File::from_raw_fd(master))
        };
        Ok(Session { child, master_read, master_write })
    }

    /// Pump stdio ↔ master until the shell exits or the host closes the
    /// connection (stdin EOF → SIGHUP to the shell, like a closed terminal).
    /// Returns the process exit code to use.
    pub fn bridge(mut session: Session) -> i32 {
        let pid = session.child.id() as libc::pid_t;
        let mut master_write = session.master_write;
        std::thread::spawn(move || {
            // Read the connection via a dup of fd 0, NOT std::io::stdin(): the
            // main loop's `lines()` iterator still holds the global stdin lock
            // while we bridge, so locking Stdin here would deadlock keystrokes
            // (validated on the node: prompt out, input dead).
            let mut stdin = unsafe { std::fs::File::from_raw_fd(libc::dup(0)) };
            let mut buf = [0u8; 4096];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if master_write.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        let _ = master_write.flush();
                    }
                }
            }
            unsafe { libc::kill(pid, libc::SIGHUP) };
        });
        let mut master_read = session.master_read;
        let mut stdout = std::io::stdout();
        let mut buf = [0u8; 4096];
        loop {
            match master_read.read(&mut buf) {
                // EIO/EOF on the master means the slave side (shell) is gone.
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdout.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = stdout.flush();
                }
            }
        }
        session.child.wait().ok().and_then(|st| st.code()).unwrap_or(0)
    }
}

#[cfg(not(unix))]
mod pty {
    pub struct Session;
    pub fn spawn(_cols: u16, _rows: u16, _cmd: Option<String>) -> Result<Session, String> {
        Err("pty is only supported on unix guests".into())
    }
    pub fn bridge(_s: Session) -> i32 {
        1
    }
}
