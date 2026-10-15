//! Legacy console backend: a real, hidden conhost instead of a ConPTY.
//!
//! Some programs cannot run under a pseudoconsole at all. ZTreeWin, for
//! example, calls `GetConsoleWindow()` at startup to size and position itself;
//! a ConPTY has no real window, so it reports "cannot get window handle" and
//! never draws. Programs like this were written against the classic console
//! API (`WriteConsoleOutput`, `ReadConsoleInput`, `SetConsoleWindowInfo`) and
//! only behave inside a classic console.
//!
//! So a `--legacy` session gives the program exactly that: a conhost window
//! created hidden (`SW_HIDE`), which is a real console with a real HWND. The
//! server observes and drives it through the same classic API rather than a
//! byte stream:
//!
//! * the screen is read with `ReadConsoleOutputW`, which returns characters
//!   *and* attributes, so there is no terminal model to keep in sync;
//! * input is injected with `WriteConsoleInputW` as key records, so function
//!   keys and Alt chords arrive exactly as a keyboard would deliver them.
//!
//! ```text
//!   wmux server ──CreateProcess(CREATE_NEW_CONSOLE, SW_HIDE)──> wmux console-host
//!        │                                                       ├─ sizes the console
//!        └── AttachConsole(host) ── ReadConsoleOutput /          ├─ joins a kill-on-close job
//!                                   WriteConsoleInput            └─ runs the program, waits
//! ```
//!
//! The trampoline exists because many legacy programs read the console size
//! once at startup. Sizing the console *before* the program starts is the only
//! race-free way to get a 160x50 session; resizing afterwards would depend on
//! the program noticing.

use crate::keys::{Key, KeyCode, ALT, CTRL, SHIFT};
use crate::snapshot::{Cell, Snapshot};
use anyhow::{bail, Context, Result};

use windows_sys::Win32::{
    Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
    System::{
        Console::{
            AttachConsole, FreeConsole, GetConsoleScreenBufferInfo, GetLargestConsoleWindowSize,
            GetNumberOfConsoleInputEvents, ReadConsoleOutputW, SetConsoleCtrlHandler,
            SetConsoleScreenBufferSize, SetConsoleWindowInfo, WriteConsoleInputW, CHAR_INFO,
            CONSOLE_SCREEN_BUFFER_INFO, COORD, INPUT_RECORD, SMALL_RECT,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Threading::{
            CreateProcessW, GetCurrentProcess, GetExitCodeProcess, TerminateProcess,
            WaitForSingleObject, CREATE_NEW_CONSOLE, INFINITE, PROCESS_INFORMATION,
            STARTF_USESHOWWINDOW, STARTUPINFOW,
        },
    },
    UI::Input::KeyboardAndMouse::{MapVirtualKeyW, VkKeyScanW},
};

/// `SW_HIDE`, without pulling in the whole windowing feature for one constant.
const SW_HIDE: u16 = 0;
const KEY_EVENT: u16 = 1;
// dwControlKeyState bits.
const RIGHT_ALT_PRESSED: u32 = 0x01;
const LEFT_ALT_PRESSED: u32 = 0x02;
const LEFT_CTRL_PRESSED: u32 = 0x08;
const SHIFT_PRESSED: u32 = 0x10;
const ENHANCED_KEY: u32 = 0x100;
/// The second cell of a double-width character.
const COMMON_LVB_TRAILING_BYTE: u16 = 0x200;

/// How long the server waits for the host's console to exist.
const ATTACH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// How long to wait for the program to consume one injected key before
/// sending the next anyway. A program that is busy, or not reading input at
/// all, must not hang `wmux key`.
const KEY_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1000);
/// `ReadConsoleOutputW` historically failed above 64 KB per call; staying
/// well under it costs nothing.
const READ_CHUNK_CELLS: usize = 8000;

/// A running legacy session, seen from the server.
pub struct Legacy {
    host: HANDLE,
    host_pid: u32,
    /// Console calls from several client threads are serialised so that a
    /// key sequence is never interleaved with another client's.
    lock: std::sync::Mutex<()>,
}

// The process handle is only used with thread-safe kernel calls.
unsafe impl Send for Legacy {}
unsafe impl Sync for Legacy {}

impl Drop for Legacy {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.host) };
    }
}

impl Legacy {
    /// Starts `wmux console-host` in a new hidden console and attaches this
    /// process to it. The caller must not already own a console; the session
    /// server is spawned `DETACHED_PROCESS`, so it does not.
    pub fn spawn(command: &[String], cols: u16, rows: u16, history: u16) -> Result<Legacy> {
        let exe = std::env::current_exe().context("could not locate the wmux executable")?;
        let mut args = vec![
            exe.to_string_lossy().into_owned(),
            "console-host".into(),
            "--cols".into(),
            cols.to_string(),
            "--rows".into(),
            rows.to_string(),
            "--history".into(),
            history.to_string(),
            "--".into(),
        ];
        args.extend(command.iter().cloned());
        let mut cmdline: Vec<u16> = command_line(&args).encode_utf16().chain([0]).collect();

        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.dwFlags = STARTF_USESHOWWINDOW;
        si.wShowWindow = SW_HIDE;
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

        let ok = unsafe {
            CreateProcessW(
                std::ptr::null(),
                cmdline.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                CREATE_NEW_CONSOLE,
                std::ptr::null(),
                std::ptr::null(),
                &si,
                &mut pi,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to start the legacy console host");
        }
        unsafe { CloseHandle(pi.hThread) };

        let legacy = Legacy {
            host: pi.hProcess,
            host_pid: pi.dwProcessId,
            lock: std::sync::Mutex::new(()),
        };
        legacy.attach()?;
        Ok(legacy)
    }

    /// Attaches to the host's console, retrying until it exists: the console
    /// is allocated during the host's own startup, not by `CreateProcess`.
    fn attach(&self) -> Result<()> {
        let deadline = std::time::Instant::now() + ATTACH_TIMEOUT;
        loop {
            unsafe { FreeConsole() };
            if unsafe { AttachConsole(self.host_pid) } != 0 {
                break;
            }
            let err = std::io::Error::last_os_error();
            if std::time::Instant::now() > deadline || self.exited() {
                return Err(err).context("could not attach to the legacy console");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // Being attached means Ctrl-C injected for the program would also be
        // delivered to the server. A NULL handler with TRUE ignores it here
        // without affecting the program, which has its own console state.
        unsafe { SetConsoleCtrlHandler(None, 1) };
        Ok(())
    }

    fn exited(&self) -> bool {
        unsafe { WaitForSingleObject(self.host, 0) == 0 }
    }

    /// Blocks until the program exits and returns its exit code. The host
    /// exits with the program's code.
    pub fn wait(&self) -> i32 {
        unsafe {
            WaitForSingleObject(self.host, INFINITE);
            let mut code = 0u32;
            GetExitCodeProcess(self.host, &mut code);
            code as i32
        }
    }

    /// Terminates the host; its kill-on-close job takes the program with it.
    pub fn kill(&self) {
        unsafe { TerminateProcess(self.host, 1) };
    }

    /// The console window size.
    pub fn size(&self) -> Result<(u16, u16)> {
        let _g = self.lock.lock().unwrap();
        let out = ConHandle::output()?;
        let info = out.info()?;
        let w = info.srWindow;
        Ok(((w.Right - w.Left + 1) as u16, (w.Bottom - w.Top + 1) as u16))
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let _g = self.lock.lock().unwrap();
        let out = ConHandle::output()?;
        // Keep whatever history the session was created with.
        let info = out.info()?;
        let window_rows = (info.srWindow.Bottom - info.srWindow.Top + 1) as u16;
        let history = (info.dwSize.Y as u16).saturating_sub(window_rows);
        resize_console(&out, cols, rows, history)
    }

    /// Reads the visible window plus up to `scrollback` rows above it.
    pub fn snapshot(&self, scrollback: usize) -> Result<Snapshot> {
        let _g = self.lock.lock().unwrap();
        // Opened per call: a program may switch to a new screen buffer with
        // SetConsoleActiveScreenBuffer at any time, and CONOUT$ always names
        // whichever buffer is active right now.
        let out = ConHandle::output()?;
        let info = out.info()?;
        let w = info.srWindow;
        let cols = (w.Right - w.Left + 1) as u16;
        let visible = (w.Bottom - w.Top + 1) as u16;
        let history = (w.Top as usize).min(scrollback) as u16;
        let top = w.Top - history as i16;
        let rows = visible + history;

        let mut cells = Vec::with_capacity(cols as usize * rows as usize);
        let chunk_rows = (READ_CHUNK_CELLS / cols.max(1) as usize).max(1) as i16;
        let mut y = top;
        while y <= w.Bottom {
            let n = chunk_rows.min(w.Bottom - y + 1);
            let mut buf: Vec<CHAR_INFO> =
                vec![unsafe { std::mem::zeroed() }; cols as usize * n as usize];
            let mut region = SMALL_RECT {
                Left: w.Left,
                Top: y,
                Right: w.Right,
                Bottom: y + n - 1,
            };
            let ok = unsafe {
                ReadConsoleOutputW(
                    out.0,
                    buf.as_mut_ptr(),
                    COORD {
                        X: cols as i16,
                        Y: n,
                    },
                    COORD { X: 0, Y: 0 },
                    &mut region,
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error()).context("ReadConsoleOutputW failed");
            }
            cells.extend(buf.iter().map(|ci| {
                let unit = unsafe { ci.Char.UnicodeChar };
                let ch = if ci.Attributes & COMMON_LVB_TRAILING_BYTE != 0 {
                    // The right half of a wide character; the left half
                    // already carries it. A space keeps the columns aligned.
                    ' '
                } else {
                    char::from_u32(unit as u32).unwrap_or('?')
                };
                Cell {
                    ch: if ch == '\0' { ' ' } else { ch },
                    attr: (ci.Attributes & 0xFF) as u8,
                }
            }));
            y += n;
        }

        let cur = info.dwCursorPosition;
        Ok(Snapshot {
            cols,
            rows,
            history,
            cursor_x: (cur.X - w.Left).max(0) as u16,
            cursor_y: (cur.Y - w.Top).max(0) as u16,
            cells,
        })
    }

    /// Injects keypresses, one at a time, waiting for the program to consume
    /// each before sending the next.
    ///
    /// Pacing matters: full-screen programs commonly flush the input buffer
    /// when they open a dialog, so a burst of keys written at once loses
    /// everything after the key that opened it.
    pub fn send_keys(&self, keys: &[Key]) -> Result<()> {
        let _g = self.lock.lock().unwrap();
        let input = ConHandle::input()?;
        for key in keys {
            let records = key_records(key);
            let mut written = 0u32;
            let ok = unsafe {
                WriteConsoleInputW(
                    input.0,
                    records.as_ptr(),
                    records.len() as u32,
                    &mut written,
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("WriteConsoleInputW failed for {key}"));
            }
            input.wait_drained(KEY_DRAIN_TIMEOUT);
        }
        Ok(())
    }
}

/// A console handle opened by name, closed on drop.
struct ConHandle(HANDLE);

impl ConHandle {
    fn open(name: &str) -> Result<ConHandle> {
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("could not open {name}"));
        }
        Ok(ConHandle(h))
    }

    fn output() -> Result<ConHandle> {
        Self::open("CONOUT$")
    }

    fn input() -> Result<ConHandle> {
        Self::open("CONIN$")
    }

    fn info(&self) -> Result<CONSOLE_SCREEN_BUFFER_INFO> {
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
        if unsafe { GetConsoleScreenBufferInfo(self.0, &mut info) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("GetConsoleScreenBufferInfo failed");
        }
        Ok(info)
    }

    fn wait_drained(&self, timeout: std::time::Duration) {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let mut pending = 0u32;
            if unsafe { GetNumberOfConsoleInputEvents(self.0, &mut pending) } == 0 || pending == 0 {
                return;
            }
            if std::time::Instant::now() > deadline {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

impl Drop for ConHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Sets the window to `cols` x `rows`, with `history` extra buffer rows of
/// scrollback above it.
///
/// The window can never be larger than the buffer, so the order matters:
/// shrink the window to nothing, size the buffer, then grow the window.
fn resize_console(out: &ConHandle, cols: u16, rows: u16, history: u16) -> Result<()> {
    let largest = unsafe { GetLargestConsoleWindowSize(out.0) };
    if largest.X > 0 && largest.Y > 0 && (cols as i16 > largest.X || rows as i16 > largest.Y) {
        bail!(
            "{cols}x{rows} is larger than this console allows ({}x{}); \
             the limit comes from the screen size and console font",
            largest.X,
            largest.Y
        );
    }
    let buffer_rows = rows.saturating_add(history).min(i16::MAX as u16);
    unsafe {
        let tiny = SMALL_RECT {
            Left: 0,
            Top: 0,
            Right: 0,
            Bottom: 0,
        };
        SetConsoleWindowInfo(out.0, 1, &tiny);
        if SetConsoleScreenBufferSize(
            out.0,
            COORD {
                X: cols as i16,
                Y: buffer_rows as i16,
            },
        ) == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("SetConsoleScreenBufferSize failed");
        }
        // Show the bottom of the buffer: that is where new output lands, and
        // with no history it is simply the whole buffer.
        let top = (buffer_rows - rows) as i16;
        let window = SMALL_RECT {
            Left: 0,
            Top: top,
            Right: cols as i16 - 1,
            Bottom: top + rows as i16 - 1,
        };
        if SetConsoleWindowInfo(out.0, 1, &window) == 0 {
            return Err(std::io::Error::last_os_error()).context("SetConsoleWindowInfo failed");
        }
    }
    Ok(())
}

/// The down and up records for one keypress.
fn key_records(key: &Key) -> [INPUT_RECORD; 2] {
    let mut state = 0u32;
    let vk = match key.vk() {
        Some(vk) => vk,
        None => {
            let KeyCode::Char(c) = key.code else {
                unreachable!("only characters lack a fixed virtual-key code")
            };
            let mut unit = [0u16; 2];
            let scan = unsafe { VkKeyScanW(c.encode_utf16(&mut unit)[0]) };
            if scan == -1 {
                // Not on this keyboard layout. VK 0 with a character is how
                // conhost itself delivers pasted text.
                0
            } else {
                let shifts = (scan as u16) >> 8;
                if shifts & 1 != 0 {
                    state |= SHIFT_PRESSED;
                }
                // Characters typed with AltGr report Ctrl+Alt on the record.
                if shifts & 6 == 6 {
                    state |= LEFT_CTRL_PRESSED | RIGHT_ALT_PRESSED;
                }
                (scan as u16) & 0xFF
            }
        }
    };
    if key.mods & SHIFT != 0 {
        state |= SHIFT_PRESSED;
    }
    if key.mods & CTRL != 0 {
        state |= LEFT_CTRL_PRESSED;
    }
    if key.mods & ALT != 0 {
        state |= LEFT_ALT_PRESSED;
    }
    if key.is_enhanced() {
        state |= ENHANCED_KEY;
    }
    let scan = unsafe { MapVirtualKeyW(vk as u32, 0) } as u16;
    let ch = key.console_char();

    let make = |down: bool| -> INPUT_RECORD {
        let mut r: INPUT_RECORD = unsafe { std::mem::zeroed() };
        r.EventType = KEY_EVENT;
        unsafe {
            let k = &mut r.Event.KeyEvent;
            k.bKeyDown = down as i32;
            k.wRepeatCount = 1;
            k.wVirtualKeyCode = vk;
            k.wVirtualScanCode = scan;
            k.uChar.UnicodeChar = ch;
            k.dwControlKeyState = state;
        }
        r
    };
    [make(true), make(false)]
}

unsafe extern "system" fn ignore_ctrl(_ctrl_type: u32) -> i32 {
    1
}

/// Entry point for the hidden `wmux console-host` trampoline.
///
/// Runs inside the new console: sizes it, makes sure the program cannot
/// outlive this process, runs the program, and exits with its exit code.
pub fn host_main(cols: u16, rows: u16, history: u16, command: &[String]) -> Result<i32> {
    if command.is_empty() {
        bail!("console-host needs a command to run");
    }
    // A handler routine, unlike SetConsoleCtrlHandler(NULL, TRUE), is not
    // inherited: Ctrl-C still reaches the program, the host just survives it.
    unsafe { SetConsoleCtrlHandler(Some(ignore_ctrl), 1) };

    // Clamp rather than fail: `wmux new -d` asks for a large default screen,
    // and the largest console window depends on the monitor and the font.
    let out = ConHandle::output()?;
    let largest = unsafe { GetLargestConsoleWindowSize(out.0) };
    let (cols, rows) = if largest.X > 0 && largest.Y > 0 {
        (cols.min(largest.X as u16), rows.min(largest.Y as u16))
    } else {
        (cols, rows)
    };
    if let Err(e) = resize_console(&out, cols, rows, history) {
        crate::server::log(&format!(
            "console-host: resize to {cols}x{rows} failed: {e:#}"
        ));
    }
    drop(out);

    // Put ourselves in a kill-on-close job. The program inherits it, and when
    // this process dies, however it dies, the last job handle closes and
    // Windows terminates the whole tree. That is what makes `wmux kill` work.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if !job.is_null() {
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
                crate::server::log(&format!(
                    "console-host: could not join kill-on-close job: {}",
                    std::io::Error::last_os_error()
                ));
            }
            // The handle is deliberately leaked: it must live exactly as long
            // as this process.
        }
    }

    let status = std::process::Command::new(&command[0])
        .args(&command[1..])
        .status()
        .with_context(|| format!("failed to start {:?}", command[0]))?;
    Ok(status.code().unwrap_or(-1))
}

/// Joins arguments into a Windows command line that `CommandLineToArgvW` and
/// the MSVC runtime split back into the same arguments.
pub fn command_line(args: &[String]) -> String {
    let mut out = String::new();
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        quote_arg(arg, &mut out);
    }
    out
}

fn quote_arg(arg: &str, out: &mut String) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // Backslashes before a quote are escapes: double them, then
                // escape the quote itself.
                out.extend(std::iter::repeat('\\').take(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.extend(std::iter::repeat('\\').take(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    // Trailing backslashes precede the closing quote, so they double too.
    out.extend(std::iter::repeat('\\').take(backslashes * 2));
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(args: &[&str]) -> String {
        command_line(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn plain_arguments_are_not_quoted() {
        assert_eq!(
            line(&["ztw.exe", "/AUTO", "X:\\src"]),
            r"ztw.exe /AUTO X:\src"
        );
    }

    #[test]
    fn spaces_and_empty_arguments_are_quoted() {
        assert_eq!(
            line(&[r"C:\Program Files\x.exe", ""]),
            r#""C:\Program Files\x.exe" """#
        );
    }

    #[test]
    fn quotes_and_backslashes_follow_msvc_rules() {
        assert_eq!(line(&[r#"a"b"#]), r#""a\"b""#);
        assert_eq!(line(&[r#"a\"b"#]), r#""a\\\"b""#);
        assert_eq!(line(&[r"dir with space\"]), r#""dir with space\\""#);
        // Backslashes not before a quote are literal.
        assert_eq!(line(&[r"a b\c"]), r#""a b\c""#);
    }

    #[test]
    fn named_keys_become_matching_down_and_up_records() {
        let [down, up] = key_records(&Key::parse("Alt-F10").unwrap());
        unsafe {
            assert_eq!(down.EventType, KEY_EVENT);
            assert_eq!(down.Event.KeyEvent.bKeyDown, 1);
            assert_eq!(up.Event.KeyEvent.bKeyDown, 0);
            assert_eq!(down.Event.KeyEvent.wVirtualKeyCode, 0x79);
            assert_eq!(down.Event.KeyEvent.dwControlKeyState, LEFT_ALT_PRESSED);
            assert_eq!(down.Event.KeyEvent.uChar.UnicodeChar, 0);
        }
        let [down, _] = key_records(&Key::parse("Down").unwrap());
        unsafe {
            assert_eq!(down.Event.KeyEvent.dwControlKeyState, ENHANCED_KEY);
        }
    }

    #[test]
    fn characters_carry_their_text_and_ctrl_carries_the_control_code() {
        let [down, _] = key_records(&Key::char('q'));
        unsafe {
            assert_eq!(down.Event.KeyEvent.uChar.UnicodeChar, u16::from(b'q'));
            assert_eq!(down.Event.KeyEvent.wVirtualKeyCode, u16::from(b'Q'));
        }
        let [down, _] = key_records(&Key::parse("C-c").unwrap());
        unsafe {
            assert_eq!(down.Event.KeyEvent.uChar.UnicodeChar, 3);
            assert_eq!(
                down.Event.KeyEvent.dwControlKeyState & LEFT_CTRL_PRESSED,
                LEFT_CTRL_PRESSED
            );
        }
    }
}
