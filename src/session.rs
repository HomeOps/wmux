//! Session naming, pipe path construction, and discovery.
//!
//! Every session is reachable at a named pipe whose name embeds the SID of the
//! user who created it. Including the SID means two users on the same machine
//! can both have a session called `build` without colliding, and it makes the
//! `ls` listing naturally scoped to the current user.

use anyhow::{bail, Context, Result};

/// Prefix shared by every wmux pipe, before the SID.
const PIPE_PREFIX: &str = "wmux";

/// Directory that Windows exposes the named pipe filesystem under.
const PIPE_DIR: &str = r"\\.\pipe\";

/// Longest session name we accept. Pipe names may be up to 256 characters
/// total and the SID eats a chunk of that, so we stay well clear.
pub const MAX_NAME_LEN: usize = 64;

/// Rejects session names that would be ambiguous in a pipe path or awkward on
/// a command line.
///
/// We allow ASCII alphanumerics plus `-` and `_`. Notably `.` is excluded
/// because it separates the fields of the pipe name.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("session name must not be empty");
    }
    if name.len() > MAX_NAME_LEN {
        bail!(
            "session name is {} characters; the maximum is {MAX_NAME_LEN}",
            name.len()
        );
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'))
    {
        bail!("session name contains {bad:?}; only letters, digits, '-' and '_' are allowed");
    }
    Ok(())
}

/// Builds the full pipe path for a session belonging to the given user SID.
pub fn pipe_path_for(sid: &str, name: &str) -> String {
    format!("{PIPE_DIR}{}", pipe_leaf_for(sid, name))
}

/// Builds the full pipe path for a session belonging to the current user.
pub fn pipe_path(name: &str) -> Result<String> {
    validate_name(name)?;
    Ok(pipe_path_for(&current_user_sid()?, name))
}

/// The pipe *name* (no `\\.\pipe\` prefix) that a session occupies.
pub fn pipe_leaf_for(sid: &str, name: &str) -> String {
    format!("{PIPE_PREFIX}.{sid}.{name}")
}

/// Recovers a session name from a pipe leaf name, if it belongs to `sid`.
pub fn session_name_from_leaf(sid: &str, leaf: &str) -> Option<String> {
    let prefix = format!("{PIPE_PREFIX}.{sid}.");
    leaf.strip_prefix(&prefix)
        .filter(|rest| !rest.is_empty() && validate_name(rest).is_ok())
        .map(|rest| rest.to_string())
}

/// Lists the names of every live session owned by the current user.
///
/// Windows exposes named pipes as a directory listing, so discovery is a
/// filtered `read_dir`. A pipe only exists while its server process holds it
/// open, which makes this listing self-cleaning: if a session server dies, its
/// pipe disappears and it stops showing up here.
pub fn list_sessions() -> Result<Vec<String>> {
    let sid = current_user_sid()?;
    let entries = std::fs::read_dir(PIPE_DIR)
        .with_context(|| format!("failed to enumerate named pipes at {PIPE_DIR}"))?;

    let mut names = Vec::new();
    for entry in entries.flatten() {
        let leaf = entry.file_name();
        let leaf = match leaf.to_str() {
            Some(s) => s,
            // Pipe names are almost always ASCII; skip anything that is not
            // valid UTF-16-to-UTF-8 rather than failing the whole listing.
            None => continue,
        };
        if let Some(name) = session_name_from_leaf(&sid, leaf) {
            names.push(name);
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// Returns true if a session with this name is currently live.
///
/// Deliberately implemented as a directory listing rather than a
/// `Path::exists` on the pipe path. Probing a named pipe with the ordinary
/// filesystem APIs opens a *client* connection to it, which consumes the
/// instance the server is waiting on and breaks the session. Enumerating the
/// pipe directory is a read-only operation with no such side effect.
pub fn session_exists(name: &str) -> Result<bool> {
    validate_name(name)?;
    Ok(list_sessions()?.iter().any(|n| n == name))
}

/// Environment variable the server sets inside every session, naming it.
///
/// Anything running in a session inherits this, which is how commands like
/// `wmux detach` can tell which session they are inside without being told.
pub const SESSION_ENV: &str = "WMUX_SESSION";

/// Resolves which session a command should act on.
///
/// An explicit name always wins. Otherwise fall back to the session the caller
/// is running inside, and fail with something actionable if it is neither.
pub fn resolve_target(explicit: Option<String>, from_env: Option<String>) -> Result<String> {
    if let Some(name) = explicit {
        validate_name(&name)?;
        return Ok(name);
    }
    match from_env {
        Some(name) if !name.trim().is_empty() => {
            let name = name.trim().to_string();
            validate_name(&name)?;
            Ok(name)
        }
        _ => bail!(
            "not running inside a wmux session, so there is nothing to detach.\n\
             Name a session explicitly, for example `wmux detach build`, or run \
             `wmux ls` to see what is running."
        ),
    }
}

/// Generates the next free `wmux-N` style name.
pub fn next_free_name() -> Result<String> {
    let existing = list_sessions().unwrap_or_default();
    for n in 0.. {
        let candidate = format!("wmux-{n}");
        if !existing.contains(&candidate) {
            return Ok(candidate);
        }
    }
    unreachable!("the loop above returns for some n")
}

/// The string form of the current process token's user SID, e.g. `S-1-5-21-...`.
#[cfg(windows)]
pub fn current_user_sid() -> Result<String> {
    use std::ptr;
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            bail!(
                "OpenProcessToken failed: {}",
                std::io::Error::last_os_error()
            );
        }

        // First call sizes the buffer, second call fills it.
        let mut needed: u32 = 0;
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed);
        let mut buf = vec![0u8; needed as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        );
        CloseHandle(token);
        if ok == 0 {
            bail!(
                "GetTokenInformation(TokenUser) failed: {}",
                std::io::Error::last_os_error()
            );
        }

        let token_user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut wide: *mut u16 = ptr::null_mut();
        if ConvertSidToStringSidW(token_user.User.Sid, &mut wide) == 0 {
            bail!(
                "ConvertSidToStringSidW failed: {}",
                std::io::Error::last_os_error()
            );
        }

        let mut len = 0usize;
        while *wide.add(len) != 0 {
            len += 1;
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(wide, len));
        LocalFree(wide.cast());
        Ok(sid)
    }
}

#[cfg(not(windows))]
pub fn current_user_sid() -> Result<String> {
    bail!("wmux only runs on Windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "S-1-5-21-1111111111-2222222222-3333333333-1001";

    #[test]
    fn ordinary_names_are_accepted() {
        for name in ["build", "a", "my-session_2", "WMUX9"] {
            validate_name(name).unwrap_or_else(|e| panic!("{name} should be valid: {e}"));
        }
    }

    #[test]
    fn empty_name_is_rejected() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn overlong_name_is_rejected() {
        let name = "a".repeat(MAX_NAME_LEN + 1);
        assert!(validate_name(&name).is_err());
        assert!(validate_name(&"a".repeat(MAX_NAME_LEN)).is_ok());
    }

    #[test]
    fn path_separators_and_dots_are_rejected() {
        // A name that escaped validation could redirect the pipe path.
        for name in [r"..\..\evil", "a/b", "a.b", "a b", "a\\b", "a\0b"] {
            assert!(validate_name(name).is_err(), "{name} should be rejected");
        }
    }

    #[test]
    fn pipe_path_embeds_sid_and_name() {
        assert_eq!(
            pipe_path_for(SID, "build"),
            format!(r"\\.\pipe\wmux.{SID}.build")
        );
    }

    #[test]
    fn leaf_roundtrips_through_session_name() {
        let leaf = pipe_leaf_for(SID, "build");
        assert_eq!(session_name_from_leaf(SID, &leaf).as_deref(), Some("build"));
    }

    #[test]
    fn leaf_from_another_user_is_ignored() {
        let leaf = pipe_leaf_for("S-1-5-21-9-9-9-500", "build");
        assert_eq!(session_name_from_leaf(SID, &leaf), None);
    }

    #[test]
    fn an_explicit_name_wins_over_the_environment() {
        let got = resolve_target(Some("chosen".into()), Some("ambient".into())).unwrap();
        assert_eq!(got, "chosen");
    }

    #[test]
    fn the_ambient_session_is_used_when_no_name_is_given() {
        let got = resolve_target(None, Some("ambient".into())).unwrap();
        assert_eq!(got, "ambient");
    }

    #[test]
    fn surrounding_whitespace_in_the_environment_is_ignored() {
        let got = resolve_target(None, Some("  ambient \n".into())).unwrap();
        assert_eq!(got, "ambient");
    }

    #[test]
    fn outside_a_session_with_no_name_is_an_actionable_error() {
        for env in [None, Some(String::new()), Some("   ".into())] {
            let err = resolve_target(None, env).unwrap_err().to_string();
            assert!(
                err.contains("not running inside a wmux session"),
                "unhelpful error: {err}"
            );
            assert!(err.contains("wmux ls"), "should point somewhere: {err}");
        }
    }

    #[test]
    fn a_bogus_ambient_session_name_is_rejected() {
        assert!(resolve_target(None, Some("../evil".into())).is_err());
    }

    #[test]
    fn unrelated_pipes_are_ignored() {
        for leaf in ["chrome.sync", "wmux", "wmuxfoo", "wmux..build"] {
            assert_eq!(session_name_from_leaf(SID, leaf), None, "{leaf}");
        }
    }
}
