//! wmux — terminal session persistence for Windows.
//!
//! Detach a console session, close the window, reattach later. wmux
//! deliberately does *not* implement panes, splits, or a status bar: Windows
//! Terminal already does those well. The one thing Windows lacks is a session
//! that outlives its window, and that is all this tool provides.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use wmux::{client, console, protocol, run, server, session};

/// Creation flags for the detached server process.
///
/// `DETACHED_PROCESS` is the whole trick: the server gets no console at all,
/// so when the terminal window that launched it is closed, the `CTRL_CLOSE_EVENT`
/// conhost sends to that console never reaches the server.
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
/// Escapes a job object that would otherwise kill the server when the parent
/// terminal exits. Not every job permits breakaway, so this is best-effort.
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// Default screen for `wmux new -d`. A legacy console that cannot be this
/// large (the limit depends on the monitor and console font) is clamped to
/// the largest size Windows allows.
const DETACHED_SIZE: console::Size = console::Size {
    cols: 200,
    rows: 60,
};

/// How long `wmux new` waits for the server to publish its pipe.
const SERVER_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Parser)]
#[command(
    name = "wmux",
    version,
    about = "Terminal session persistence for Windows",
    long_about = "Detach a console session, close the window, reattach later.\n\n\
                  Inside a session, press the tmux prefix Ctrl-B then d to detach.\n\
                  Press Ctrl-B twice to send a literal Ctrl-B.\n\
                  Set WMUX_PREFIX (for example ^A) to rebind the prefix."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a session and attach to it.
    New {
        /// Session name. Defaults to the next free wmux-N.
        #[arg(short = 's', long = "session")]
        name: Option<String>,

        /// Create the session but do not attach.
        #[arg(short = 'd', long = "detached")]
        detached: bool,

        /// Width in columns. Defaults to the current terminal, or 200 with -d.
        #[arg(short = 'x', long = "cols")]
        cols: Option<u16>,

        /// Height in rows. Defaults to the current terminal, or 60 with -d.
        #[arg(short = 'y', long = "rows")]
        rows: Option<u16>,

        /// Host the program in a real, hidden console instead of a ConPTY.
        ///
        /// For programs written against the classic console API that fail or
        /// draw nothing under a pseudoconsole, typically because they need a
        /// real console window. Drive them with `key`, `send` and `capture`;
        /// interactive attach is not supported yet.
        #[arg(long)]
        legacy: bool,

        /// Rows of scrollback above the window in a --legacy console.
        /// ConPTY sessions always keep 10000 lines.
        #[arg(long, default_value_t = 0, requires = "legacy")]
        history: u16,

        /// Program to run. Defaults to $WMUX_SHELL, else pwsh.exe.
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },

    /// List running sessions.
    #[command(alias = "list")]
    Ls,

    /// Attach to an existing session.
    #[command(alias = "a")]
    Attach {
        /// Session name. Defaults to the only running session, if there is one.
        name: Option<String>,
    },

    /// Send input to a session without attaching to it.
    ///
    /// Useful from scripts and other programs that have no console of their
    /// own. Returns once the session has consumed the input.
    Send {
        /// Session name.
        #[arg(short = 't', long = "session")]
        name: String,

        /// Do not append Enter to the input.
        #[arg(long)]
        no_enter: bool,

        /// Text to type into the session.
        #[arg(trailing_var_arg = true, required = true)]
        keys: Vec<String>,
    },

    /// Press named keys in a session: F1, Alt-F10, C-x, Down, PgUp, Enter, ...
    ///
    /// Modifiers are Ctrl-/C-, Alt-/M- and Shift-/S-. A single character is
    /// pressed literally. Returns once the session has consumed the keys.
    Key {
        /// Session name.
        #[arg(short = 't', long = "session")]
        name: String,

        /// Keys to press, in order.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        keys: Vec<String>,
    },

    /// Resize a session without attaching.
    Resize {
        /// Session name.
        #[arg(short = 't', long = "session")]
        name: String,

        /// Width in columns.
        #[arg(short = 'x', long = "cols")]
        cols: u16,

        /// Height in rows.
        #[arg(short = 'y', long = "rows")]
        rows: u16,
    },

    /// Run a command in a PowerShell session and print its serialised result.
    ///
    /// Unlike `capture`, the result does not travel over the terminal: the
    /// session writes it to a temporary file, so it is neither wrapped to the
    /// terminal width nor limited to what fits on screen.
    Run {
        /// Session name.
        #[arg(short = 't', long = "session")]
        name: String,

        /// Serialisation format: clixml, json, or text.
        #[arg(long, default_value = "clixml")]
        format: run::Format,

        /// Serialisation depth for clixml and json.
        ///
        /// Defaults to 2, matching Export-Clixml. Raising this on rich objects
        /// such as files or processes can produce gigabytes, because their
        /// graphs are recursive; prefer Select-Object to pick properties.
        #[arg(long, default_value_t = run::DEFAULT_DEPTH)]
        depth: u32,

        /// Seconds to wait for the command to finish.
        #[arg(long, default_value_t = 120)]
        timeout: u64,

        /// The PowerShell command to run.
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },

    /// Print a session's visible screen as plain text.
    Capture {
        /// Session name.
        #[arg(short = 't', long = "session")]
        name: String,

        /// Poll until this text appears on screen.
        #[arg(long)]
        wait_for: Option<String>,

        /// Seconds to wait when --wait-for is given.
        #[arg(long, default_value_t = 15)]
        timeout: u64,

        /// Output format. `json` adds a console attribute per cell (two hex
        /// digits, background then foreground, e.g. 1E is yellow on blue)
        /// and the cursor position. `ansi` redraws the screen in colour:
        /// pipe it to Write-Host or save it and `Get-Content -Raw` it later.
        #[arg(long, default_value = "text")]
        format: CaptureFormat,

        /// Include up to this many rows of scrollback above the screen.
        #[arg(short = 'S', long, default_value_t = 0)]
        scrollback: u32,
    },

    /// Terminate a session and its child process.
    Kill {
        /// Session name.
        name: String,
    },

    /// Detach the terminal viewing a session, leaving the session running.
    ///
    /// With no arguments this detaches the session you are currently inside,
    /// so `wmux detach` typed at a session's own prompt does the same thing as
    /// the prefix key. Naming a session detaches it from another terminal,
    /// which is the way out when the prefix key is not usable.
    Detach {
        /// Session name. Defaults to the session this command runs inside.
        name: Option<String>,
    },

    /// Diagnostic: show the raw bytes the console delivers for each keypress.
    ///
    /// Use this when a key binding is not being recognised. It reads the
    /// console exactly the way `wmux attach` does, so whatever it prints is
    /// what the detach detector sees.
    Keys,

    /// Internal: run the session server in this process.
    #[command(hide = true)]
    Server {
        #[arg(long)]
        name: String,
        #[arg(long, default_value_t = 80)]
        cols: u16,
        #[arg(long, default_value_t = 24)]
        rows: u16,
        #[arg(long)]
        legacy: bool,
        #[arg(long, default_value_t = 0)]
        history: u16,
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },

    /// Internal: the trampoline inside a legacy session's hidden console.
    #[command(hide = true)]
    ConsoleHost {
        #[arg(long)]
        cols: u16,
        #[arg(long)]
        rows: u16,
        #[arg(long, default_value_t = 0)]
        history: u16,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum CaptureFormat {
    Text,
    Json,
    Ansi,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("wmux: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::New {
            name,
            detached,
            cols,
            rows,
            legacy,
            history,
            command,
        } => {
            let mode = if legacy {
                server::Mode::Legacy { history }
            } else {
                server::Mode::Pty
            };
            cmd_new(name, detached, cols, rows, mode, command)
        }
        Command::Key { name, keys } => cmd_key(&name, &keys),
        Command::Resize { name, cols, rows } => cmd_resize(&name, cols, rows),
        Command::Ls => cmd_ls(),
        Command::Attach { name } => cmd_attach(name),
        Command::Send {
            name,
            no_enter,
            keys,
        } => cmd_send(&name, &keys, no_enter),
        Command::Run {
            name,
            format,
            depth,
            timeout,
            command,
        } => cmd_run(&name, &command, format, depth, timeout),
        Command::Capture {
            name,
            wait_for,
            timeout,
            format,
            scrollback,
        } => cmd_capture(&name, wait_for.as_deref(), timeout, format, scrollback),
        Command::Kill { name } => cmd_kill(&name),
        Command::Detach { name } => cmd_detach(name),
        Command::Keys => cmd_keys(),
        Command::Server {
            name,
            cols,
            rows,
            legacy,
            history,
            command,
        } => {
            let mode = if legacy {
                server::Mode::Legacy { history }
            } else {
                server::Mode::Pty
            };
            server::run(&name, &command, cols, rows, mode)
        }
        Command::ConsoleHost {
            cols,
            rows,
            history,
            command,
        } => {
            let code = wmux::legacy::host_main(cols, rows, history, &command)?;
            std::process::exit(code)
        }
    }
}

fn cmd_new(
    name: Option<String>,
    detached: bool,
    cols: Option<u16>,
    rows: Option<u16>,
    mode: server::Mode,
    command: Vec<String>,
) -> Result<()> {
    let name = match name {
        Some(n) => {
            session::validate_name(&n)?;
            n
        }
        None => session::next_free_name()?,
    };

    if session::session_exists(&name).unwrap_or(false) {
        bail!("a session named {name:?} is already running; use `wmux attach {name}`");
    }

    let command = if command.is_empty() {
        vec![default_shell()]
    } else {
        command
    };

    // An attached session takes the size of the terminal it opens in. A
    // detached one has no terminal worth copying (often there is none at all,
    // and the console fallback is a cramped 80x24), so it gets a large screen
    // that full-screen programs and `capture` can make use of. Attaching later
    // resizes it to the attaching terminal either way.
    let size = if detached {
        DETACHED_SIZE
    } else {
        console::console_size()
    };
    let cols = cols.unwrap_or(size.cols).max(1);
    let rows = rows.unwrap_or(size.rows).max(1);
    spawn_detached_server(&name, &command, cols, rows, mode)?;
    wait_for_session(&name)?;

    if detached {
        println!("{name}");
        return Ok(());
    }
    finish_attach(&name, client::attach(&name)?)
}

fn cmd_ls() -> Result<()> {
    let names = session::list_sessions()?;
    if names.is_empty() {
        println!("no sessions");
        return Ok(());
    }
    for name in names {
        match client::query_info(&name) {
            Ok(protocol::ServerMsg::InfoReply {
                cols,
                rows,
                clients,
                command,
            }) => {
                let attached = if clients > 0 { "attached" } else { "detached" };
                println!("{name}\t{cols}x{rows}\t{attached}\t{command}");
            }
            // The session went away between listing and querying, or is busy.
            _ => println!("{name}\t?"),
        }
    }
    Ok(())
}

fn cmd_attach(name: Option<String>) -> Result<()> {
    let name = match name {
        Some(n) => n,
        None => {
            let mut names = session::list_sessions()?;
            match names.len() {
                0 => bail!("no sessions are running; start one with `wmux new`"),
                1 => names.remove(0),
                n => bail!(
                    "{n} sessions are running; name one of: {}",
                    names.join(", ")
                ),
            }
        }
    };
    finish_attach(&name, client::attach(&name)?)
}

fn finish_attach(name: &str, outcome: client::Outcome) -> Result<()> {
    match outcome {
        client::Outcome::Detached => {
            println!("[detached from {name}]");
            Ok(())
        }
        client::Outcome::Exited(code) => {
            println!("[session {name} exited with code {code}]");
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
    }
}

fn cmd_send(name: &str, keys: &[String], no_enter: bool) -> Result<()> {
    let mut text = keys.join(" ");
    if !no_enter {
        // CR, not LF: that is what a terminal sends for Enter.
        text.push('\r');
    }
    client::send_keys(name, &text)
}

fn cmd_run(
    name: &str,
    command: &[String],
    format: run::Format,
    depth: u32,
    timeout_secs: u64,
) -> Result<()> {
    let command = command.join(" ");
    let output = run::run(
        name,
        &command,
        format,
        depth,
        std::time::Duration::from_secs(timeout_secs),
    )?;
    print!("{output}");
    if !output.ends_with('\n') {
        println!();
    }
    Ok(())
}

fn cmd_key(name: &str, specs: &[String]) -> Result<()> {
    let keys = specs
        .iter()
        .map(|s| wmux::keys::Key::parse(s).map_err(anyhow::Error::msg))
        .collect::<Result<Vec<_>>>()?;
    client::send_key_events(name, &keys)
}

fn cmd_resize(name: &str, cols: u16, rows: u16) -> Result<()> {
    let (got_cols, got_rows) = client::resize(name, cols, rows)?;
    if (got_cols, got_rows) != (cols, rows) {
        bail!(
            "session {name:?} is {got_cols}x{got_rows}, not the requested {cols}x{rows} \
             (a legacy console is capped by the screen and font size, and an attached \
             client resizes the session to its own terminal)"
        );
    }
    println!("{name}\t{got_cols}x{got_rows}");
    Ok(())
}

fn cmd_capture(
    name: &str,
    wait_for: Option<&str>,
    timeout_secs: u64,
    format: CaptureFormat,
    scrollback: u32,
) -> Result<()> {
    let text = match wait_for {
        Some(needle) => Some(client::capture_wait(
            name,
            needle,
            std::time::Duration::from_secs(timeout_secs),
        )?),
        None => None,
    };
    match format {
        CaptureFormat::Text if scrollback == 0 => {
            let screen = match text {
                Some(t) => t,
                None => client::capture(name)?,
            };
            println!("{screen}");
        }
        CaptureFormat::Text => println!("{}", client::snapshot(name, scrollback)?.to_text()),
        CaptureFormat::Json => println!("{}", client::snapshot(name, scrollback)?.to_json()),
        CaptureFormat::Ansi => println!("{}", client::snapshot(name, scrollback)?.to_ansi()),
    }
    Ok(())
}

fn cmd_kill(name: &str) -> Result<()> {
    client::kill(name)?;
    println!("[killed {name}]");
    Ok(())
}

fn cmd_detach(name: Option<String>) -> Result<()> {
    let name = session::resolve_target(name, std::env::var(session::SESSION_ENV).ok())?;
    client::detach_clients(&name)?;
    println!("[detached {name}]");
    Ok(())
}

fn cmd_keys() -> Result<()> {
    use std::io::Write;

    let prefix = client::configured_prefix();
    println!("wmux keys — press keys to see the bytes wmux receives.");
    println!(
        "prefix is 0x{prefix:02x} (Ctrl-{}); press it then 'd' to detach when attached.",
        (prefix + b'@') as char
    );
    println!("Ctrl-Q quits.\n");

    let _raw = console::RawMode::enter()?;
    let mut stdout = std::io::stdout();
    let mut buf = [0u8; 256];

    loop {
        let n = console::read_console_input(&mut buf)?;
        if n == 0 {
            break;
        }
        let bytes = &buf[..n];
        let hex = bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        let printable: String = bytes
            .iter()
            .map(|b| {
                if (0x20..0x7f).contains(b) {
                    *b as char
                } else {
                    '.'
                }
            })
            .collect();
        let note = if bytes.contains(&prefix) {
            "  <- prefix seen"
        } else {
            ""
        };
        // \r\n because the console is in raw mode.
        write!(stdout, "  {hex:<40} {printable}{note}\r\n")?;
        stdout.flush()?;

        if bytes.contains(&0x11) {
            break;
        }
    }
    Ok(())
}

/// Launches `wmux server` as a console-less background process.
fn spawn_detached_server(
    name: &str,
    command: &[String],
    cols: u16,
    rows: u16,
    mode: server::Mode,
) -> Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().context("could not locate the wmux executable")?;

    let build = |flags: u32| {
        let mut c = Command::new(&exe);
        c.arg("server")
            .arg("--name")
            .arg(name)
            .arg("--cols")
            .arg(cols.to_string())
            .arg("--rows")
            .arg(rows.to_string());
        if let server::Mode::Legacy { history } = mode {
            c.arg("--legacy").arg("--history").arg(history.to_string());
        }
        if !command.is_empty() {
            // `--` keeps clap from treating the child's own flags as ours.
            c.arg("--");
            for part in command {
                c.arg(part);
            }
        }
        c.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(flags);
        c
    };

    let preferred = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB;
    match build(preferred).spawn() {
        Ok(_) => Ok(()),
        // A job object that forbids breakaway rejects the flag outright. Fall
        // back to a plain detached spawn: the session still survives its
        // window closing, but if the launching process really was in a
        // kill-on-close job the session dies with that job. Say so rather than
        // silently downgrading, because this is exactly the case that matters
        // when launching over SSH.
        Err(_) => {
            build(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
                .spawn()
                .context("failed to start the wmux session server")?;
            eprintln!(
                "wmux: warning: could not break away from the parent job object; \
                 this session may not survive its parent process exiting"
            );
            Ok(())
        }
    }
}

/// Polls until the server publishes its pipe, so `new` never races `attach`.
fn wait_for_session(name: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + SERVER_START_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if session::session_exists(name).unwrap_or(false) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    bail!(
        "the session server did not start within {} seconds \
         (set WMUX_LOG=1 and check %LOCALAPPDATA%\\wmux\\wmux.log)",
        SERVER_START_TIMEOUT.as_secs()
    )
}

/// Picks the shell to run when the user does not name one.
fn default_shell() -> String {
    if let Some(shell) = std::env::var_os("WMUX_SHELL") {
        if let Ok(s) = shell.into_string() {
            if !s.trim().is_empty() {
                return s;
            }
        }
    }
    // Prefer PowerShell 7 when it is installed, fall back to Windows PowerShell.
    if which("pwsh.exe") {
        "pwsh.exe".to_string()
    } else {
        "powershell.exe".to_string()
    }
}

/// Cheap PATH lookup; avoids pulling in a crate for one call.
fn which(exe: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(exe).is_file())
}
