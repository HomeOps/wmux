//! The session server: a detached process that owns the session's console and
//! outlives every terminal window that attaches to it.
//!
//! Two backends host the program:
//!
//! * **ConPTY** (the default). The server keeps a `vt100::Parser` fed with
//!   everything the pty emits. That parser is the reason reattach works at
//!   all: ConPTY hands us a stream of VT bytes, and once those bytes have gone
//!   past there is no way to ask Windows what the screen currently looks like.
//!   By replaying the stream into a terminal model we can render the *current*
//!   screen for a client that shows up ten minutes later.
//!   The same model also serves a **default-terminal handoff**, where Windows
//!   Terminal's OpenConsole is the pseudoconsole and wmux only receives its
//!   pipes; see [`crate::handoff`].
//! * **Legacy console** (`--legacy`). A real conhost with a hidden window, for
//!   programs that cannot run under a pseudoconsole. The screen is read
//!   straight from the console buffer; see [`crate::legacy`].

use anyhow::{Context, Result};
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use crate::keys::Key;
use crate::legacy::Legacy;
use crate::pipe::{PipeConn, PipeListener};
use crate::protocol::{ClientMsg, ServerMsg};
use crate::session;
use crate::snapshot::Snapshot;

/// How much scrollback the session model retains, in lines.
const SCROLLBACK_LINES: usize = 10_000;

/// Read chunk for pty output.
const PTY_CHUNK: usize = 8 * 1024;

/// How many messages may be queued for a client before it is considered stuck.
///
/// A client that cannot keep up is dropped rather than allowed to stall the
/// session. Writing straight to the pipe from the pty pump would block once the
/// 64 KB pipe buffer filled, freezing output for *every* client and wedging the
/// session — which is exactly what happened before this queue existed.
const OUTBOUND_QUEUE: usize = 1024;

/// How long a handed-off session waits, after its program exits, for the
/// console host to deliver the last of its output.
const FINAL_OUTPUT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// What `attach` says about a legacy session. Interactive attach needs a
/// renderer that turns the console buffer into VT and key records back into
/// input; until that exists, say so plainly rather than show a blank screen.
const LEGACY_ATTACH_UNSUPPORTED: &str = "attach is not supported for --legacy sessions yet; \
     drive the session with `wmux key`, `wmux send` and `wmux capture`";

/// How the session's program is hosted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Pty,
    /// A hidden real console, with `history` rows of scrollback above the
    /// window.
    Legacy {
        history: u16,
    },
}

struct Client {
    id: u64,
    outbound: std::sync::mpsc::SyncSender<ServerMsg>,
}

/// What a session does to its pseudoconsole besides reading and writing it.
///
/// Implemented once for a ConPTY wmux created itself and once for one handed
/// over by Windows, so the terminal model, repaint and capture paths are
/// shared rather than duplicated.
pub(crate) trait PtyControl: Send {
    /// `(cols, rows)`, the wire order.
    fn resize(&mut self, cols: u16, rows: u16) -> Result<()>;
    fn kill(&mut self);
    /// Closes the pseudoconsole once its program has ended. Runs only for a
    /// session that is kept after a failure.
    fn release(&mut self) {}
}

struct OwnedPty {
    master: Box<dyn portable_pty::MasterPty + Send>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
}

impl PtyControl for OwnedPty {
    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        // PtySize is a struct, so the order cannot be transposed here.
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
    }

    fn kill(&mut self) {
        let _ = self.killer.kill();
    }
}

struct Pty {
    parser: Mutex<vt100::Parser>,
    writer: Mutex<Box<dyn std::io::Write + Send>>,
    control: Mutex<Box<dyn PtyControl>>,
    size: Mutex<(u16, u16)>,
}

/// What happens when a session's program ends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExitPolicy {
    /// Keep the session after a non-zero exit, so its last screen can still
    /// be listed, captured and attached to until someone kills it. Nobody
    /// watches a program wmux hosts as the default terminal, so this is the
    /// only way its failure is ever seen.
    pub keep_failed: bool,
}

enum Backend {
    // Boxed: the pty side is several mutexes, the legacy side one handle.
    Pty(Box<Pty>),
    Legacy(Legacy),
}

struct Session {
    backend: Backend,
    clients: Mutex<Vec<Client>>,
    command: String,
    next_client_id: AtomicU64,
    policy: ExitPolicy,
    /// Set once the program has ended in a session that is kept.
    exited: Mutex<Option<i32>>,
    /// `wmux kill` ended the program, so its exit code is ours, not a failure.
    killed: AtomicBool,
}

impl Session {
    fn new(backend: Backend, command: String, policy: ExitPolicy) -> Arc<Session> {
        Arc::new(Session {
            backend,
            clients: Mutex::new(Vec::new()),
            command,
            next_client_id: AtomicU64::new(1),
            policy,
            exited: Mutex::new(None),
            killed: AtomicBool::new(false),
        })
    }

    fn exit_code(&self) -> Option<i32> {
        *self.exited.lock().unwrap()
    }

    /// Whether a program that ended with `code` leaves the session behind.
    fn keeps(&self, code: i32) -> bool {
        code != 0 && self.policy.keep_failed && !self.killed.load(Ordering::SeqCst)
    }

    /// The program has ended. Either the server exits with it, or, for a
    /// failure in a session that keeps failures, it stays up holding the
    /// final screen.
    fn finish(&self, code: i32) {
        self.broadcast(&ServerMsg::Exited(code));
        if self.keeps(code) {
            log(&format!(
                "program failed with code {code}; keeping the session"
            ));
            *self.exited.lock().unwrap() = Some(code);
            // Attached clients have been told; they return to their shell.
            self.clients.lock().unwrap().clear();
            if let Backend::Pty(pty) = &self.backend {
                pty.control.lock().unwrap().release();
            }
            return;
        }
        // Let the per-client writer threads drain before the pipe disappears.
        std::thread::sleep(std::time::Duration::from_millis(300));
        std::process::exit(code);
    }

    /// Queues a message for every attached client.
    ///
    /// Never blocks: each client owns a bounded queue drained by its own
    /// writer thread. A client that fills its queue is dropped, because one
    /// unresponsive terminal must not be able to freeze the session.
    fn broadcast(&self, msg: &ServerMsg) {
        let mut dead = Vec::new();
        {
            let clients = self.clients.lock().unwrap();
            for client in clients.iter() {
                if client.outbound.try_send(msg.clone()).is_err() {
                    dead.push(client.id);
                }
            }
        }
        if !dead.is_empty() {
            log(&format!("dropping unresponsive clients: {dead:?}"));
            let mut clients = self.clients.lock().unwrap();
            clients.retain(|c| !dead.contains(&c.id));
        }
    }

    fn resize(&self, cols: u16, rows: u16) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        match &self.backend {
            Backend::Pty(pty) => {
                {
                    let mut size = pty.size.lock().unwrap();
                    if *size == (cols, rows) {
                        return;
                    }
                    *size = (cols, rows);
                }
                // vt100 takes (rows, cols).
                pty.parser.lock().unwrap().set_size(rows, cols);
                if self.exit_code().is_none() {
                    if let Err(e) = pty.control.lock().unwrap().resize(cols, rows) {
                        log(&format!("pty resize to {cols}x{rows} failed: {e:#}"));
                    }
                }
            }
            // A refused size is visible to the caller through `Info`, which
            // reports what the console actually is.
            Backend::Legacy(legacy) => {
                if let Err(e) = legacy.resize(cols, rows) {
                    log(&format!("legacy resize to {cols}x{rows} failed: {e:#}"));
                }
            }
        }
    }

    fn size(&self) -> (u16, u16) {
        match &self.backend {
            Backend::Pty(pty) => *pty.size.lock().unwrap(),
            Backend::Legacy(legacy) => legacy.size().unwrap_or((0, 0)),
        }
    }

    /// Typed input. A legacy console reads key records, so text is converted
    /// to the keypresses that would have produced it.
    fn input(&self, bytes: &[u8]) -> Result<()> {
        if let Some(code) = self.exit_code() {
            anyhow::bail!("the program exited with code {code}; nothing is reading input");
        }
        match &self.backend {
            Backend::Pty(pty) => {
                use std::io::Write;
                let mut writer = pty.writer.lock().unwrap();
                writer.write_all(bytes)?;
                writer.flush()?;
                Ok(())
            }
            Backend::Legacy(legacy) => {
                legacy.send_keys(&Key::from_text(&String::from_utf8_lossy(bytes)))
            }
        }
    }

    fn keys(&self, keys: &[Key]) -> Result<()> {
        match &self.backend {
            Backend::Pty(_) => {
                let bytes: Vec<u8> = keys.iter().flat_map(Key::to_vt).collect();
                self.input(&bytes)
            }
            Backend::Legacy(legacy) => legacy.send_keys(keys),
        }
    }

    fn snapshot(&self, scrollback: usize) -> Result<Snapshot> {
        match &self.backend {
            Backend::Pty(pty) => Ok(Snapshot::from_vt100(
                &mut pty.parser.lock().unwrap(),
                scrollback,
            )),
            Backend::Legacy(legacy) => legacy.snapshot(scrollback),
        }
    }

    /// The escape sequence blob that reproduces the current screen.
    fn repaint(&self) -> Option<Vec<u8>> {
        match &self.backend {
            Backend::Pty(pty) => Some(pty.parser.lock().unwrap().screen().contents_formatted()),
            Backend::Legacy(_) => None,
        }
    }

    /// The visible screen as plain text, for callers with no terminal.
    fn capture_text(&self) -> String {
        match &self.backend {
            Backend::Pty(pty) => pty.parser.lock().unwrap().screen().contents(),
            Backend::Legacy(legacy) => legacy
                .snapshot(0)
                .map(|s| s.to_text())
                .unwrap_or_else(|e| format!("[wmux: could not read the console: {e:#}]")),
        }
    }

    fn kill(&self) {
        // A kept session has no program left; killing it removes the session.
        if let Some(code) = self.exit_code() {
            log("kept session killed");
            std::process::exit(code);
        }
        self.killed.store(true, Ordering::SeqCst);
        match &self.backend {
            Backend::Pty(pty) => pty.control.lock().unwrap().kill(),
            Backend::Legacy(legacy) => legacy.kill(),
        }
    }

    fn attach(&self, conn: Arc<PipeConn>) -> u64 {
        let id = self.next_client_id.fetch_add(1, Ordering::Relaxed);
        let (outbound, inbox) = std::sync::mpsc::sync_channel::<ServerMsg>(OUTBOUND_QUEUE);

        // One writer thread per client. Blocking here only ever affects the
        // client that is not reading.
        std::thread::spawn(move || {
            while let Ok(msg) = inbox.recv() {
                if msg.write_to(&mut &*conn).is_err() {
                    break;
                }
            }
        });

        self.clients.lock().unwrap().push(Client { id, outbound });
        id
    }

    fn detach(&self, id: u64) {
        self.clients.lock().unwrap().retain(|c| c.id != id);
    }

    fn client_count(&self) -> u32 {
        self.clients.lock().unwrap().len() as u32
    }
}

/// Runs a session server in the current process. Never returns normally: it
/// exits the process when the child terminates or a client asks it to.
pub fn run(
    name: &str,
    command: &[String],
    cols: u16,
    rows: u16,
    mode: Mode,
    policy: ExitPolicy,
) -> Result<()> {
    // Claim the name first. If another server already owns it we want to fail
    // now rather than after spawning a shell nobody can reach.
    let listener = bind(name)?;

    let cols = cols.max(1);
    let rows = rows.max(1);

    let sess = match mode {
        Mode::Pty => start_pty(name, command, cols, rows, policy)?,
        Mode::Legacy { history } => start_legacy(name, command, cols, rows, history, policy)?,
    };
    serve(listener, sess)
}

/// A pseudoconsole somebody else created, handed to wmux to host.
pub(crate) struct ForeignPty {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn std::io::Write + Send>,
    pub control: Box<dyn PtyControl>,
    /// Blocks until the program ends and returns its exit code.
    pub wait: Box<dyn FnOnce() -> i32 + Send>,
}

/// Hosts a pseudoconsole that was created elsewhere: the default-terminal
/// handoff, where OpenConsole owns the console and wmux gets its pipes.
pub(crate) fn run_foreign(
    listener: PipeListener,
    pty: ForeignPty,
    cols: u16,
    rows: u16,
    command: String,
    policy: ExitPolicy,
) -> Result<()> {
    let cols = cols.max(1);
    let rows = rows.max(1);
    let mut control = pty.control;
    // The console was created at whatever size its program asked for. Make it
    // agree with the model before the first byte is parsed.
    if let Err(e) = control.resize(cols, rows) {
        log(&format!("initial resize to {cols}x{rows} failed: {e:#}"));
    }
    let sess = Session::new(
        Backend::Pty(Box::new(Pty {
            parser: Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK_LINES)),
            writer: Mutex::new(pty.writer),
            control: Mutex::new(control),
            size: Mutex::new((cols, rows)),
        })),
        command,
        policy,
    );
    let pump = spawn_pty_pump(sess.clone(), pty.reader);
    let reaper = sess.clone();
    let wait = pty.wait;
    std::thread::spawn(move || {
        let code = wait();
        log(&format!("handed-off program exited with code {code}"));
        // The program can exit before its last output has crossed the pipe.
        // A kept session exists to show that output, so wait for the console
        // host to close the pipe, within reason.
        let deadline = std::time::Instant::now() + FINAL_OUTPUT_WAIT;
        while !pump.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        reaper.finish(code);
    });
    serve(listener, sess)
}

/// Claims a session name by creating its pipe.
pub(crate) fn bind(name: &str) -> Result<PipeListener> {
    session::validate_name(name)?;
    let sid = session::current_user_sid()?;
    let path = session::pipe_path_for(&sid, name);
    let listener = PipeListener::bind(&path, &sid)
        .with_context(|| format!("session {name:?} is already running"))?;
    log(&format!("listening on {}", listener.path()));
    Ok(listener)
}

fn serve(mut listener: PipeListener, sess: Arc<Session>) -> Result<()> {
    // Accept loop. Each client gets a thread; there are never many.
    loop {
        match listener.accept() {
            Ok(conn) => {
                let sess = sess.clone();
                std::thread::spawn(move || {
                    if let Err(e) = serve_client(sess, conn) {
                        log(&format!("client handler ended: {e:#}"));
                    }
                });
            }
            Err(e) => {
                log(&format!("accept failed: {e:#}"));
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

fn start_pty(
    name: &str,
    command: &[String],
    cols: u16,
    rows: u16,
    policy: ExitPolicy,
) -> Result<Arc<Session>> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to create a pseudoconsole")?;

    let mut builder = CommandBuilder::new(&command[0]);
    for arg in &command[1..] {
        builder.arg(arg);
    }
    if let Ok(cwd) = std::env::current_dir() {
        builder.cwd(cwd);
    }
    // Let programs inside the session know they are hosted by wmux, and give
    // them a terminal type that implies VT support.
    builder.env(session::SESSION_ENV, name);
    builder.env("TERM", "xterm-256color");

    let child = pair
        .slave
        .spawn_command(builder)
        .with_context(|| format!("failed to start {:?}", command[0]))?;
    // Dropping the slave here is what lets the pty signal EOF once the child
    // and all its descendants have exited.
    drop(pair.slave);

    let killer = child.clone_killer();
    let reader = pair
        .master
        .try_clone_reader()
        .context("failed to clone the pty reader")?;
    let writer = pair
        .master
        .take_writer()
        .context("failed to take the pty writer")?;

    let sess = Session::new(
        Backend::Pty(Box::new(Pty {
            parser: Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK_LINES)),
            writer: Mutex::new(writer),
            control: Mutex::new(Box::new(OwnedPty {
                master: pair.master,
                killer,
            })),
            size: Mutex::new((cols, rows)),
        })),
        command.join(" "),
        policy,
    );

    spawn_pty_pump(sess.clone(), reader);
    spawn_child_reaper(sess.clone(), child);
    Ok(sess)
}

fn start_legacy(
    name: &str,
    command: &[String],
    cols: u16,
    rows: u16,
    history: u16,
    policy: ExitPolicy,
) -> Result<Arc<Session>> {
    // The console host and the program inherit this process's environment.
    std::env::set_var(session::SESSION_ENV, name);
    let legacy = Legacy::spawn(command, cols, rows, history)?;
    let sess = Session::new(
        Backend::Legacy(legacy),
        format!("[legacy] {}", command.join(" ")),
        policy,
    );

    let reaper = sess.clone();
    std::thread::spawn(move || {
        let Backend::Legacy(legacy) = &reaper.backend else {
            unreachable!()
        };
        let code = legacy.wait();
        log(&format!("legacy program exited with code {code}"));
        reaper.finish(code);
    });
    Ok(sess)
}

/// Streams pty output into the terminal model and out to attached clients.
fn spawn_pty_pump(
    sess: Arc<Session>,
    mut reader: Box<dyn Read + Send>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let Backend::Pty(pty) = &sess.backend else {
            return;
        };
        let mut buf = vec![0u8; PTY_CHUNK];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = &buf[..n];
                    // Update the model first so a client attaching right now
                    // cannot observe a screen that is behind the live stream.
                    pty.parser.lock().unwrap().process(chunk);
                    sess.broadcast(&ServerMsg::Output(chunk.to_vec()));
                }
                Err(e) => {
                    log(&format!("pty read failed: {e}"));
                    break;
                }
            }
        }
    })
}

/// Waits for the child and tears the session down when it exits.
fn spawn_child_reaper(sess: Arc<Session>, mut child: Box<dyn portable_pty::Child + Send + Sync>) {
    std::thread::spawn(move || {
        let code = match child.wait() {
            Ok(status) => status.exit_code() as i32,
            Err(e) => {
                log(&format!("waiting for the child failed: {e}"));
                -1
            }
        };
        if sess.keeps(code) {
            // ConPTY can still be rendering the program's last output; a kept
            // session exists to show it. The pipe never reaches EOF here (the
            // pty stays open), so there is nothing better to wait on.
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        sess.finish(code);
    });
}

fn serve_client(sess: Arc<Session>, conn: PipeConn) -> Result<()> {
    let conn = Arc::new(conn);
    let mut attached: Option<u64> = None;

    let result = (|| -> Result<()> {
        loop {
            let msg = match ClientMsg::read_from(&mut &*conn) {
                Ok(m) => m,
                // Peer closed: treat as an implicit detach.
                Err(_) => return Ok(()),
            };
            match msg {
                ClientMsg::Attach { cols, rows } => {
                    let Some(screen) = sess.repaint() else {
                        ServerMsg::Error(LEGACY_ATTACH_UNSUPPORTED.into()).write_to(&mut &*conn)?;
                        return Ok(());
                    };
                    sess.resize(cols, rows);
                    // Repaint after the resize, and before registering, so the
                    // client sees exactly one copy of the current screen and
                    // then only subsequent output.
                    let screen = sess.repaint().unwrap_or(screen);
                    ServerMsg::Repaint(screen).write_to(&mut &*conn)?;
                    // A kept session shows its last screen and how it ended,
                    // then lets the client go: there is nothing left to drive.
                    if let Some(code) = sess.exit_code() {
                        ServerMsg::Exited(code).write_to(&mut &*conn)?;
                        return Ok(());
                    }
                    attached = Some(sess.attach(conn.clone()));
                }
                ClientMsg::Input(bytes) => sess.input(&bytes)?,
                ClientMsg::Keys(keys) => {
                    if let Err(e) = sess.keys(&keys) {
                        log(&format!("sending keys failed: {e:#}"));
                    }
                }
                ClientMsg::Resize { cols, rows } => sess.resize(cols, rows),
                ClientMsg::Detach => return Ok(()),
                ClientMsg::Kill => {
                    sess.kill();
                    return Ok(());
                }
                ClientMsg::DetachClients => {
                    let n = sess.client_count();
                    log(&format!("detaching {n} client(s) on request"));
                    sess.broadcast(&ServerMsg::Detached);
                    // Let the writer threads deliver the notice before the
                    // client list is torn down.
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    sess.clients.lock().unwrap().clear();
                }
                ClientMsg::Capture => {
                    let text = sess.capture_text();
                    ServerMsg::CaptureReply(text).write_to(&mut &*conn)?;
                }
                ClientMsg::Snapshot { scrollback } => {
                    let reply = match sess.snapshot(scrollback as usize) {
                        Ok(snap) => ServerMsg::Snapshot(snap),
                        Err(e) => ServerMsg::Error(format!("{e:#}")),
                    };
                    reply.write_to(&mut &*conn)?;
                }
                ClientMsg::Info => {
                    let (cols, rows) = sess.size();
                    ServerMsg::InfoReply {
                        cols,
                        rows,
                        clients: sess.client_count(),
                        exited: sess.exit_code(),
                        command: sess.command.clone(),
                    }
                    .write_to(&mut &*conn)?;
                }
            }
        }
    })();

    if let Some(id) = attached {
        sess.detach(id);
    }
    result
}

/// Appends a line to `%LOCALAPPDATA%\wmux\wmux.log` when `WMUX_LOG` is set.
///
/// A detached server has no console, so this is the only way to see what it
/// is doing. Off by default to avoid writing to disk during normal use.
pub fn log(message: &str) {
    if std::env::var_os("WMUX_LOG").is_none() {
        return;
    }
    let Some(dir) = std::env::var_os("LOCALAPPDATA") else {
        return;
    };
    let dir = std::path::Path::new(&dir).join("wmux");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("wmux.log"))
    {
        let _ = writeln!(f, "[{}] {}", std::process::id(), message);
    }
}
