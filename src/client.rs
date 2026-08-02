//! The attaching client.
//!
//! The client is deliberately dumb: it owns no session state. It connects to
//! the pipe, announces its terminal size, paints whatever repaint blob the
//! server sends back, and then shuttles bytes in both directions until the
//! user detaches or the session ends.
//!
//! Because it is a plain console program that speaks VT on stdout, it works
//! anywhere a console works: Windows Terminal, the VS Code terminal, or an
//! OpenSSH session from another machine.

use anyhow::{Context, Result};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::console;
use crate::console::{console_size, RawMode};
use crate::pipe::PipeConn;
use crate::protocol::{ClientMsg, ServerMsg};
use crate::session;

/// Default prefix key: Ctrl-B (0x02), matching tmux.
///
/// PSReadLine binds Ctrl-B to backward-char, so inside a session you reach the
/// literal keystroke the same way tmux users already do: press the prefix
/// twice. `WMUX_PREFIX` overrides the binding for anyone who would rather not
/// give up Ctrl-B.
pub const DEFAULT_PREFIX: u8 = 0x02;

/// The key pressed after the prefix to detach, matching tmux's `prefix d`.
pub const DETACH_KEY: u8 = b'd';

/// Resolves the prefix key, honouring `WMUX_PREFIX`.
///
/// Accepts a caret form (`^B`, `^\`) or a bare control character. Anything
/// unparseable falls back to the default rather than failing an attach.
pub fn configured_prefix() -> u8 {
    match std::env::var("WMUX_PREFIX") {
        Ok(raw) => parse_prefix(&raw).unwrap_or(DEFAULT_PREFIX),
        Err(_) => DEFAULT_PREFIX,
    }
}

/// Parses a prefix specification such as `^B` into its control byte.
pub fn parse_prefix(raw: &str) -> Option<u8> {
    let trimmed = raw.trim();
    let bytes = trimmed.as_bytes();
    match bytes {
        // Caret notation: ^A..^_ maps to 0x01..0x1F.
        [b'^', c] => {
            let upper = c.to_ascii_uppercase();
            if (b'@'..=b'_').contains(&upper) {
                Some(upper - b'@')
            } else {
                None
            }
        }
        // A single literal control byte.
        [c] if *c < 0x20 => Some(*c),
        _ => None,
    }
}

/// How often the client re-reads the console size to spot a window resize.
const RESIZE_POLL: std::time::Duration = std::time::Duration::from_millis(200);

/// How a completed attach ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The user pressed the detach key; the session is still running.
    Detached,
    /// The session's child process exited with this code.
    Exited(i32),
}

/// Recognises the `prefix` + `d` detach sequence in a raw input stream.
///
/// Split out from the I/O so the escape handling can be tested without a
/// console attached.
#[derive(Debug)]
pub struct DetachDetector {
    prefix: u8,
    /// True when the previous byte was the prefix and we are waiting to see
    /// what it modifies.
    armed: bool,
}

impl DetachDetector {
    pub fn new(prefix: u8) -> Self {
        DetachDetector {
            prefix,
            armed: false,
        }
    }

    /// Consumes raw input and returns the bytes to forward to the pty plus
    /// whether the user asked to detach.
    ///
    /// Pressing the prefix twice forwards a single literal prefix byte, which
    /// is how you type Ctrl-\ inside a session.
    pub fn feed(&mut self, input: &[u8]) -> (Vec<u8>, bool) {
        let mut out = Vec::with_capacity(input.len());
        for &byte in input {
            if self.armed {
                self.armed = false;
                if byte == DETACH_KEY || byte == DETACH_KEY.to_ascii_uppercase() {
                    return (out, true);
                }
                if byte == self.prefix {
                    out.push(self.prefix);
                } else {
                    // Not a recognised binding: pass the prefix through
                    // followed by whatever was typed.
                    out.push(self.prefix);
                    out.push(byte);
                }
            } else if byte == self.prefix {
                self.armed = true;
            } else {
                out.push(byte);
            }
        }
        (out, false)
    }
}

/// Attaches to a running session and blocks until detach or session exit.
pub fn attach(name: &str) -> Result<Outcome> {
    let path = session::pipe_path(name)?;
    let conn = Arc::new(
        PipeConn::connect(&path)
            .with_context(|| format!("no session named {name:?} is running"))?,
    );

    // Raw mode is restored by the guard's Drop on every exit path below.
    let _raw = RawMode::enter()?;

    let size = console_size();
    ClientMsg::Attach {
        cols: size.cols,
        rows: size.rows,
    }
    .write_to(&mut &*conn)
    .context("failed to announce the attach")?;

    let detached = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));

    spawn_input_pump(conn.clone(), detached.clone(), finished.clone());
    spawn_resize_watcher(conn.clone(), finished.clone(), size);

    let outcome = pump_output(&conn, &detached);

    finished.store(true, Ordering::SeqCst);
    // Leave the cursor somewhere sane and drop any partial styling, so the
    // shell the user returns to is not wearing the session's colours.
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(b"\x1b[0m\r\n");
    let _ = stdout.flush();

    outcome
}

/// Reads the server side of the connection and paints it to stdout.
fn pump_output(conn: &Arc<PipeConn>, detached: &Arc<AtomicBool>) -> Result<Outcome> {
    let mut stdout = std::io::stdout();
    loop {
        match ServerMsg::read_from(&mut &**conn) {
            Ok(ServerMsg::Repaint(bytes)) | Ok(ServerMsg::Output(bytes)) => {
                stdout.write_all(&bytes)?;
                stdout.flush()?;
            }
            Ok(ServerMsg::Exited(code)) => return Ok(Outcome::Exited(code)),
            // An attached client never asks for these, but ignoring them keeps
            // the stream in sync if a future version starts pushing them.
            Ok(ServerMsg::InfoReply { .. }) | Ok(ServerMsg::CaptureReply(_)) => {}
            Err(_) => {
                // The connection dropped. If we asked to detach, that is the
                // server acknowledging by closing; otherwise the session died
                // underneath us, which we also report as a detach so the user
                // is not shown a spurious error.
                if detached.load(Ordering::SeqCst) {
                    return Ok(Outcome::Detached);
                }
                return Ok(Outcome::Detached);
            }
        }
    }
}

/// Forwards keystrokes, watching for the detach sequence.
fn spawn_input_pump(conn: Arc<PipeConn>, detached: Arc<AtomicBool>, finished: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut detector = DetachDetector::new(configured_prefix());
        let mut buf = [0u8; 1024];
        loop {
            if finished.load(Ordering::SeqCst) {
                return;
            }
            // Reads the console handle directly rather than going through
            // std::io::stdin(); see console::read_console_input for why.
            let n = match console::read_console_input(&mut buf) {
                Ok(0) => {
                    crate::server::log("input: console returned EOF");
                    return;
                }
                Ok(n) => n,
                Err(e) => {
                    crate::server::log(&format!("input: read failed: {e}"));
                    return;
                }
            };
            // With WMUX_LOG set this records exactly what the console handed
            // us, which is the only way to tell a key-translation problem from
            // a detector problem without a debugger attached to a live TTY.
            crate::server::log(&format!(
                "input: {n} bytes [{}] prefix=0x{:02x}",
                buf[..n]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<Vec<_>>()
                    .join(" "),
                configured_prefix()
            ));

            let (forward, detach) = detector.feed(&buf[..n]);
            if detach {
                crate::server::log("input: detach sequence recognised");
            }
            if !forward.is_empty() && ClientMsg::Input(forward).write_to(&mut &*conn).is_err() {
                return;
            }
            if detach {
                detached.store(true, Ordering::SeqCst);
                let _ = ClientMsg::Detach.write_to(&mut &*conn);
                return;
            }
        }
    });
}

/// Polls the console size and tells the server when the window changes.
///
/// Windows has no SIGWINCH; polling is the standard approach and 200 ms is
/// well inside the threshold where a resize feels instant.
fn spawn_resize_watcher(
    conn: Arc<PipeConn>,
    finished: Arc<AtomicBool>,
    initial: crate::console::Size,
) {
    std::thread::spawn(move || {
        let mut last = initial;
        loop {
            std::thread::sleep(RESIZE_POLL);
            if finished.load(Ordering::SeqCst) {
                return;
            }
            let now = console_size();
            if now != last {
                last = now;
                let resize = ClientMsg::Resize {
                    cols: now.cols,
                    rows: now.rows,
                };
                if resize.write_to(&mut &*conn).is_err() {
                    return;
                }
            }
        }
    });
}

/// Opens a short-lived connection just to ask a session about itself.
pub fn query_info(name: &str) -> Result<ServerMsg> {
    let path = session::pipe_path(name)?;
    let conn = PipeConn::connect(&path)?;
    ClientMsg::Info.write_to(&mut &conn)?;
    ServerMsg::read_from(&mut &conn).context("session did not answer the info request")
}

/// Sends literal input to a session without attaching.
///
/// The call round-trips a capture afterwards, so it returns only once the
/// server has actually consumed the bytes and written them to the pty. Without
/// that handshake a caller that exits immediately could close the pipe before
/// the input was read.
pub fn send_keys(name: &str, keys: &str) -> Result<()> {
    let path = session::pipe_path(name)?;
    let conn = PipeConn::connect(&path)
        .with_context(|| format!("no session named {name:?} is running"))?;
    ClientMsg::Input(keys.as_bytes().to_vec()).write_to(&mut &conn)?;
    ClientMsg::Capture.write_to(&mut &conn)?;
    match ServerMsg::read_from(&mut &conn)? {
        ServerMsg::CaptureReply(_) => Ok(()),
        other => anyhow::bail!("unexpected reply to send: {other:?}"),
    }
}

/// Reads back the session's visible screen as plain text.
pub fn capture(name: &str) -> Result<String> {
    let path = session::pipe_path(name)?;
    let conn = PipeConn::connect(&path)
        .with_context(|| format!("no session named {name:?} is running"))?;
    ClientMsg::Capture.write_to(&mut &conn)?;
    match ServerMsg::read_from(&mut &conn)? {
        ServerMsg::CaptureReply(text) => Ok(text),
        other => anyhow::bail!("unexpected reply to capture: {other:?}"),
    }
}

/// Polls `capture` until `needle` appears on screen or the deadline passes.
///
/// Automation needs this: after sending a command there is no signal that the
/// shell has finished with it, and sleeping a guessed interval is both slower
/// and less reliable than waiting for the output to show up.
pub fn capture_wait(name: &str, needle: &str, timeout: std::time::Duration) -> Result<String> {
    let deadline = std::time::Instant::now() + timeout;
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        last = capture(name)?;
        if last.contains(needle) {
            return Ok(last);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    anyhow::bail!("{needle:?} did not appear within {timeout:?}; screen was:\n{last}")
}

/// Asks a session to terminate its child process.
pub fn kill(name: &str) -> Result<()> {
    let path = session::pipe_path(name)?;
    let conn = PipeConn::connect(&path)
        .with_context(|| format!("no session named {name:?} is running"))?;
    ClientMsg::Kill.write_to(&mut &conn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detector() -> DetachDetector {
        DetachDetector::new(DEFAULT_PREFIX)
    }

    #[test]
    fn ordinary_typing_passes_straight_through() {
        let (out, detach) = detector().feed(b"echo hello\r");
        assert_eq!(out, b"echo hello\r");
        assert!(!detach);
    }

    #[test]
    fn prefix_then_d_detaches_and_forwards_nothing() {
        let mut d = detector();
        let (out, detach) = d.feed(&[DEFAULT_PREFIX, b'd']);
        assert!(out.is_empty());
        assert!(detach);
    }

    #[test]
    fn uppercase_d_also_detaches() {
        let mut d = detector();
        let (_, detach) = d.feed(&[DEFAULT_PREFIX, b'D']);
        assert!(detach);
    }

    #[test]
    fn prefix_split_across_reads_still_detaches() {
        // The prefix and the key routinely land in different console reads.
        let mut d = detector();
        let (out, detach) = d.feed(&[DEFAULT_PREFIX]);
        assert!(out.is_empty());
        assert!(!detach);
        let (out, detach) = d.feed(b"d");
        assert!(out.is_empty());
        assert!(detach);
    }

    #[test]
    fn doubled_prefix_sends_one_literal_prefix() {
        let mut d = detector();
        let (out, detach) = d.feed(&[DEFAULT_PREFIX, DEFAULT_PREFIX]);
        assert_eq!(out, vec![DEFAULT_PREFIX]);
        assert!(!detach);
    }

    #[test]
    fn prefix_followed_by_an_unbound_key_forwards_both() {
        let mut d = detector();
        let (out, detach) = d.feed(&[DEFAULT_PREFIX, b'x']);
        assert_eq!(out, vec![DEFAULT_PREFIX, b'x']);
        assert!(!detach);
    }

    #[test]
    fn text_before_the_detach_is_still_forwarded() {
        let mut d = detector();
        let (out, detach) = d.feed(&[b'h', b'i', DEFAULT_PREFIX, b'd']);
        assert_eq!(out, b"hi");
        assert!(detach);
    }

    #[test]
    fn arrow_key_escape_sequences_are_untouched() {
        let mut d = detector();
        let (out, detach) = d.feed(b"\x1b[A\x1b[B\x1b[C\x1b[D");
        assert_eq!(out, b"\x1b[A\x1b[B\x1b[C\x1b[D");
        assert!(!detach);
    }

    #[test]
    fn ctrl_c_is_forwarded_not_intercepted() {
        let mut d = detector();
        let (out, detach) = d.feed(&[0x03]);
        assert_eq!(out, vec![0x03]);
        assert!(!detach);
    }

    #[test]
    fn default_prefix_matches_tmux() {
        assert_eq!(DEFAULT_PREFIX, 0x02, "tmux uses Ctrl-B");
    }

    #[test]
    fn caret_notation_parses_to_control_bytes() {
        assert_eq!(parse_prefix("^B"), Some(0x02));
        assert_eq!(parse_prefix("^b"), Some(0x02));
        assert_eq!(parse_prefix("^A"), Some(0x01));
        // ^\ is 0x1C, the other common multiplexer prefix.
        assert_eq!(parse_prefix(r"^\"), Some(0x1C));
    }

    #[test]
    fn literal_control_byte_parses() {
        assert_eq!(parse_prefix("\x02"), Some(0x02));
    }

    #[test]
    fn nonsense_prefixes_are_rejected() {
        for raw in ["", "hello", "^", "abc", "^^^"] {
            assert_eq!(parse_prefix(raw), None, "{raw:?}");
        }
    }

    #[test]
    fn a_custom_prefix_detaches_on_d() {
        let mut d = DetachDetector::new(0x01);
        let (out, detach) = d.feed(&[0x01, b'd']);
        assert!(out.is_empty());
        assert!(detach);
        // ...and Ctrl-B is now just ordinary input.
        let mut d = DetachDetector::new(0x01);
        let (out, detach) = d.feed(&[0x02]);
        assert_eq!(out, vec![0x02]);
        assert!(!detach);
    }
}
