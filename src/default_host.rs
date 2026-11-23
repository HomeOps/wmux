//! `wmux default-host`: registers wmux as Windows' default terminal.
//!
//! Windows has no API for this. Settings › For developers › Terminal writes
//! two CLSIDs to `HKCU\Console\%%Startup`, and its dropdown only lists MSIX
//! packages. wmux ships as a portable zip, so it writes the same values
//! itself:
//!
//! | Value | Points at |
//! |---|---|
//! | `DelegationConsole` | the OpenConsole of an installed Windows Terminal |
//! | `DelegationTerminal` | wmux, registered at `HKCU\Software\Classes\CLSID\{wmux}` |
//!
//! OpenConsole and the proxy that marshals the handoff interfaces belong to
//! the Windows Terminal package, which registers both in Windows' packaged
//! COM catalog. wmux neither ships nor copies them; it finds the package the
//! way `Get-AppxPackage` does and points the delegation console at it, so
//! Windows Terminal (or Preview) has to be installed.
//!
//! The values that were there before are saved under
//! `HKCU\Software\wmux\DefaultHost` and put back by `disable`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use windows_sys::core::{GUID, PWSTR};
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};
use windows_sys::Win32::Storage::Packaging::Appx::{
    GetPackagePathByFullName, GetPackagesByPackageFamily,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegGetValueW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
    RRF_RT_REG_SZ,
};

use crate::handoff::{
    guid_eq, guid_string, parse_guid, LET_WINDOWS_DECIDE, OPENCONSOLE_CLSID,
    PREVIEW_OPENCONSOLE_CLSID, WMUX_TERMINAL_CLSID,
};

const STARTUP_KEY: &str = r"Console\%%Startup";
const CONSOLE_VALUE: &str = "DelegationConsole";
const TERMINAL_VALUE: &str = "DelegationTerminal";

const STATE_KEY: &str = r"Software\wmux\DefaultHost";
const PREVIOUS_CONSOLE: &str = "PreviousConsole";
const PREVIOUS_TERMINAL: &str = "PreviousTerminal";
const VISIBLE: &str = "Visible";

const OPENCONSOLE_EXE: &str = "OpenConsole.exe";

/// A Windows Terminal package that provides OpenConsole.
struct TerminalPackage {
    name: &'static str,
    family: &'static str,
    console: GUID,
}

/// Tried in order; the first one installed is used.
const TERMINAL_PACKAGES: [TerminalPackage; 2] = [
    TerminalPackage {
        name: "Windows Terminal Preview",
        family: "Microsoft.WindowsTerminalPreview_8wekyb3d8bbwe",
        console: PREVIEW_OPENCONSOLE_CLSID,
    },
    TerminalPackage {
        name: "Windows Terminal",
        family: "Microsoft.WindowsTerminal_8wekyb3d8bbwe",
        console: OPENCONSOLE_CLSID,
    },
];

fn class_key(clsid: &GUID) -> String {
    format!(r"Software\Classes\CLSID\{}", guid_string(clsid))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Writes a `REG_SZ` under HKCU, creating the key. `None` is the default value.
fn set_string(subkey: &str, value: Option<&str>, data: &str) -> Result<()> {
    let mut key: HKEY = std::ptr::null_mut();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(subkey).as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    };
    if status != ERROR_SUCCESS {
        bail!("could not open HKCU\\{subkey}: error {status}");
    }
    let name = value.map(wide);
    let data = wide(data);
    let status = unsafe {
        RegSetValueExW(
            key,
            name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
            0,
            REG_SZ,
            data.as_ptr() as *const u8,
            (data.len() * 2) as u32,
        )
    };
    unsafe {
        RegCloseKey(key);
    }
    if status != ERROR_SUCCESS {
        bail!("could not write HKCU\\{subkey}: error {status}");
    }
    Ok(())
}

fn get_string(subkey: &str, value: Option<&str>) -> Option<String> {
    get_string_in(HKEY_CURRENT_USER, subkey, value)
}

fn get_string_in(root: HKEY, subkey: &str, value: Option<&str>) -> Option<String> {
    let subkey_w = wide(subkey);
    let name = value.map(wide);
    let name_ptr = name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr());
    let mut bytes = 0u32;
    let status = unsafe {
        RegGetValueW(
            root,
            subkey_w.as_ptr(),
            name_ptr,
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut bytes,
        )
    };
    if status != ERROR_SUCCESS || bytes == 0 {
        return None;
    }
    let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
    let status = unsafe {
        RegGetValueW(
            root,
            subkey_w.as_ptr(),
            name_ptr,
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut _,
            &mut bytes,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    Some(from_wide(&buf))
}

/// Removes a key and everything under it; a missing key is not an error.
fn delete_tree(subkey: &str) {
    unsafe {
        RegDeleteTreeW(HKEY_CURRENT_USER, wide(subkey).as_ptr());
    }
}

fn get_guid(subkey: &str, value: &str) -> Option<GUID> {
    get_string(subkey, Some(value)).and_then(|s| parse_guid(&s))
}

/// Where a package family is installed for this user, or `None` when it is
/// not: the `InstallLocation` that `Get-AppxPackage` reports.
fn package_install_dir(family: &str) -> Option<PathBuf> {
    let family_w = wide(family);
    let mut count = 0u32;
    let mut chars = 0u32;
    let status = unsafe {
        GetPackagesByPackageFamily(
            family_w.as_ptr(),
            &mut count,
            std::ptr::null_mut(),
            &mut chars,
            std::ptr::null_mut(),
        )
    };
    if status != ERROR_INSUFFICIENT_BUFFER || count == 0 {
        return None;
    }
    let mut names: Vec<PWSTR> = vec![std::ptr::null_mut(); count as usize];
    let mut buffer = vec![0u16; chars as usize];
    let status = unsafe {
        GetPackagesByPackageFamily(
            family_w.as_ptr(),
            &mut count,
            names.as_mut_ptr(),
            &mut chars,
            buffer.as_mut_ptr(),
        )
    };
    if status != ERROR_SUCCESS || count == 0 {
        return None;
    }
    let full_name = names[0];

    let mut len = 0u32;
    let status = unsafe { GetPackagePathByFullName(full_name, &mut len, std::ptr::null_mut()) };
    if status != ERROR_INSUFFICIENT_BUFFER {
        return None;
    }
    let mut path = vec![0u16; len as usize];
    let status = unsafe { GetPackagePathByFullName(full_name, &mut len, path.as_mut_ptr()) };
    if status != ERROR_SUCCESS {
        return None;
    }
    Some(PathBuf::from(from_wide(&path)))
}

/// Windows 11 24H2, the first build that honours the manifest's
/// `consoleAllocationPolicy`; see `build.rs`.
const MIN_BUILD: u32 = 26100;

/// The running Windows build, e.g. 26100. `GetVersionEx` lies to an
/// unmanifested caller, the registry does not.
fn windows_build() -> Option<u32> {
    get_string_in(
        HKEY_LOCAL_MACHINE,
        r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
        Some("CurrentBuildNumber"),
    )?
    .trim()
    .parse()
    .ok()
}

/// The first installed Windows Terminal that has OpenConsole, and where.
fn installed_terminal() -> Option<(&'static TerminalPackage, PathBuf)> {
    TERMINAL_PACKAGES.iter().find_map(|package| {
        package_install_dir(package.family)
            .filter(|dir| dir.join(OPENCONSOLE_EXE).is_file())
            .map(|dir| (package, dir))
    })
}

/// True when the delegation terminal currently points at wmux.
pub fn is_enabled() -> bool {
    get_guid(STARTUP_KEY, TERMINAL_VALUE).is_some_and(|g| guid_eq(&g, &WMUX_TERMINAL_CLSID))
}

/// Whether handed-off programs are also shown through the previous terminal.
pub fn is_visible() -> bool {
    get_string(STATE_KEY, Some(VISIBLE)).as_deref() == Some("1")
}

/// The delegation terminal that was set before wmux took over.
pub fn previous_terminal() -> Option<GUID> {
    get_guid(STATE_KEY, PREVIOUS_TERMINAL)
}

/// The wmux.exe COM should start: the one running `enable`.
///
/// When that is winget's copy, register winget's link instead, because the
/// versioned package directory the exe really lives in changes on every
/// upgrade. Any other copy (a source build, an unzipped release) is used as
/// is, even if a winget install also exists.
fn stable_wmux_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("could not locate the wmux executable")?;
    let from_winget = exe.components().collect::<Vec<_>>().windows(2).any(|w| {
        w[0].as_os_str().eq_ignore_ascii_case("WinGet")
            && w[1].as_os_str().eq_ignore_ascii_case("Packages")
    });
    if from_winget {
        if let Some(base) = std::env::var_os("LOCALAPPDATA") {
            let link = Path::new(&base)
                .join("Microsoft")
                .join("WinGet")
                .join("Links")
                .join("wmux.exe");
            if link.is_file() {
                return Ok(link);
            }
        }
    }
    Ok(exe)
}

pub fn enable(visible: bool) -> Result<()> {
    // Older builds ignore the manifest's detached console policy, so COM
    // would start wmux with a console of its own, that console would be
    // handed off to wmux, and every console program would hang.
    match windows_build() {
        Some(build) if build >= MIN_BUILD => {}
        Some(build) => bail!(
            "wmux default-host needs Windows 11 24H2 (build {MIN_BUILD}) or later; \
             this is build {build}"
        ),
        None => bail!("could not read the Windows build number"),
    }
    let Some((package, dir)) = installed_terminal() else {
        bail!(
            "wmux default-host needs Windows Terminal or Windows Terminal Preview \
             installed: its OpenConsole is the console half of the handoff. \
             Install it with `winget install Microsoft.WindowsTerminal`"
        );
    };
    let wmux = stable_wmux_exe()?;

    // Save what was there, unless that is wmux already: enabling twice must
    // not make wmux its own "previous" terminal.
    if !is_enabled() {
        let zero = guid_string(&LET_WINDOWS_DECIDE);
        let console = get_string(STARTUP_KEY, Some(CONSOLE_VALUE)).unwrap_or(zero.clone());
        let terminal = get_string(STARTUP_KEY, Some(TERMINAL_VALUE)).unwrap_or(zero);
        set_string(STATE_KEY, Some(PREVIOUS_CONSOLE), &console)?;
        set_string(STATE_KEY, Some(PREVIOUS_TERMINAL), &terminal)?;
    }
    set_string(STATE_KEY, Some(VISIBLE), if visible { "1" } else { "0" })?;

    let wmux_key = class_key(&WMUX_TERMINAL_CLSID);
    set_string(&wmux_key, None, "wmux default terminal")?;
    set_string(
        &format!(r"{wmux_key}\LocalServer32"),
        None,
        &format!("\"{}\" com-server", wmux.display()),
    )?;

    // Last, so a failure above leaves Windows' default untouched.
    set_string(
        STARTUP_KEY,
        Some(CONSOLE_VALUE),
        &guid_string(&package.console),
    )?;
    set_string(
        STARTUP_KEY,
        Some(TERMINAL_VALUE),
        &guid_string(&WMUX_TERMINAL_CLSID),
    )?;

    println!(
        "wmux is now the default terminal{}.",
        if visible {
            ", showing each program through the previous one"
        } else {
            "; console programs run invisibly"
        }
    );
    println!(
        "  console: OpenConsole from {} ({})",
        package.name,
        dir.display()
    );
    println!(
        "List them with `wmux ls`; a program that fails stays there until you attach or kill it."
    );
    Ok(())
}

pub fn disable() -> Result<()> {
    let zero = guid_string(&LET_WINDOWS_DECIDE);
    if is_enabled() {
        let console = get_string(STATE_KEY, Some(PREVIOUS_CONSOLE)).unwrap_or(zero.clone());
        let terminal = get_string(STATE_KEY, Some(PREVIOUS_TERMINAL)).unwrap_or(zero);
        set_string(STARTUP_KEY, Some(CONSOLE_VALUE), &console)?;
        set_string(STARTUP_KEY, Some(TERMINAL_VALUE), &terminal)?;
    }
    delete_tree(&class_key(&WMUX_TERMINAL_CLSID));
    delete_tree(STATE_KEY);
    println!("wmux is no longer the default terminal; the previous setting is restored.");
    Ok(())
}

pub fn status() -> Result<()> {
    let show = |g: Option<GUID>| g.map_or("(unset)".to_string(), |g| guid_string(&g));
    if !is_enabled() {
        println!("wmux is not the default terminal.");
        println!(
            "  DelegationTerminal: {}",
            show(get_guid(STARTUP_KEY, TERMINAL_VALUE))
        );
        return Ok(());
    }
    println!(
        "wmux is the default terminal ({}).",
        if is_visible() { "visible" } else { "invisible" }
    );
    println!("  previous terminal: {}", show(previous_terminal()));
    let server = get_string(
        &format!(r"{}\LocalServer32", class_key(&WMUX_TERMINAL_CLSID)),
        None,
    );
    println!("  wmux:    {}", server.as_deref().unwrap_or("(missing)"));
    let console = get_guid(STARTUP_KEY, CONSOLE_VALUE);
    let package = console.and_then(|c| TERMINAL_PACKAGES.iter().find(|p| guid_eq(&p.console, &c)));
    match package {
        Some(p) => match package_install_dir(p.family) {
            Some(dir) => println!("  console: OpenConsole from {} ({})", p.name, dir.display()),
            None => println!(
                "  warning: {} is no longer installed, so handoffs fail; run enable again",
                p.name
            ),
        },
        None => println!(
            "  console: {} (not a Windows Terminal OpenConsole)",
            show(console)
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_package_family_that_is_not_installed_has_no_install_dir() {
        let missing_family = "wmux.NoSuchPackage_0000000000000";
        assert!(package_install_dir(missing_family).is_none());
    }

    #[test]
    fn the_windows_build_number_is_readable() {
        let first_windows_10_build = 10240;
        assert!(windows_build().is_some_and(|b| b >= first_windows_10_build));
    }

    #[test]
    fn each_terminal_package_has_its_own_openconsole() {
        let [preview, stable] = &TERMINAL_PACKAGES;
        assert!(!guid_eq(&preview.console, &stable.console));
    }
}
