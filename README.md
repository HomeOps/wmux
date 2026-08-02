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
| `wmux run -t NAME <cmd>` | Run a PowerShell command, get its result as CLIXML/JSON/text |
| `wmux send -t NAME <text>` | Type raw keystrokes into a session without attaching |
| `wmux capture -t NAME` | Print the session's visible screen as plain text |
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

## Scripting: a shell that remembers

None of `run`, `send`, or `capture` needs a console, so a script — or an AI
agent, or anything else without a terminal — can drive a session and keep shell
state between invocations that would otherwise each start cold:

```powershell
wmux new -d -s work pwsh.exe -NoLogo -NoProfile

# expensive work happens once, and the result stays in the session
wmux run -t work -- '$inv = Get-ChildItem C:\big -Recurse -File | Select FullName, Length'

# ...later, from a completely separate process
wmux run -t work --format json -- '$inv | Measure-Object Length -Sum'
```

The payoff is that the dataset never crosses the process boundary. Only the
answer you asked for does.

None of these register as an attached client, so `wmux ls` still reports the
session as `detached` while a script is driving it, and several callers can use
one session concurrently.

### Why `run` does not use the screen

`capture` scrapes the rendered terminal, which is the wrong tool for getting a
*value* out of PowerShell. The screen is wrapped to the terminal width, drops
anything that scrolled away, flattens the object pipeline to whatever
`Out-String` produced, and interleaves your echoed command with its own output.

So `run` routes the payload around the terminal entirely. It types a wrapper
that serialises the pipeline to a temp file and then touches a completion
marker; wmux polls for the marker and reads the payload off disk. The terminal
only ever carries the *command*.

That means no wrapping, no size limit, and real types:

```powershell
wmux run -t work --format clixml -- 'Get-Process pwsh' > procs.xml
$procs = Import-Clixml procs.xml     # real objects, not text
```

Every result is wrapped in an envelope so failures are data rather than a
parsing problem:

```json
{"Success":true,"Error":null,"ExitCode":null,"Output":[{"Name":"main.rs","Length":12795}]}
```

A command that throws comes back with `"Success":false` and the message in
`Error`, and the session stays usable.

`run` is the one PowerShell-specific part of wmux. Sessions running anything
else still work with `send` and `capture`.

### `capture --wait-for` matches the echo too

Only relevant if you use `capture` directly. A terminal echoes what you type,
so `--wait-for 'count='` matches the moment the command is echoed, before it
has produced anything. Wait on a sentinel the shell builds at runtime:

```powershell
wmux send -t work -- 'Write-Output ("DONE"+"MARK"+" rows=$($data.Count)")'
wmux capture -t work --wait-for 'DONEMARK '
```

The echo shows `("DONE"+"MARK"+...)`; only the output contains `DONEMARK `.
`run` does not have this problem, which is the main reason to prefer it.

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
