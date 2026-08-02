//! End-to-end tests for the thing wmux exists to do: keep a session alive
//! across a detach and give the next client the screen back.
//!
//! These drive the wire protocol directly rather than going through
//! `wmux attach`, because attaching needs a real console and CI does not have
//! one. Everything below the console layer is exercised for real: a detached
//! server process, a live ConPTY, pwsh, and the named pipe.

use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wmux::pipe::PipeConn;
use wmux::protocol::{ClientMsg, ServerMsg};
use wmux::session;

/// How long to wait for the server process to publish its pipe.
const BOOT_TIMEOUT: Duration = Duration::from_secs(45);
/// How long to wait for a shell to start and echo something back.
const SHELL_TIMEOUT: Duration = Duration::from_secs(90);

const COLS: u16 = 80;
const ROWS: u16 = 24;

/// Locates the `wmux` binary that cargo just built, next to the test binary.
fn wmux_exe() -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("test executable path");
    path.pop();
    if path.file_name().is_some_and(|f| f == "deps") {
        path.pop();
    }
    path.join("wmux.exe")
}

/// A server process plus its session name, killed when the test ends.
struct TestSession {
    name: String,
    child: Child,
}

impl Drop for TestSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// The child is handed to `TestSession`, whose Drop kills and reaps it on every
// exit path. Clippy cannot see across the move.
#[allow(clippy::zombie_processes)]
fn start_session(tag: &str) -> TestSession {
    // Unique per process so concurrent test binaries never collide.
    let name = format!("wmuxtest-{}-{}", tag, std::process::id());

    let child = Command::new(wmux_exe())
        .args([
            "server",
            "--name",
            &name,
            "--cols",
            &COLS.to_string(),
            "--rows",
            &ROWS.to_string(),
            "--",
            "pwsh.exe",
            "-NoLogo",
            "-NoProfile",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the wmux session server");

    let deadline = Instant::now() + BOOT_TIMEOUT;
    while Instant::now() < deadline {
        if session::session_exists(&name).unwrap_or(false) {
            return TestSession { name, child };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("session {name:?} never published its pipe within {BOOT_TIMEOUT:?}");
}

fn connect(name: &str) -> Arc<PipeConn> {
    let path = session::pipe_path(name).expect("pipe path");
    Arc::new(PipeConn::connect(&path).expect("connect to session pipe"))
}

/// Pumps server messages onto a channel so the test can apply a timeout.
fn spawn_reader(conn: Arc<PipeConn>) -> Receiver<ServerMsg> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(msg) = ServerMsg::read_from(&mut &*conn) {
            if tx.send(msg).is_err() {
                return;
            }
        }
    });
    rx
}

/// Feeds every byte the server sends into a terminal model and waits until the
/// rendered screen contains `needle`.
///
/// Matching on the rendered screen rather than the raw stream matters: ConPTY
/// interleaves cursor movement with the characters it echoes, so a substring
/// search over the raw bytes is unreliable.
fn wait_for_screen_text(rx: &Receiver<ServerMsg>, needle: &str, timeout: Duration) -> String {
    let mut parser = vt100::Parser::new(ROWS, COLS, 0);
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            panic!(
                "never saw {needle:?} on screen within {timeout:?}; screen was:\n{}",
                parser.screen().contents()
            );
        }
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(ServerMsg::Repaint(bytes)) | Ok(ServerMsg::Output(bytes)) => {
                parser.process(&bytes);
                let contents = parser.screen().contents();
                if contents.contains(needle) {
                    return contents;
                }
            }
            Ok(ServerMsg::Exited(code)) => panic!("session exited early with code {code}"),
            Ok(ServerMsg::InfoReply { .. }) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                panic!(
                    "server closed the connection before {needle:?} appeared; screen was:\n{}",
                    parser.screen().contents()
                )
            }
        }
    }
}

#[test]
fn session_survives_detach_and_replays_the_screen_on_reattach() {
    let session = start_session("reattach");
    let marker = format!("WMUXMARK{}", std::process::id());

    // --- first attach -------------------------------------------------
    let conn = connect(&session.name);
    let rx = spawn_reader(conn.clone());
    ClientMsg::Attach {
        cols: COLS,
        rows: ROWS,
    }
    .write_to(&mut &*conn)
    .expect("send attach");

    // Wait for a prompt before typing, otherwise the keystrokes race pwsh's
    // startup and get swallowed.
    wait_for_screen_text(&rx, "PS ", SHELL_TIMEOUT);

    ClientMsg::Input(format!("echo {marker}\r").into_bytes())
        .write_to(&mut &*conn)
        .expect("send input");

    let screen = wait_for_screen_text(&rx, &marker, SHELL_TIMEOUT);
    assert!(
        screen.contains(&marker),
        "the marker should be on the live screen"
    );

    // --- detach -------------------------------------------------------
    ClientMsg::Detach
        .write_to(&mut &*conn)
        .expect("send detach");
    drop(rx);
    drop(conn);

    // The session must still be running with nobody attached.
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        session::session_exists(&session.name).expect("list sessions"),
        "the session should outlive its client"
    );

    // --- reattach -----------------------------------------------------
    let conn2 = connect(&session.name);
    let rx2 = spawn_reader(conn2.clone());
    ClientMsg::Attach {
        cols: COLS,
        rows: ROWS,
    }
    .write_to(&mut &*conn2)
    .expect("send second attach");

    // The very first thing a reattaching client gets must be the screen as it
    // stands, including output produced while nothing was attached.
    let replayed = wait_for_screen_text(&rx2, &marker, Duration::from_secs(20));
    assert!(
        replayed.contains(&marker),
        "reattach must replay the screen state from before the detach"
    );
}

#[test]
fn a_new_session_is_discoverable_and_reports_its_geometry() {
    let session = start_session("info");

    let names = session::list_sessions().expect("list sessions");
    assert!(
        names.contains(&session.name),
        "{:?} should appear in {names:?}",
        session.name
    );

    let conn = connect(&session.name);
    ClientMsg::Info.write_to(&mut &*conn).expect("send info");
    match ServerMsg::read_from(&mut &*conn).expect("read info reply") {
        ServerMsg::InfoReply {
            cols,
            rows,
            clients,
            command,
        } => {
            assert_eq!((cols, rows), (COLS, ROWS));
            assert_eq!(clients, 0, "an info query should not count as an attach");
            assert!(command.contains("pwsh"), "unexpected command {command:?}");
        }
        other => panic!("expected an info reply, got {other:?}"),
    }
}

#[test]
fn output_produced_while_detached_is_still_there_on_reattach() {
    let session = start_session("offline");
    let marker = format!("OFFLINE{}", std::process::id());

    let conn = connect(&session.name);
    let rx = spawn_reader(conn.clone());
    ClientMsg::Attach {
        cols: COLS,
        rows: ROWS,
    }
    .write_to(&mut &*conn)
    .expect("send attach");
    wait_for_screen_text(&rx, "PS ", SHELL_TIMEOUT);

    // Queue work that lands after we are gone, then leave immediately.
    ClientMsg::Input(format!("Start-Sleep -Milliseconds 1500; echo {marker}\r").into_bytes())
        .write_to(&mut &*conn)
        .expect("send input");
    ClientMsg::Detach
        .write_to(&mut &*conn)
        .expect("send detach");
    drop(rx);
    drop(conn);

    // The command runs with no client attached at all.
    std::thread::sleep(Duration::from_secs(4));

    let conn2 = connect(&session.name);
    let rx2 = spawn_reader(conn2.clone());
    ClientMsg::Attach {
        cols: COLS,
        rows: ROWS,
    }
    .write_to(&mut &*conn2)
    .expect("send second attach");

    let screen = wait_for_screen_text(&rx2, &marker, Duration::from_secs(30));
    assert!(
        screen.contains(&marker),
        "work done while detached must be visible on reattach"
    );
}

#[test]
fn killing_a_session_removes_it() {
    let session = start_session("kill");
    assert!(session::session_exists(&session.name).unwrap());

    let conn = connect(&session.name);
    ClientMsg::Kill.write_to(&mut &*conn).expect("send kill");
    drop(conn);

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if !session::session_exists(&session.name).unwrap_or(true) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("session {:?} was still listed after kill", session.name);
}
