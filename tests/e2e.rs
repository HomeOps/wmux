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
            Ok(ServerMsg::InfoReply { .. })
            | Ok(ServerMsg::CaptureReply(_))
            | Ok(ServerMsg::Detached) => {}
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
fn shell_state_persists_across_separate_send_and_capture_calls() {
    // This is the automation path: no console, no attach. Each send/capture is
    // an independent connection, exactly as a script or another program would
    // do it, and the shell's variables have to survive between them.
    let session = start_session("state");

    wmux::client::capture_wait(&session.name, "PS ", SHELL_TIMEOUT).expect("shell should start");

    wmux::client::send_keys(&session.name, "$demo = 6 * 7\r").expect("assign a variable");
    // A fresh connection, as if from a completely separate invocation.
    wmux::client::send_keys(&session.name, "Write-Output \"answer=$demo\"\r")
        .expect("read the variable back");

    let screen = wmux::client::capture_wait(&session.name, "answer=42", Duration::from_secs(20))
        .expect("the variable should still be set");

    assert!(
        screen.contains("answer=42"),
        "state did not survive between calls; screen was:\n{screen}"
    );
}

#[test]
fn capture_does_not_register_as_an_attached_client() {
    let session = start_session("captureonly");
    wmux::client::capture_wait(&session.name, "PS ", SHELL_TIMEOUT).expect("shell should start");

    let conn = connect(&session.name);
    ClientMsg::Info.write_to(&mut &*conn).expect("send info");
    match ServerMsg::read_from(&mut &*conn).expect("read info reply") {
        ServerMsg::InfoReply { clients, .. } => assert_eq!(clients, 0),
        other => panic!("expected an info reply, got {other:?}"),
    }
}

#[test]
fn run_returns_the_pipeline_as_json_not_a_screen_scrape() {
    use wmux::run::{self, Format};

    let session = start_session("runjson");
    wmux::client::capture_wait(&session.name, "PS ", SHELL_TIMEOUT).expect("shell should start");

    let json = run::run(
        &session.name,
        "1..4 | ForEach-Object { $_ * $_ }",
        Format::Json,
        4,
        Duration::from_secs(60),
    )
    .expect("run should return");

    assert!(
        json.contains("\"Output\":[1,4,9,16]"),
        "expected the real pipeline values, got: {json}"
    );
    assert!(json.contains("\"Success\":true"), "got: {json}");
}

#[test]
fn run_survives_output_far_wider_than_the_terminal() {
    use wmux::run::{self, Format};

    // 4000 characters on an 80-column screen. A screen scrape would return
    // this wrapped across 50 rows, and most of it would have scrolled away.
    let session = start_session("runwide");
    wmux::client::capture_wait(&session.name, "PS ", SHELL_TIMEOUT).expect("shell should start");

    let text = run::run(
        &session.name,
        "-join ('x' * 4000)",
        Format::Text,
        4,
        Duration::from_secs(60),
    )
    .expect("run should return");

    let xs = text.chars().filter(|c| *c == 'x').count();
    assert_eq!(
        xs, 4000,
        "payload was truncated or wrapped; got {xs} characters"
    );
}

#[test]
fn run_reports_a_failing_command_without_killing_the_session() {
    use wmux::run::{self, Format};

    let session = start_session("runerr");
    wmux::client::capture_wait(&session.name, "PS ", SHELL_TIMEOUT).expect("shell should start");

    let json = run::run(
        &session.name,
        "throw 'deliberate failure'",
        Format::Json,
        4,
        Duration::from_secs(60),
    )
    .expect("run should still return a result");

    assert!(json.contains("\"Success\":false"), "got: {json}");
    assert!(json.contains("deliberate failure"), "got: {json}");

    // The session must still be usable afterwards.
    let after = run::run(
        &session.name,
        "2 + 2",
        Format::Json,
        4,
        Duration::from_secs(60),
    )
    .expect("the session should survive a failing command");
    assert!(after.contains("\"Output\":[4]"), "got: {after}");
}

#[test]
fn ctrl_b_then_d_is_intercepted_by_the_client_not_the_session() {
    // A session's ConPTY *is* a real console, so wmux can host its own client
    // and exercise the whole key path: input bytes -> conhost key records ->
    // ENABLE_VIRTUAL_TERMINAL_INPUT translation -> the client's read.
    //
    // This regression exists because the client originally read through
    // `std::io::stdin()`, which on Windows goes via `ReadConsoleW` — the
    // legacy console path that does not deliver VT input. The prefix fell
    // straight through to the session, where PSReadLine treated Ctrl-B as
    // backward-char, and detaching was impossible.
    let inner = start_session("kbinner");
    let outer = start_session("kbouter");

    wmux::client::capture_wait(&inner.name, "PS ", SHELL_TIMEOUT).expect("inner shell");
    wmux::client::capture_wait(&outer.name, "PS ", SHELL_TIMEOUT).expect("outer shell");

    // Split at the source so the marker only ever appears in *output*, never
    // in the echoed command.
    let marker = format!("KBMARK{}", std::process::id());
    let (head, tail) = marker.split_at(6);
    wmux::client::send_keys(
        &inner.name,
        &format!("Write-Output ('{head}' + '{tail}')\r"),
    )
    .expect("mark inner");
    wmux::client::capture_wait(&inner.name, &marker, Duration::from_secs(30))
        .expect("inner should show its marker");

    // Attach to inner from inside outer.
    let exe = wmux_exe();
    wmux::client::send_keys(
        &outer.name,
        &format!("& '{}' attach {}\r", exe.display(), inner.name),
    )
    .expect("start attach inside outer");

    // Outer's screen is now inner's screen, which proves the repaint landed.
    wmux::client::capture_wait(&outer.name, &marker, Duration::from_secs(45))
        .expect("attaching should repaint inner's screen onto outer");

    // The actual test: Ctrl-B, then d.
    wmux::client::send_keys(&outer.name, "\u{2}d").expect("send the detach sequence");

    let screen = wmux::client::capture_wait(&outer.name, "[detached from", Duration::from_secs(45))
        .expect("the prefix must be intercepted by the client, not forwarded to the session");
    assert!(
        screen.contains("[detached from"),
        "expected a detach notice, got:\n{screen}"
    );

    assert!(
        session::session_exists(&inner.name).expect("list sessions"),
        "detaching must leave the session running"
    );
}

#[test]
fn a_lone_prefix_is_swallowed_rather_than_reaching_the_shell() {
    // If the prefix leaked through, PSReadLine would act on it. Sending the
    // prefix followed by Enter must leave the command line untouched, so the
    // shell just sees a bare Enter and redraws its prompt.
    let inner = start_session("kbsolo");
    let outer = start_session("kbsoloout");

    wmux::client::capture_wait(&inner.name, "PS ", SHELL_TIMEOUT).expect("inner shell");
    wmux::client::capture_wait(&outer.name, "PS ", SHELL_TIMEOUT).expect("outer shell");

    // Mark inner so we can tell its screen from outer's. Waiting on "PS "
    // would match outer's *own* prompt and race ahead of the attach.
    let marker = format!("SOLOMARK{}", std::process::id());
    let (head, tail) = marker.split_at(8);
    wmux::client::send_keys(
        &inner.name,
        &format!("Write-Output ('{head}' + '{tail}')\r"),
    )
    .expect("mark inner");
    wmux::client::capture_wait(&inner.name, &marker, Duration::from_secs(30))
        .expect("inner should show its marker");

    let exe = wmux_exe();
    wmux::client::send_keys(
        &outer.name,
        &format!("& '{}' attach {}\r", exe.display(), inner.name),
    )
    .expect("start attach inside outer");
    wmux::client::capture_wait(&outer.name, &marker, Duration::from_secs(45))
        .expect("attach should repaint inner's screen onto outer");

    // A lone prefix arms the detector and forwards nothing.
    wmux::client::send_keys(&outer.name, "\u{2}").expect("send a bare prefix");
    std::thread::sleep(Duration::from_secs(2));

    // Then 'd' completes the sequence even though it arrived in a later read.
    wmux::client::send_keys(&outer.name, "d").expect("complete the sequence");
    wmux::client::capture_wait(&outer.name, "[detached from", Duration::from_secs(45))
        .expect("a prefix split across reads must still detach");
}

#[test]
fn detach_from_outside_works_without_any_keyboard() {
    // The escape hatch: if the prefix key cannot be delivered at all, a second
    // terminal can still free the attached client.
    let inner = start_session("extinner");
    let outer = start_session("extouter");

    wmux::client::capture_wait(&inner.name, "PS ", SHELL_TIMEOUT).expect("inner shell");
    wmux::client::capture_wait(&outer.name, "PS ", SHELL_TIMEOUT).expect("outer shell");

    let marker = format!("EXTMARK{}", std::process::id());
    let (head, tail) = marker.split_at(7);
    wmux::client::send_keys(
        &inner.name,
        &format!("Write-Output ('{head}' + '{tail}')\r"),
    )
    .expect("mark inner");
    wmux::client::capture_wait(&inner.name, &marker, Duration::from_secs(30)).expect("marked");

    let exe = wmux_exe();
    wmux::client::send_keys(
        &outer.name,
        &format!("& '{}' attach {}\r", exe.display(), inner.name),
    )
    .expect("attach from outer");
    wmux::client::capture_wait(&outer.name, &marker, Duration::from_secs(45)).expect("attached");

    // No keystrokes involved at all.
    wmux::client::detach_clients(&inner.name).expect("detach from outside");

    wmux::client::capture_wait(&outer.name, "[detached from", Duration::from_secs(45))
        .expect("the attached client should have returned to its shell");
    assert!(
        session::session_exists(&inner.name).expect("list sessions"),
        "an external detach must leave the session running"
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
