//! Windows console setup for the attaching client.
//!
//! A wmux client is a plain console application: it reads keystrokes from
//! stdin, forwards them to the session server, and writes whatever comes back
//! to stdout. For that to work the console has to stop helping us.
//!
//! On input we disable line buffering, echo, and Ctrl-C handling so that keys
//! reach the remote shell as raw bytes, and enable VT input so that arrow keys
//! and friends arrive as escape sequences rather than console key records.
//!
//! On output we enable VT processing so the escape sequences produced by the
//! remote pty are interpreted rather than printed literally.

use anyhow::{bail, Result};

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::HANDLE,
    System::Console::{
        GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle, SetConsoleMode,
        CONSOLE_SCREEN_BUFFER_INFO, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_ECHO_INPUT,
        ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE,
    },
};

/// Terminal dimensions in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

impl Size {
    /// Clamps degenerate sizes. A zero dimension makes ConPTY unhappy, and a
    /// minimized or hidden window can legitimately report one.
    pub fn sanitized(self) -> Size {
        Size {
            cols: self.cols.max(1),
            rows: self.rows.max(1),
        }
    }
}

impl Default for Size {
    fn default() -> Self {
        Size { cols: 80, rows: 24 }
    }
}

/// Restores the console modes that were in effect before the client started.
///
/// Held for the lifetime of an attach and restored on drop, including when the
/// attach ends via an error path, so a detach never leaves the user's shell
/// with echo turned off.
#[cfg(windows)]
pub struct RawMode {
    stdin: HANDLE,
    stdout: HANDLE,
    previous_input: u32,
    previous_output: u32,
}

#[cfg(windows)]
impl RawMode {
    pub fn enter() -> Result<RawMode> {
        unsafe {
            let stdin = GetStdHandle(STD_INPUT_HANDLE);
            let stdout = GetStdHandle(STD_OUTPUT_HANDLE);

            let mut previous_input = 0u32;
            let mut previous_output = 0u32;
            if GetConsoleMode(stdin, &mut previous_input) == 0 {
                bail!(
                    "wmux attach needs a real console on stdin: {}",
                    std::io::Error::last_os_error()
                );
            }
            if GetConsoleMode(stdout, &mut previous_output) == 0 {
                bail!(
                    "wmux attach needs a real console on stdout: {}",
                    std::io::Error::last_os_error()
                );
            }

            // Strip the line-discipline flags, add VT input.
            let input_mode = (previous_input
                & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
                | ENABLE_VIRTUAL_TERMINAL_INPUT;
            if SetConsoleMode(stdin, input_mode) == 0 {
                bail!(
                    "failed to put the console into raw input mode: {}",
                    std::io::Error::last_os_error()
                );
            }

            // DISABLE_NEWLINE_AUTO_RETURN keeps the remote pty in charge of
            // wrapping; without it the console inserts its own CR at the right
            // margin and the redraw drifts.
            let output_mode = previous_output
                | ENABLE_PROCESSED_OUTPUT
                | ENABLE_VIRTUAL_TERMINAL_PROCESSING
                | DISABLE_NEWLINE_AUTO_RETURN;
            if SetConsoleMode(stdout, output_mode) == 0 {
                SetConsoleMode(stdin, previous_input);
                bail!(
                    "failed to enable VT output processing: {}",
                    std::io::Error::last_os_error()
                );
            }

            Ok(RawMode {
                stdin,
                stdout,
                previous_input,
                previous_output,
            })
        }
    }
}

#[cfg(windows)]
impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            SetConsoleMode(self.stdin, self.previous_input);
            SetConsoleMode(self.stdout, self.previous_output);
        }
    }
}

/// Current size of the console window (not the scrollback buffer).
#[cfg(windows)]
pub fn console_size() -> Size {
    unsafe {
        let stdout = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(stdout, &mut info) == 0 {
            return Size::default();
        }
        let window = info.srWindow;
        Size {
            cols: (window.Right - window.Left + 1).max(1) as u16,
            rows: (window.Bottom - window.Top + 1).max(1) as u16,
        }
        .sanitized()
    }
}

#[cfg(not(windows))]
pub struct RawMode;

#[cfg(not(windows))]
impl RawMode {
    pub fn enter() -> Result<RawMode> {
        bail!("wmux only runs on Windows")
    }
}

#[cfg(not(windows))]
pub fn console_size() -> Size {
    Size::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_dimensions_are_clamped_to_one() {
        let s = Size { cols: 0, rows: 0 }.sanitized();
        assert_eq!(s, Size { cols: 1, rows: 1 });
    }

    #[test]
    fn ordinary_dimensions_survive_sanitizing() {
        let s = Size {
            cols: 120,
            rows: 30,
        };
        assert_eq!(s.sanitized(), s);
    }

    #[test]
    fn default_size_is_a_classic_terminal() {
        assert_eq!(Size::default(), Size { cols: 80, rows: 24 });
    }
}
