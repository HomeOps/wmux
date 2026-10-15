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

Native builds exist for x64 and ARM64 Windows; winget picks the right one.
They are also attached to each [GitHub release](https://github.com/HomeOps/wmux/releases)
as `wmux-<version>-x64.zip` and `wmux-<version>-arm64.zip`.

Or build from source (requires Rust 1.77+):

```powershell
cargo build --release
# ARM64 from an x64 machine (needs the VS "MSVC ARM64 build tools"):
rustup target add aarch64-pc-windows-msvc
cargo build --release --target aarch64-pc-windows-msvc
```

## Usage

| Command | What it does |
|---|---|
| `wmux new` | Create a session (auto-named) and attach |
| `wmux new -s NAME` | Create a named session and attach |
| `wmux new -d -s NAME` | Create it but stay detached (a 200x60 screen unless `-x`/`-y` say otherwise) |
| `wmux new -s NAME -x 132 -y 43` | Create it at an explicit size |
| `wmux new -s NAME -- pwsh -NoProfile` | Run a specific program |
| `wmux new -d -s NAME --legacy -- prog.exe` | Host a classic console program in a real, hidden console |
| `wmux ls` | List running sessions |
| `wmux attach [NAME]` | Attach; NAME is optional if only one exists |
| `wmux run -t NAME <cmd>` | Run a PowerShell command, get its result as CLIXML/JSON/text |
| `wmux send -t NAME <text>` | Type raw keystrokes into a session without attaching |
| `wmux key -t NAME F1 Alt-F10 Down` | Press named keys without attaching |
| `wmux capture -t NAME` | Print the session's visible screen as plain text |
| `wmux capture -t NAME --format json -S 500` | The screen with per-cell colours, plus scrollback |
| `wmux resize -t NAME -x 160 -y 50` | Resize a session without attaching |
| `wmux kill NAME` | Terminate a session and its child |

### Keys

tmux bindings, so muscle memory carries over:

| Keys | Action |
|---|---|
| `Ctrl-B` `d` | Detach |
| `Ctrl-B` `Ctrl-B` | Send a literal Ctrl-B to the session |

Or, without touching the keyboard at all:

```powershell
wmux detach          # from inside the session
wmux detach build    # from any other terminal
```

PSReadLine binds `Ctrl-B` to backward-char, which is the same tradeoff tmux
users already live with. To rebind:

```powershell
$env:WMUX_PREFIX = '^A'    # or any ^X form, or a literal control byte
```

### Named keys

`wmux send` types text; `wmux key` presses keys that text cannot express:

```powershell
wmux key -t ztw F1            # function keys F1-F24
wmux key -t ztw Alt-F10 C-x   # modifiers: Ctrl-/C-, Alt-/M-, Shift-/S-
wmux key -t ztw Down Down PgDn Home Enter Esc Tab BSpace Ins Del
wmux key -t ztw q             # a single character is pressed literally
```

A ConPTY session receives the xterm escape sequence for each key; a
`--legacy` session receives a real key record (see below). Either way the
command returns once the session has consumed the keys.

### Legacy console sessions

Some programs cannot run under a pseudoconsole. They were written against the
classic console API, and typically ask for their console window with
`GetConsoleWindow()`; a ConPTY has no real window, so they fail or draw nothing.
ZTreeWin is one, reporting "cannot get window handle".

`--legacy` gives such a program a real conhost, created with its window
hidden. wmux reads the screen straight from the console buffer and injects
input as key records, so everything the program does with the classic API
works exactly as it would on a desktop:

```powershell
wmux new -d -s ztw --legacy -- ZTW64.EXE /AUTO X:\src
wmux capture -t ztw --wait-for COMMANDS
wmux key -t ztw Down Down F1
wmux capture -t ztw --format json      # characters and colours
```

The console is sized *before* the program starts, since many legacy programs
read the size once at startup. A console window cannot be larger than the
monitor and font allow, so a requested size is clamped to that limit. Killing
the session kills the program's whole process tree. `wmux attach` is not
supported for legacy sessions yet.

### Colours and scrollback

`capture --format json` returns each row twice: as text, and as one console
attribute per cell, two hex digits each (background then foreground, in the
classic console palette). `1E` is bright yellow on blue; `70` is black on grey.

```json
{"cols":200,"rows":49,"history":0,"cursor":{"x":0,"y":48},
 "lines":[" X:\\src  ...", ...], "attrs":["1E1E1E...", ...]}
```

To *look* at a session instead, `--format ansi` redraws it in colour. It
starts with `CSI 8;rows;cols t` to ask the terminal for the session's size
(conhost honours it; Windows Terminal ignores it, so widen the window first):

```powershell
wmux capture -t ztw --format ansi | Write-Host
wmux capture -t ztw --format ansi > ztw.ans    # Get-Content -Raw ztw.ans | Write-Host
```

ConPTY sessions are mapped to the same 16 colours from the terminal model,
with 256-colour and RGB values rounded to the nearest. `-S N` prepends up to
`N` rows of scrollback; `history` says how many of the rows are scrollback.

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
