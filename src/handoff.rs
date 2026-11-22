//! Receives console programs from Windows' default-terminal handoff.
//!
//! When a console program starts without a console of its own (Start menu,
//! Explorer, Win+R, a shortcut), the inbox conhost reads
//! `HKCU\Console\%%Startup` and, if a delegation pair is set, hands the
//! console to the *delegation console* — OpenConsole, which runs it as a
//! headless pseudoconsole — and OpenConsole hands the pseudoconsole's pipes to
//! the *delegation terminal* by calling
//! `ITerminalHandoff3::EstablishPtyHandoff` on a COM local server.
//!
//! wmux is that local server. COM starts `wmux com-server -Embedding`, the
//! class is registered `REGCLS_SINGLEUSE` so every handoff gets a fresh
//! process, and that process becomes the session server for the program it
//! was given. It has no console and is not in any window's process tree, so
//! like a `DETACHED_PROCESS` server it survives everything but a logoff.
//!
//! The COM objects are hand-rolled: two interfaces with one method each do
//! not justify a COM framework, and the objects are statics, so reference
//! counting is a formality. Handle marshalling is done by the proxy DLL the
//! Windows Terminal package registers; see [`crate::default_host`].
//!
//! When the client is wmux itself — the viewer `--visible` opens — the
//! handoff is relayed to whichever terminal was the default before wmux, so
//! the viewer shows up there instead of being wrapped in yet another session.

use std::ffi::c_void;
use std::os::windows::io::{FromRawHandle, IntoRawHandle, OwnedHandle, RawHandle};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use windows_sys::core::{GUID, HRESULT};
use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, E_FAIL, E_NOINTERFACE, E_POINTER,
    E_UNEXPECTED, HANDLE, S_OK,
};
use windows_sys::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoRegisterClassObject, CoRevokeClassObject,
    CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED, REGCLS_SINGLEUSE,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, GetProcessId, OpenProcess, QueryFullProcessImageNameW,
    TerminateProcess, WaitForSingleObject, INFINITE, PROCESS_NAME_WIN32, PROCESS_TERMINATE,
};

use crate::default_host;
use crate::server::{self, ExitPolicy, ForeignPty, PtyControl};

/// wmux's delegation terminal class.
pub const WMUX_TERMINAL_CLSID: GUID = GUID::from_u128(0x01A200F4_72F9_7B41_98E3_DEE225A6B2A0);
/// The OpenConsole each Windows Terminal package registers as its delegation
/// console. wmux uses whichever is installed, Preview first.
pub const PREVIEW_OPENCONSOLE_CLSID: GUID = GUID::from_u128(0x06EC847C_C0A5_46B8_92CB_7C92F6E35CD5);
pub const OPENCONSOLE_CLSID: GUID = GUID::from_u128(0x2EACA947_7F5F_4CFA_BA87_8F7FBEEFBE69);
/// Windows Terminal Preview's and Windows Terminal's delegation terminals,
/// tried in that order by `--visible` after the previous default. Optional:
/// when neither is installed the viewer gets a plain console window.
pub const WINDOWS_TERMINAL_PREVIEW_CLSID: GUID =
    GUID::from_u128(0x86633F1F_6454_40EC_89CE_DA4EBA977EE2);
pub const WINDOWS_TERMINAL_CLSID: GUID = GUID::from_u128(0xE12CFF52_A866_4C77_9A90_F570A7AA2C6B);
/// The two special values Settings writes: the inbox console host, and
/// "Let Windows decide".
pub const CONHOST_CLSID: GUID = GUID::from_u128(0xB23D10C0_E52E_411E_9D5B_C09FDF709C7D);
pub const LET_WINDOWS_DECIDE: GUID = GUID::from_u128(0);

pub const IID_ITERMINAL_HANDOFF3: GUID = GUID::from_u128(0x6F23DA90_15C5_4203_9DB0_64E73F1B1B00);

const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_C000_000000000046);
const IID_ICLASSFACTORY: GUID = GUID::from_u128(0x00000001_0000_0000_C000_000000000046);
const CLASS_E_NOAGGREGATION: HRESULT = 0x8004_0110_u32 as HRESULT;

/// ConPTY's out-of-band resize message, written to the signal pipe.
const PTY_SIGNAL_RESIZE_WINDOW: u16 = 8;

/// The console a handed-off program gets. OpenConsole creates it at the
/// registry default; wmux resizes it to this so the model and the console
/// agree, and an attaching client resizes it again to its own terminal.
const HANDOFF_SIZE: (u16, u16) = (120, 30);

/// How long a COM-launched server waits for the call it was started for.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(30);

/// After relaying a handoff the process exits, but not before COM has sent
/// the reply: the call returns on an RPC thread after it hands us the news.
const RELAY_REPLY_GRACE: Duration = Duration::from_secs(1);

/// The prefix `wmux ls` shows in the command column of a handed-off session.
pub const COMMAND_PREFIX: &str = "[default-host]";

/// `TERMINAL_STARTUP_INFO` from `ITerminalHandoff.idl`.
#[repr(C)]
pub struct TerminalStartupInfo {
    title: *const u16,
    icon_path: *const u16,
    icon_index: i32,
    x: u32,
    y: u32,
    x_size: u32,
    y_size: u32,
    x_count_chars: u32,
    y_count_chars: u32,
    fill_attribute: u32,
    flags: u32,
    show_window: u16,
}

type EstablishPtyHandoff = unsafe extern "system" fn(
    this: *mut c_void,
    input: *mut HANDLE,
    output: *mut HANDLE,
    signal: HANDLE,
    reference: HANDLE,
    server: HANDLE,
    client: HANDLE,
    startup_info: *const TerminalStartupInfo,
) -> HRESULT;

#[repr(C)]
struct HandoffVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    establish_pty_handoff: EstablishPtyHandoff,
}

#[repr(C)]
struct ClassFactoryVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    create_instance: unsafe extern "system" fn(
        *mut c_void,
        *mut c_void,
        *const GUID,
        *mut *mut c_void,
    ) -> HRESULT,
    lock_server: unsafe extern "system" fn(*mut c_void, i32) -> HRESULT,
}

/// A COM object is a pointer to a pointer to its vtable.
#[repr(C)]
struct ComObject<V: 'static> {
    vtbl: &'static V,
}

// The objects are immutable statics holding function pointers.
unsafe impl<V> Sync for ComObject<V> {}

static HANDOFF_VTBL: HandoffVtbl = HandoffVtbl {
    query_interface: handoff_query_interface,
    add_ref: static_add_ref,
    release: static_release,
    establish_pty_handoff,
};
static HANDOFF: ComObject<HandoffVtbl> = ComObject {
    vtbl: &HANDOFF_VTBL,
};

static FACTORY_VTBL: ClassFactoryVtbl = ClassFactoryVtbl {
    query_interface: factory_query_interface,
    add_ref: static_add_ref,
    release: static_release,
    create_instance: factory_create_instance,
    lock_server: factory_lock_server,
};
static FACTORY: ComObject<ClassFactoryVtbl> = ComObject {
    vtbl: &FACTORY_VTBL,
};

fn object_ptr<V>(object: &'static ComObject<V>) -> *mut c_void {
    object as *const ComObject<V> as *mut c_void
}

pub fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

/// The registry spelling of a GUID: `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`.
pub fn guid_string(g: &GUID) -> String {
    let d = g.data4;
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1, g.data2, g.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
    )
}

/// Parses the registry spelling, braces optional.
pub fn parse_guid(s: &str) -> Option<GUID> {
    let hex: String = s
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .split('-')
        .collect();
    if hex.len() != 32 {
        return None;
    }
    u128::from_str_radix(&hex, 16).ok().map(GUID::from_u128)
}

unsafe extern "system" fn static_add_ref(_this: *mut c_void) -> u32 {
    1
}

unsafe extern "system" fn static_release(_this: *mut c_void) -> u32 {
    1
}

unsafe fn answer_query(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
    supported: &GUID,
) -> HRESULT {
    if ppv.is_null() || riid.is_null() {
        return E_POINTER;
    }
    let riid = &*riid;
    if guid_eq(riid, &IID_IUNKNOWN) || guid_eq(riid, supported) {
        *ppv = this;
        S_OK
    } else {
        *ppv = std::ptr::null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "system" fn handoff_query_interface(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    answer_query(this, riid, ppv, &IID_ITERMINAL_HANDOFF3)
}

unsafe extern "system" fn factory_query_interface(
    this: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    answer_query(this, riid, ppv, &IID_ICLASSFACTORY)
}

unsafe extern "system" fn factory_create_instance(
    _this: *mut c_void,
    outer: *mut c_void,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if !outer.is_null() {
        return CLASS_E_NOAGGREGATION;
    }
    handoff_query_interface(object_ptr(&HANDOFF), riid, ppv)
}

unsafe extern "system" fn factory_lock_server(_this: *mut c_void, _lock: i32) -> HRESULT {
    S_OK
}

/// Everything a handoff delivers, with handles this process owns.
struct Handoff {
    /// Our end of the program's input: wmux writes keystrokes here.
    input: OwnedHandle,
    /// Our end of the program's output: wmux reads VT from here.
    output: OwnedHandle,
    signal: OwnedHandle,
    /// Keeps the console session alive until wmux lets go of it.
    reference: OwnedHandle,
    /// OpenConsole. Held so the console host outlives nothing it should not.
    _server: OwnedHandle,
    client: OwnedHandle,
    image: String,
    title: String,
}

enum Received {
    Session(Handoff),
    Relayed,
}

static RECEIVED: Mutex<Option<mpsc::Sender<Received>>> = Mutex::new(None);

fn deliver(received: Received) {
    if let Some(tx) = RECEIVED.lock().unwrap().as_ref() {
        let _ = tx.send(received);
    }
}

unsafe extern "system" fn establish_pty_handoff(
    _this: *mut c_void,
    input: *mut HANDLE,
    output: *mut HANDLE,
    signal: HANDLE,
    reference: HANDLE,
    server: HANDLE,
    client: HANDLE,
    startup_info: *const TerminalStartupInfo,
) -> HRESULT {
    // A panic must not unwind into the RPC runtime.
    let outcome = std::panic::catch_unwind(|| {
        receive(
            input,
            output,
            signal,
            reference,
            server,
            client,
            startup_info,
        )
    });
    match outcome {
        Ok(Ok(hr)) => hr,
        Ok(Err(e)) => {
            server::log(&format!("handoff failed: {e:#}"));
            E_FAIL
        }
        Err(_) => E_UNEXPECTED,
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn receive(
    input: *mut HANDLE,
    output: *mut HANDLE,
    signal: HANDLE,
    reference: HANDLE,
    server: HANDLE,
    client: HANDLE,
    startup_info: *const TerminalStartupInfo,
) -> Result<HRESULT> {
    if input.is_null() || output.is_null() {
        return Ok(E_POINTER);
    }
    let image = process_image(client).unwrap_or_default();
    server::log(&format!("handoff received for {image:?}"));

    if is_wmux(&image) {
        let hr = relay(
            input,
            output,
            signal,
            reference,
            server,
            client,
            startup_info,
        );
        server::log(&format!("relayed wmux's own console: 0x{:08x}", hr as u32));
        deliver(Received::Relayed);
        return Ok(hr);
    }

    // Two anonymous pipes rather than one duplex pipe: each direction is its
    // own file object, so a blocking read never serialises behind a write.
    let (their_input, our_input) = anonymous_pipe().context("input pipe")?;
    let (our_output, their_output) = anonymous_pipe().context("output pipe")?;

    let title = if startup_info.is_null() {
        String::new()
    } else {
        bstr_to_string((*startup_info).title)
    };

    // COM closes the handles it passed in once this call returns.
    let handoff = Handoff {
        input: our_input,
        output: our_output,
        signal: duplicate(signal).context("signal handle")?,
        reference: duplicate(reference).context("reference handle")?,
        _server: duplicate(server).context("server process handle")?,
        client: duplicate(client).context("client process handle")?,
        image,
        title,
    };

    // Ownership of OpenConsole's ends passes to COM, which duplicates them
    // into OpenConsole and closes ours.
    *input = their_input.into_raw_handle() as HANDLE;
    *output = their_output.into_raw_handle() as HANDLE;
    deliver(Received::Session(handoff));
    Ok(S_OK)
}

/// Passes a handoff straight through to another terminal, or declines it so
/// Windows falls back to a console window.
///
/// Order: the terminal that was the default before wmux, then Windows
/// Terminal Preview, then Windows Terminal, then conhost.
#[allow(clippy::too_many_arguments)]
unsafe fn relay(
    input: *mut HANDLE,
    output: *mut HANDLE,
    signal: HANDLE,
    reference: HANDLE,
    server: HANDLE,
    client: HANDLE,
    startup_info: *const TerminalStartupInfo,
) -> HRESULT {
    let mut candidates = Vec::new();
    match default_host::previous_terminal() {
        // Someone who picked the console host explicitly gets it.
        Some(previous) if guid_eq(&previous, &CONHOST_CLSID) => return E_FAIL,
        // The all-zero "Let Windows decide" is not a class to activate.
        Some(previous) if !guid_eq(&previous, &LET_WINDOWS_DECIDE) => candidates.push(previous),
        _ => {}
    }
    candidates.push(WINDOWS_TERMINAL_PREVIEW_CLSID);
    candidates.push(WINDOWS_TERMINAL_CLSID);

    let mut tried: Vec<GUID> = Vec::new();
    for clsid in candidates {
        if guid_eq(&clsid, &WMUX_TERMINAL_CLSID) || tried.iter().any(|t| guid_eq(t, &clsid)) {
            continue;
        }
        tried.push(clsid);
        let mut terminal: *mut c_void = std::ptr::null_mut();
        let hr = CoCreateInstance(
            &clsid,
            std::ptr::null_mut(),
            CLSCTX_LOCAL_SERVER,
            &IID_ITERMINAL_HANDOFF3,
            &mut terminal,
        );
        if hr < 0 || terminal.is_null() {
            server::log(&format!(
                "terminal {} is not available: 0x{:08x}",
                guid_string(&clsid),
                hr as u32
            ));
            continue;
        }
        let vtbl = *(terminal as *const *const HandoffVtbl);
        let hr = ((*vtbl).establish_pty_handoff)(
            terminal,
            input,
            output,
            signal,
            reference,
            server,
            client,
            startup_info,
        );
        ((*vtbl).release)(terminal);
        return hr;
    }
    // Declining makes the console host show the program in its own window.
    E_FAIL
}

fn is_wmux(image: &str) -> bool {
    std::path::Path::new(image)
        .file_name()
        .is_some_and(|f| f.eq_ignore_ascii_case("wmux.exe"))
}

fn anonymous_pipe() -> Result<(OwnedHandle, OwnedHandle)> {
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    if unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    unsafe {
        Ok((
            OwnedHandle::from_raw_handle(read as RawHandle),
            OwnedHandle::from_raw_handle(write as RawHandle),
        ))
    }
}

fn duplicate(handle: HANDLE) -> Result<OwnedHandle> {
    let mut copy: HANDLE = std::ptr::null_mut();
    let ok = unsafe {
        let me = GetCurrentProcess();
        DuplicateHandle(me, handle, me, &mut copy, 0, 0, DUPLICATE_SAME_ACCESS)
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(copy as RawHandle) })
}

fn raw(handle: &OwnedHandle) -> HANDLE {
    use std::os::windows::io::AsRawHandle;
    handle.as_raw_handle() as HANDLE
}

fn process_image(process: HANDLE) -> Option<String> {
    let mut buf = vec![0u16; 32 * 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe {
        QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len)
    };
    (ok != 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
}

/// A BSTR carries its byte length in the four bytes before the pointer.
unsafe fn bstr_to_string(bstr: *const u16) -> String {
    if bstr.is_null() {
        return String::new();
    }
    let bytes = *(bstr as *const u32).sub(1);
    let units = std::slice::from_raw_parts(bstr, (bytes / 2) as usize);
    String::from_utf16_lossy(units)
}

/// `cmd-4120` for `C:\Windows\System32\cmd.exe`, process 4120.
pub fn session_name(image: &str, pid: u32) -> String {
    let stem = std::path::Path::new(image)
        .file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let mut stem: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    // Leave room for the pid and a collision suffix within MAX_NAME_LEN.
    stem.truncate(40);
    if stem.is_empty() {
        stem.push_str("console");
    }
    format!("{stem}-{pid}")
}

/// The resize packet ConPTY expects on its signal pipe: three little-endian
/// u16s, the message id then columns then rows.
pub fn resize_packet(cols: u16, rows: u16) -> [u8; 6] {
    let mut packet = [0u8; 6];
    packet[0..2].copy_from_slice(&PTY_SIGNAL_RESIZE_WINDOW.to_le_bytes());
    packet[2..4].copy_from_slice(&cols.to_le_bytes());
    packet[4..6].copy_from_slice(&rows.to_le_bytes());
    packet
}

/// The pseudoconsole as OpenConsole lent it to us.
struct HandedOffPty {
    signal: Arc<Mutex<Option<std::fs::File>>>,
    client: OwnedHandle,
}

impl PtyControl for HandedOffPty {
    fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        use std::io::Write;
        match self.signal.lock().unwrap().as_mut() {
            Some(signal) => Ok(signal.write_all(&resize_packet(cols, rows))?),
            None => bail!("the console has already been closed"),
        }
    }

    fn kill(&mut self) {
        // The client handle OpenConsole passes is for waiting on the program,
        // not for ending it, so reopen it with the right to terminate. Holding
        // the original handle keeps the pid from being reused meanwhile.
        unsafe {
            if TerminateProcess(raw(&self.client), 1) != 0 {
                return;
            }
            let pid = GetProcessId(raw(&self.client));
            let process = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if process.is_null() {
                server::log(&format!(
                    "could not open process {pid} to kill it: {}",
                    std::io::Error::last_os_error()
                ));
                return;
            }
            if TerminateProcess(process, 1) == 0 {
                server::log(&format!(
                    "could not kill process {pid}: {}",
                    std::io::Error::last_os_error()
                ));
            }
            CloseHandle(process);
        }
    }

    fn release(&mut self) {
        self.signal.lock().unwrap().take();
    }
}

/// `wmux com-server`: what COM runs when OpenConsole asks for wmux.
pub fn com_server_main() -> Result<()> {
    let (tx, rx) = mpsc::channel();
    *RECEIVED.lock().unwrap() = Some(tx);

    let hr = unsafe { CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED as u32) };
    if hr < 0 {
        bail!("CoInitializeEx failed: 0x{:08x}", hr as u32);
    }
    let mut cookie = 0u32;
    let hr = unsafe {
        CoRegisterClassObject(
            &WMUX_TERMINAL_CLSID,
            object_ptr(&FACTORY),
            CLSCTX_LOCAL_SERVER,
            REGCLS_SINGLEUSE as u32,
            &mut cookie,
        )
    };
    if hr < 0 {
        bail!("CoRegisterClassObject failed: 0x{:08x}", hr as u32);
    }
    server::log("com-server waiting for a handoff");

    let received = rx.recv_timeout(HANDOFF_TIMEOUT);
    unsafe {
        CoRevokeClassObject(cookie);
    }
    match received {
        Ok(Received::Session(handoff)) => host(handoff),
        Ok(Received::Relayed) => {
            std::thread::sleep(RELAY_REPLY_GRACE);
            Ok(())
        }
        Err(_) => bail!("no handoff arrived within {HANDOFF_TIMEOUT:?}"),
    }
}

/// Becomes the session server for a handed-off program.
fn host(handoff: Handoff) -> Result<()> {
    let pid = unsafe { GetProcessId(raw(&handoff.client)) };
    let base = session_name(&handoff.image, pid);
    let (name, listener) = claim_name(&base)?;
    server::log(&format!("hosting {:?} as session {name}", handoff.image));

    let shown = if handoff.title.is_empty() {
        &handoff.image
    } else {
        &handoff.title
    };
    let command = format!("{COMMAND_PREFIX} {shown}");

    let signal = Arc::new(Mutex::new(Some(std::fs::File::from(handoff.signal))));
    let client_wait = duplicate(raw(&handoff.client))?;
    let reference = handoff.reference;
    let closing_signal = signal.clone();
    let wait = move || -> i32 {
        let mut code = 0u32;
        unsafe {
            WaitForSingleObject(raw(&client_wait), INFINITE);
            if GetExitCodeProcess(raw(&client_wait), &mut code) == 0 {
                code = u32::MAX;
            }
        }
        // Letting go of the console is what makes OpenConsole flush its last
        // output and close the pipe, so the final screen reaches the model.
        drop(reference);
        closing_signal.lock().unwrap().take();
        code as i32
    };

    if default_host::is_visible() {
        open_viewer(&name);
    }

    server::run_foreign(
        listener,
        ForeignPty {
            reader: Box::new(std::fs::File::from(handoff.output)),
            writer: Box::new(std::fs::File::from(handoff.input)),
            control: Box::new(HandedOffPty {
                signal,
                client: handoff.client,
            }),
            wait: Box::new(wait),
        },
        HANDOFF_SIZE.0,
        HANDOFF_SIZE.1,
        command,
        ExitPolicy { keep_failed: true },
    )
}

/// Pids are reused, and a kept session can outlive its program by days.
fn claim_name(base: &str) -> Result<(String, crate::pipe::PipeListener)> {
    let mut last = None;
    for n in 1..=9 {
        let name = if n == 1 {
            base.to_string()
        } else {
            format!("{base}-{n}")
        };
        match server::bind(&name) {
            Ok(listener) => return Ok((name, listener)),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no free session name")))
}

/// `--visible`: show the session through the previous default terminal.
///
/// The viewer is `wmux attach` in a new console. That console goes through
/// the handoff like any other, reaches a fresh `wmux com-server`, and is
/// relayed onward because its program is wmux.
fn open_viewer(name: &str) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let result = std::process::Command::new(exe)
        .arg("attach")
        .arg(name)
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn();
    if let Err(e) = result {
        server::log(&format!("could not open a viewer for {name}: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_is_named_after_its_program_and_pid() {
        let image = r"C:\Windows\System32\cmd.exe";
        let pid = 4120;
        assert_eq!(session_name(image, pid), "cmd-4120");
    }

    #[test]
    fn characters_a_session_name_cannot_hold_become_dashes() {
        let image = r"C:\Tools\My App.v2.exe";
        let pid = 7;
        let name = session_name(image, pid);
        assert_eq!(name, "my-app-v2-7");
        crate::session::validate_name(&name).unwrap();
    }

    #[test]
    fn a_program_with_no_usable_name_still_gets_a_valid_session_name() {
        let image = "";
        let pid = 99;
        let name = session_name(image, pid);
        assert_eq!(name, "console-99");
        crate::session::validate_name(&name).unwrap();
    }

    #[test]
    fn the_resize_packet_matches_conpty_layout() {
        let cols: u16 = 120;
        let rows: u16 = 30;
        let packet = resize_packet(cols, rows);
        assert_eq!(packet, [8, 0, 120, 0, 30, 0]);
    }

    #[test]
    fn guids_use_the_registry_spelling_and_parse_back() {
        let text = "{2EACA947-7F5F-4CFA-BA87-8F7FBEEFBE69}";
        assert_eq!(guid_string(&OPENCONSOLE_CLSID), text);
        let parsed = parse_guid(text).unwrap();
        assert!(guid_eq(&parsed, &OPENCONSOLE_CLSID));
    }

    #[test]
    fn a_malformed_guid_is_rejected() {
        let truncated = "{2EACA947-7F5F-4CFA}";
        assert!(parse_guid(truncated).is_none());
    }

    #[test]
    fn only_wmux_itself_is_relayed() {
        let viewer = r"C:\Users\me\AppData\Local\Microsoft\WinGet\Links\WMUX.EXE";
        let program = r"C:\Windows\System32\cmd.exe";
        assert!(is_wmux(viewer));
        assert!(!is_wmux(program));
    }
}
