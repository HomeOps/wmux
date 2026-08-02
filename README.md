# wmux

Terminal session persistence for Windows. Start a session, detach, close the
window, reattach later — the shell and everything running in it keep going.

```powershell
wmux new -s build          # create a session and attach
# ... start a long build ...
# press Ctrl-B then d       -> detached, build keeps running
wmux ls                    # see it running
wmux attach build          # pick up exactly where you left off
```

## Why this exists

Windows has never had a terminal multiplexer, for a reason that stopped being
true in 2018. The classic console was not a byte stream — programs poked
characters into a screen buffer through the Console API, so there was no stream
for a multiplexer to sit in the middle of. **ConPTY** (Windows 10 1809) changed
that, and everything wmux needs has been shipping since.

wmux uses it directly:

```
  wmux new ──spawns──> wmux server        (DETACHED_PROCESS: no console at all)
                            │
                            ├── ConPTY ──────> pwsh.exe
                            ├── vt100 model    (so a later client can be repainted)
                            └── named pipe ──< wmux attach   (any terminal)
```

The server holds no console, so when you close the window that started it, the
`CTRL_CLOSE_EVENT` conhost sends to that console never reaches the server. That
is the entire trick.

## What it deliberately does not do

No panes, no splits, no status bar, no copy mode, no config language.

Windows Terminal already does tabs, splits, search, and settings, and does them
well. On Unix tmux grew those features because terminals of the era did not
have them; that history is not a reason to reimplement them here. wmux does the
one thing Windows actually lacks — a session that outlives its window — and
leaves the rest to your terminal. Split a Windows Terminal pane and attach a
different wmux session in each.

## Install

```powershell
winget install HomeOps.wmux
```

Or build from source (requires Rust 1.77+):

```powershell
cargo build --release
```

## Usage

| Command | What it does |
|---|---|
| `wmux new` | Create a session (auto-named) and attach |
| `wmux new -s NAME` | Create a named session and attach |
| `wmux new -d -s NAME` | Create it but stay detached |
| `wmux new -s NAME -- pwsh -NoProfile` | Run a specific program |
| `wmux ls` | List running sessions |
| `wmux attach [NAME]` | Attach; NAME is optional if only one exists |
| `wmux kill NAME` | Terminate a session and its child |

### Keys

tmux bindings, so muscle memory carries over:

| Keys | Action |
|---|---|
| `Ctrl-B` `d` | Detach |
| `Ctrl-B` `Ctrl-B` | Send a literal Ctrl-B to the session |

PSReadLine binds `Ctrl-B` to backward-char, which is the same tradeoff tmux
users already live with. To rebind:

```powershell
$env:WMUX_PREFIX = '^A'    # or any ^X form, or a literal control byte
```

### Environment

| Variable | Effect |
|---|---|
| `WMUX_PREFIX` | Prefix key, e.g. `^A`. Defaults to `^B`. |
| `WMUX_SHELL` | Program `wmux new` runs. Defaults to `pwsh.exe`, then `powershell.exe`. |
| `WMUX_LOG` | Set to anything to log to `%LOCALAPPDATA%\wmux\wmux.log`. |
| `WMUX_SESSION` | Set *inside* a session to its name. |

## Over SSH

The case nothing else covers. `sshd` on Windows creates a ConPTY per session,
so a dropped connection kills whatever was running. With wmux:

```powershell
ssh you@windows-box
wmux new -s build
# start the build, press Ctrl-B d, close the laptop
# later, from anywhere:
ssh you@windows-box
wmux attach build
```

wmux spawns its server with `CREATE_BREAKAWAY_FROM_JOB` so it escapes any job
object the SSH session is holding. If breakaway is refused it says so on stderr
rather than silently leaving you with a session that will die.

## How reattach works

ConPTY only ever hands out a *stream*. Once bytes have gone past, there is no
API to ask Windows what the screen currently looks like — which is exactly what
a client attaching ten minutes later needs.

So the server replays everything the pty emits into a `vt100` terminal model and
keeps it current. On attach it serialises that model back into escape sequences
and sends it as a single repaint. This is the same reason tmux contains a
terminal emulator.

## Security

Each session is a named pipe at `\\.\pipe\wmux.<your-SID>.<name>`, created with
an explicit DACL granting access to SYSTEM, Administrators, and you — nobody
else. The first instance is created with `FILE_FLAG_FIRST_PIPE_INSTANCE` so a
squatter holding the name causes a hard failure instead of silently intercepting
keystrokes. Clients connect with `SECURITY_IDENTIFICATION` so a compromised
server cannot impersonate the attaching user.

## Development

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test                        # unit tests
cargo test --test e2e             # spawns real servers, ConPTYs, and shells
```

The end-to-end tests drive the wire protocol directly rather than going through
`wmux attach`, because attaching needs a real console and CI does not have one.
Everything below the console layer is exercised for real.

## License

MIT
