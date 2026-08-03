# wmux — Claude Code Directives

Terminal session persistence for Windows, written in Rust. A detached server
process owns a ConPTY; clients attach and detach over a named pipe.

## Scope discipline

**wmux is `dtach`, not `tmux`.** It does session persistence and nothing else.

Do **not** add panes, splits, status bars, copy mode, layouts, or a config
language. Windows Terminal already provides tabs, splits, and search, and the
whole design bet is that duplicating them is wasted work that turns a small
maintainable tool into a large one. If a feature request implies a second
terminal UI inside wmux, the answer is "use a Windows Terminal pane per
session".

## Commands

```powershell
cargo fmt --all -- --check              # CI enforces this
cargo clippy --all-targets -- -D warnings
cargo test --lib --bins                 # fast, no processes spawned
cargo test --test e2e -- --test-threads=2   # spawns real servers + pwsh
cargo build --release
```

Rust is installed per-user and may not be on `PATH` in a fresh shell. Use
`Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'` if `cargo` is not found.

**Kill leftover servers before rebuilding.** A running session server holds
`target\debug\wmux.exe` open and the build fails with `Access is denied`:

```powershell
Get-Process wmux -ErrorAction SilentlyContinue | Stop-Process -Force
```

## Architecture

| File | Responsibility |
|---|---|
| `src/protocol.rs` | Length-prefixed frame codec. Hand-rolled; no serde. |
| `src/pipe.rs` | Named pipe listener and connection, overlapped I/O. |
| `src/session.rs` | Name validation, pipe paths, discovery, user SID. |
| `src/server.rs` | ConPTY host, vt100 screen model, client fanout. |
| `src/client.rs` | Attach loop, detach key state machine, resize polling. |
| `src/run.rs` | PowerShell command execution with structured results. |
| `src/console.rs` | Raw-mode guard and console size. |
| `src/main.rs` | CLI only. Keep logic in the library. |

The crate is deliberately split into a lib and a thin bin so integration tests
can drive the protocol without a console.

## Win32 gotchas that cost real debugging time

These are load-bearing. Do not "simplify" them away.

**Never probe a pipe path with filesystem APIs.** `Path::exists()`,
`Test-Path`, `GetFileAttributes` — all of these open a *client connection* to
the named pipe, which consumes the instance the server is waiting on and leaves
`ConnectNamedPipe` returning `ERROR_NO_DATA` forever. `session_exists` uses a
`read_dir` over `\\.\pipe\` instead, which has no side effect. `PipeListener::accept`
also replaces a poisoned instance on failure, because antivirus and directory
tools do this to us uninvited.

**Never call `DisconnectNamedPipe` on close.** It discards data the peer has
not read yet, which silently loses the final `Exited` frame every time a
session ends. Just `CloseHandle`; buffered bytes stay readable and the peer
gets a clean `ERROR_BROKEN_PIPE` after draining them.

**Pipes must be opened with `FILE_FLAG_OVERLAPPED`.** A synchronous handle
serialises all I/O through the file object lock, so the reader thread's
blocking `ReadFile` would stall the writer thread and deadlock the session.
Each `PipeConn` owns a separate event for the read and write directions.

**The server must be spawned `DETACHED_PROCESS`.** That is what makes it
survive its launching window: with no console, the `CTRL_CLOSE_EVENT` conhost
sends when the window closes never reaches it. `CREATE_BREAKAWAY_FROM_JOB` is
also attempted so it escapes an SSH session's job object; when breakaway is
refused we warn on stderr rather than silently downgrading.

**vt100 takes `(rows, cols)`, ConPTY's `PtySize` is a struct.** Easy to
transpose. The wire protocol carries `(cols, rows)`.

**`capture --wait-for` matches the echoed command, not just its output.** The
screen holds both. Waiting on a literal that appears in the text you sent
returns immediately, before the command has run. Wait on a sentinel the shell
constructs at runtime — `("DONE"+"MARK"+...)` — so the needle cannot appear in
the echo. Prefer `run`, which sidesteps the problem entirely.

**Never route a result over the terminal.** The screen is wrapped to the
terminal width, loses whatever scrolled away, flattens the object pipeline to
text, and interleaves the echo with the output. `run` writes the payload to a
temp file and touches a separate completion marker; wmux polls for the marker
so it can never read a half-flushed payload. Keep that ordering — the marker is
written *after* the payload, and there is a test asserting it.

**Console input is not a byte stream — decode it before matching keys.**
Windows Terminal negotiates **win32-input-mode**, so a keystroke arrives as
`ESC [ Vk ; Sc ; Uc ; Kd ; Cs ; Rc _` rather than as a plain byte. Ctrl-B is
`ESC[66;48;2;1;40;1_`, never a bare `0x02`. Anything scanning the raw stream
for a control byte finds nothing and forwards the whole sequence to the
session, where the shell acts on it. Always go through `input::InputParser`,
which yields `Key { ch, raw }`; match on `ch`, forward `raw` untouched so the
negotiated encoding still reaches the session.

**Key *releases* arrive between the prefix and the key it modifies.** Ctrl-B
produces a key-down and a key-up, and releasing Ctrl produces another event,
all before `d` is pressed. A detector that resolves its binding on the next
event of any kind gets cancelled by the release and can never detach. Hold
characterless events while armed and replay them only if the sequence turns
out not to be a binding.

**A ConPTY does not negotiate win32-input-mode, so nested-session tests cannot
catch this class of bug.** They still cover byte plumbing, but key encoding
must be tested against captured real-terminal sequences — see the constants in
`client::tests`.

**Never read console input with `std::io::stdin()`.** Rust's Windows
implementation goes through `ReadConsoleW`, the legacy console path, which does
not deliver `ENABLE_VIRTUAL_TERMINAL_INPUT` bytes. The Ctrl-B prefix fell
straight through to the session, PSReadLine treated it as backward-char, and
detaching was impossible. Use `console::read_console_input`, which calls
`ReadFile` on the console handle — the documented path for VT input.
`ctrl_b_then_d_is_intercepted_by_the_client_not_the_session` guards this.

**A session's ConPTY is a real console, so wmux can test its own client.**
Attach to session B from inside session A and drive it with `send`. ConPTY
converts the injected bytes into key records and the client reads them back
through the full translation path, so this exercises key handling rather than
just the byte plumbing. This is the only way to test `attach` without a TTY.

**When testing nested sessions, never synchronise on `"PS "`.** The outer
session's own prompt matches it, so the test races ahead of the attach. Mark
the inner session with a runtime-built string and wait for *that*.

**Broadcast must never write to a pipe from the pty pump thread.** A client
that stops reading fills the 64 KB pipe buffer and the blocking write freezes
output for every client. Each client owns a bounded queue drained by its own
writer thread, and one that fills its queue is dropped.

**`wmux new` looks like it hangs from a test harness.** The detached server
outlives the command, so a harness that waits on the whole process tree blocks
until timeout. Not a bug: invoke via `Start-Process -PassThru
-RedirectStandardOutput` and it returns in milliseconds.

**`run` delivers one line of keystrokes.** A newline would submit the command
early, so the wrapper must stay single-line and `run` rejects a multi-line
command. Paths interpolated into it go through `ps_single_quote`, which doubles
`'` — PowerShell single-quoted literals have no other escape, and backslashes
are *not* escapes.

## Testing

Unit tests live beside the code. The end-to-end tests in `tests/e2e.rs` spawn
real server processes, real ConPTYs, and real `pwsh` instances, then assert on
the *rendered* screen rather than the raw byte stream — ConPTY interleaves
cursor movement with echoed characters, so substring-matching raw output is
unreliable.

Any change to attach, detach, or the repaint path needs an e2e test. That is
the behaviour the tool exists for.

## Branch and PR policy (release-please)

**Never commit directly to `main`.** This repo uses release-please to bump
`Cargo.toml`'s version and generate `CHANGELOG.md` from merged PR titles.

1. `git checkout -b <type>/<slug>`
2. Commit, then `gh pr create --base main --title "<type>: <description>"`
3. Squash-merge; GitHub uses the PR title as the commit subject.
4. Release-please opens a release PR; merging it tags and publishes.

PR titles must follow [Conventional Commits](https://www.conventionalcommits.org/):

| Type | Bump |
|---|---|
| `feat` | minor (pre-1.0: minor, via `bump-minor-pre-major`) |
| `fix`, `perf`, `refactor` | patch |
| `feat!` / `BREAKING CHANGE:` footer | major |
| `docs`, `chore`, `ci`, `test`, `build` | none |

Imperative mood, under 72 characters. A title that does not match the grammar
is silently ignored by release-please.

## Release and winget

`.github/workflows/release.yml` runs release-please, builds an x64 zip, attaches
it to the GitHub release, and submits a winget manifest update.

Publishing uses [WinGet Releaser](https://github.com/vedantmgoyal9/winget-releaser),
the community-standard action that generates most winget-pkgs PRs. It handles
forking, branching, the manifest edit, and the upstream PR.

**It can only *update* an existing package.** At least one version must already
be in `microsoft/winget-pkgs`, so the first submission is manual:

```powershell
winget install Microsoft.WingetCreate
wingetcreate new https://github.com/HomeOps/wmux/releases/download/v0.3.1/wmux-0.3.1-x64.zip
```

Identifier is `HomeOps.wmux`. wmux ships a **portable zip**, so the manifest is
`InstallerType: zip` with `NestedInstallerType: portable` and a
`PortableCommandAlias` of `wmux`.

Two settings that are easy to get wrong:

- **`installers-regex` must be widened.** The action defaults to
  `.(exe|msi|msix|appx)(bundle){0,1}$`, which never matches a zip, so the job
  silently finds no installers. wmux sets `'\.zip$'`.
- **`token` must be a *classic* PAT with `public_repo`.** Fine-grained tokens
  do not work — see winget-releaser issue #172. Set it as the `WINGET_TOKEN`
  secret; the job skips with a warning when it is absent so a release never
  fails just because publishing is not configured.

Set the `WINGET_FORK_USER` repository *variable* if the `winget-pkgs` fork
lives under an account other than the token owner's.

**Signing is not required by winget**, and unsigned portable packages are
accepted. It does matter for the install experience: unsigned binaries trigger
SmartScreen, and machines with Smart App Control enforced will refuse to run
them outright. Treat signing as a quality improvement, not a prerequisite.

ARM64 is not built yet; the release workflow ships x64 only. Adding it means
cross-compiling `aarch64-pc-windows-msvc`, which needs the ARM64 MSVC tools on
the runner.

---
*Last garbage-collected: 2026-08-02*
