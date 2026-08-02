//! Named pipe transport.
//!
//! # Why overlapped I/O
//!
//! A wmux connection is full duplex and genuinely concurrent: one thread
//! streams pty output to the client while another thread reads the client's
//! keystrokes. On Windows, a handle opened in *synchronous* mode serialises
//! every operation through the file object lock, so a blocking `ReadFile`
//! would stall a concurrent `WriteFile` on the same handle and deadlock the
//! session. Opening with `FILE_FLAG_OVERLAPPED` removes that serialisation.
//!
//! Each direction owns its own event object, which is what lets a read and a
//! write be in flight at the same time without their completions colliding.
//!
//! # Access control
//!
//! The pipe is created with an explicit DACL granting access to SYSTEM,
//! Administrators, and the creating user only. Without an explicit descriptor
//! the default would be more permissive than we want for a pipe that carries
//! keystrokes. The first instance is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`
//! so that a squatter who already owns the name causes a hard failure instead
//! of silently intercepting the session.

use anyhow::{anyhow, bail, Context, Result};
use std::io::{self, Read, Write};
use std::ptr;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_MORE_DATA,
    ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_SHARE_MODE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, WaitNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::CreateEventW;
use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

/// Hand-defined because their placement in `windows-sys` moves between
/// releases; the numeric values are part of the stable Win32 ABI.
const SECURITY_SQOS_PRESENT: u32 = 0x0010_0000;
const SECURITY_IDENTIFICATION: u32 = 0x0001_0000;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;

const PIPE_BUFFER_BYTES: u32 = 64 * 1024;

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Owns a security descriptor allocated by the SDDL converter.
struct SecurityDescriptor {
    raw: PSECURITY_DESCRIPTOR,
}

impl SecurityDescriptor {
    /// Builds a descriptor that grants full control to SYSTEM, the local
    /// Administrators group, and `sid`, and denies everyone else by virtue of
    /// the DACL being protected (`P`) so no inherited ACEs widen it.
    fn for_user(sid: &str) -> Result<SecurityDescriptor> {
        let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{sid})");
        let wide = to_wide(&sddl);
        let mut raw: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut raw,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            bail!("failed to build pipe security descriptor: {}", last_error());
        }
        Ok(SecurityDescriptor { raw })
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.raw,
            bInheritHandle: 0,
        }
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.raw.cast());
        }
    }
}

/// A connected pipe endpoint, usable concurrently from a reader thread and a
/// writer thread.
pub struct PipeConn {
    handle: HANDLE,
    read_event: HANDLE,
    write_event: HANDLE,
}

// The handle and its two events are only ever touched through overlapped
// operations, and the read path and write path use disjoint events. Sharing
// the struct across threads is exactly the usage Win32 supports here.
unsafe impl Send for PipeConn {}
unsafe impl Sync for PipeConn {}

impl PipeConn {
    fn from_handle(handle: HANDLE) -> Result<PipeConn> {
        let read_event = new_event()?;
        let write_event = match new_event() {
            Ok(e) => e,
            Err(e) => {
                unsafe { CloseHandle(read_event) };
                return Err(e);
            }
        };
        Ok(PipeConn {
            handle,
            read_event,
            write_event,
        })
    }

    /// Opens a client connection to an existing session pipe.
    pub fn connect(path: &str) -> Result<PipeConn> {
        let wide = to_wide(path);
        // A pipe instance may be momentarily busy while the server hands off
        // to a fresh instance; retry across that window rather than failing.
        for attempt in 0..50 {
            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0 as FILE_SHARE_MODE,
                    ptr::null(),
                    OPEN_EXISTING,
                    // SECURITY_IDENTIFICATION stops a compromised server from
                    // impersonating the attaching user.
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                return PipeConn::from_handle(handle);
            }
            let err = unsafe { GetLastError() };
            if err != ERROR_PIPE_BUSY {
                return Err(anyhow!(last_error()))
                    .with_context(|| format!("could not open session pipe {path}"));
            }
            unsafe { WaitNamedPipeW(wide.as_ptr(), 100) };
            let _ = attempt;
        }
        bail!("session pipe {path} stayed busy; giving up")
    }

    /// Reads into `buf`, returning `Ok(0)` when the peer has gone away.
    pub fn read_bytes(&self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = self.read_event;
        let mut transferred: u32 = 0;

        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::ReadFile(
                self.handle,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut transferred,
                &mut overlapped,
            )
        };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            match err {
                ERROR_IO_PENDING => {
                    let ok = unsafe {
                        GetOverlappedResult(self.handle, &overlapped, &mut transferred, 1)
                    };
                    if ok == 0 {
                        return match unsafe { GetLastError() } {
                            ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED => Ok(0),
                            _ => Err(last_error()),
                        };
                    }
                }
                ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED => return Ok(0),
                // Byte-mode pipes should not produce this, but treat a partial
                // read as success rather than an error if it ever happens.
                ERROR_MORE_DATA => {}
                _ => return Err(last_error()),
            }
        }
        Ok(transferred as usize)
    }

    /// Writes the whole buffer, looping over partial writes.
    pub fn write_bytes(&self, buf: &[u8]) -> io::Result<()> {
        let mut written_total = 0usize;
        while written_total < buf.len() {
            let chunk = &buf[written_total..];
            let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
            overlapped.hEvent = self.write_event;
            let mut transferred: u32 = 0;

            let ok = unsafe {
                windows_sys::Win32::Storage::FileSystem::WriteFile(
                    self.handle,
                    chunk.as_ptr(),
                    chunk.len() as u32,
                    &mut transferred,
                    &mut overlapped,
                )
            };
            if ok == 0 {
                let err = unsafe { GetLastError() };
                match err {
                    ERROR_IO_PENDING => {
                        let ok = unsafe {
                            GetOverlappedResult(self.handle, &overlapped, &mut transferred, 1)
                        };
                        if ok == 0 {
                            return Err(last_error());
                        }
                    }
                    _ => return Err(last_error()),
                }
            }
            if transferred == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "named pipe accepted zero bytes",
                ));
            }
            written_total += transferred as usize;
        }
        Ok(())
    }
}

impl Drop for PipeConn {
    fn drop(&mut self) {
        // Deliberately *not* DisconnectNamedPipe: on the server side that
        // discards anything the client has not read yet, which would lose the
        // final `Exited` frame every time a session ends. Closing the handle
        // instead leaves buffered bytes readable, and the peer sees a clean
        // ERROR_BROKEN_PIPE once it has drained them.
        unsafe {
            CloseHandle(self.handle);
            CloseHandle(self.read_event);
            CloseHandle(self.write_event);
        }
    }
}

// Blanket `Read`/`Write` on a shared reference so the framing helpers in
// `protocol` work directly against an `Arc<PipeConn>` held by two threads.
impl Read for &PipeConn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        PipeConn::read_bytes(self, buf)
    }
}

impl Write for &PipeConn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        PipeConn::write_bytes(self, buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn new_event() -> Result<HANDLE> {
    // Manual-reset, initially unsignalled, unnamed.
    let handle = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if handle.is_null() {
        bail!("CreateEvent failed: {}", last_error());
    }
    Ok(handle)
}

/// Accepts client connections for one session.
pub struct PipeListener {
    path: String,
    wide_path: Vec<u16>,
    security: SecurityDescriptor,
    /// The instance currently waiting for a client. Keeping one always
    /// outstanding is what makes the pipe name visible to `wmux ls`.
    pending: HANDLE,
}

// A listener is owned by one thread at a time. The raw pipe handle and the
// security descriptor it holds are process-wide resources with no thread
// affinity, so moving the listener between threads is sound.
unsafe impl Send for PipeListener {}

impl PipeListener {
    /// Claims the pipe name, failing if another process already owns it.
    pub fn bind(path: &str, sid: &str) -> Result<PipeListener> {
        let security = SecurityDescriptor::for_user(sid)?;
        let wide_path = to_wide(path);
        let pending = create_instance(&wide_path, &security, true)
            .with_context(|| format!("could not create session pipe {path}"))?;
        Ok(PipeListener {
            path: path.to_string(),
            wide_path,
            security,
            pending,
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Blocks until a client connects, then hands back the connection and
    /// stands up a fresh instance for the next one.
    ///
    /// If the wait fails the pending instance is replaced before returning.
    /// This matters more than it looks: anything that merely *probes* the pipe
    /// path — `Path::exists`, a directory tool, an antivirus scanner — opens
    /// and immediately closes a client handle, which consumes the listening
    /// instance and leaves `ConnectNamedPipe` returning ERROR_NO_DATA forever.
    /// Without this recovery the server spins on a dead handle and the session
    /// becomes unreachable.
    pub fn accept(&mut self) -> Result<PipeConn> {
        if let Err(e) = wait_for_connection(self.pending) {
            unsafe { CloseHandle(self.pending) };
            self.pending = create_instance(&self.wide_path, &self.security, false)
                .context("could not replace a poisoned pipe instance")?;
            return Err(e);
        }
        let connected = self.pending;
        // Create the replacement before returning so there is always an
        // instance listening; the accepted handle keeps the name alive in the
        // meantime.
        self.pending = create_instance(&self.wide_path, &self.security, false)
            .context("could not create a replacement pipe instance")?;
        PipeConn::from_handle(connected).inspect_err(|_| {
            // Do not leak the accepted handle if we could not build the
            // connection's event objects.
            unsafe { CloseHandle(connected) };
        })
    }
}

impl Drop for PipeListener {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.pending);
        }
    }
}

fn create_instance(
    wide_path: &[u16],
    security: &SecurityDescriptor,
    first: bool,
) -> Result<HANDLE> {
    let attrs = security.attributes();
    let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let handle = unsafe {
        CreateNamedPipeW(
            wide_path.as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUFFER_BYTES,
            PIPE_BUFFER_BYTES,
            0,
            &attrs,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        bail!("CreateNamedPipe failed: {}", last_error());
    }
    Ok(handle)
}

fn wait_for_connection(handle: HANDLE) -> Result<()> {
    let event = new_event()?;
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.hEvent = event;

    let ok = unsafe { ConnectNamedPipe(handle, &mut overlapped) };
    let result = if ok != 0 {
        Ok(())
    } else {
        match unsafe { GetLastError() } {
            // A client raced in between CreateNamedPipe and ConnectNamedPipe.
            ERROR_PIPE_CONNECTED => Ok(()),
            ERROR_IO_PENDING => {
                let mut transferred: u32 = 0;
                let ok = unsafe { GetOverlappedResult(handle, &overlapped, &mut transferred, 1) };
                if ok == 0 {
                    Err(anyhow!("ConnectNamedPipe failed: {}", last_error()))
                } else {
                    Ok(())
                }
            }
            _ => Err(anyhow!("ConnectNamedPipe failed: {}", last_error())),
        }
    };
    unsafe { CloseHandle(event) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings_are_nul_terminated() {
        let w = to_wide("ab");
        assert_eq!(w, vec![b'a' as u16, b'b' as u16, 0]);
    }

    #[test]
    fn security_descriptor_builds_for_a_well_known_sid() {
        // S-1-5-18 is LocalSystem; always resolvable, so this exercises the
        // SDDL path without depending on the test account.
        SecurityDescriptor::for_user("S-1-5-18").expect("SDDL should convert");
    }

    #[test]
    fn security_descriptor_rejects_a_malformed_sid() {
        assert!(SecurityDescriptor::for_user("not-a-sid").is_err());
    }

    #[test]
    fn listener_and_client_exchange_frames() {
        use crate::protocol::{ClientMsg, ServerMsg};
        use std::sync::Arc;

        let sid = crate::session::current_user_sid().unwrap();
        // Unique per test run so repeated runs never collide.
        let path = format!(r"\\.\pipe\wmux-test.{}.{}", sid, std::process::id());

        let mut listener = PipeListener::bind(&path, &sid).unwrap();
        let server = std::thread::spawn(move || {
            let conn = Arc::new(listener.accept().unwrap());
            let msg = ClientMsg::read_from(&mut &*conn).unwrap();
            assert_eq!(msg, ClientMsg::Attach { cols: 90, rows: 20 });
            ServerMsg::Repaint(b"hello".to_vec())
                .write_to(&mut &*conn)
                .unwrap();
        });

        let client = Arc::new(PipeConn::connect(&path).unwrap());
        ClientMsg::Attach { cols: 90, rows: 20 }
            .write_to(&mut &*client)
            .unwrap();
        let reply = ServerMsg::read_from(&mut &*client).unwrap();
        assert_eq!(reply, ServerMsg::Repaint(b"hello".to_vec()));

        server.join().unwrap();
    }

    #[test]
    fn binding_the_same_name_twice_fails() {
        let sid = crate::session::current_user_sid().unwrap();
        let path = format!(r"\\.\pipe\wmux-test-dup.{}.{}", sid, std::process::id());
        let _first = PipeListener::bind(&path, &sid).unwrap();
        assert!(
            PipeListener::bind(&path, &sid).is_err(),
            "FILE_FLAG_FIRST_PIPE_INSTANCE should reject the second bind"
        );
    }
}
