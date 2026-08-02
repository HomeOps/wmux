//! Structured command execution inside a PowerShell session.
//!
//! `wmux send` types keystrokes; `wmux capture` scrapes the rendered screen.
//! Neither is a good way to get a *result* out of PowerShell:
//!
//! * the screen is wrapped to the terminal width, so a long value arrives with
//!   newlines injected at arbitrary points;
//! * anything that scrolls off the visible screen is simply gone;
//! * everything is text, so the object pipeline is flattened to whatever
//!   `Out-String` decided it should look like;
//! * a terminal echoes what you type, so output and input are interleaved on
//!   the same surface with no reliable way to tell them apart.
//!
//! So `run` does not use the screen at all. It types a wrapper that serialises
//! the pipeline to a temporary file and then touches a second file to signal
//! completion. wmux polls for the completion marker and reads the payload off
//! disk. The terminal is only ever the transport for the *command*, never for
//! the result.
//!
//! This is the one PowerShell-specific part of wmux. A session running
//! something else can still use `send` and `capture`.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::client;

/// How the pipeline is serialised on the session side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// PowerShell's own serialisation. Round-trips types through
    /// `Import-Clixml`, so the caller gets real objects back.
    Clixml,
    /// Compact JSON. Lossier than CLIXML but readable by anything.
    Json,
    /// `Out-String` output, for when you just want what it looked like.
    Text,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Clixml => "clixml",
            Format::Json => "json",
            Format::Text => "text",
        }
    }
}

impl std::str::FromStr for Format {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "clixml" | "xml" => Ok(Format::Clixml),
            "json" => Ok(Format::Json),
            "text" | "string" => Ok(Format::Text),
            other => Err(format!(
                "unknown format {other:?}; expected clixml, json, or text"
            )),
        }
    }
}

/// Escapes a string for embedding in a PowerShell single-quoted literal.
///
/// Single-quoted strings in PowerShell have exactly one escape: a doubled
/// quote. No backslash processing, no interpolation.
pub fn ps_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Builds the one-line wrapper that is typed into the session.
///
/// Everything is on a single line because it is delivered as keystrokes, and a
/// newline would submit the command early.
pub fn wrapper(command: &str, out: &Path, done: &Path, format: Format, depth: u32) -> String {
    let out_lit = ps_single_quote(&out.to_string_lossy());
    let done_lit = ps_single_quote(&done.to_string_lossy());

    // `$__wmuxE` is left undefined on success, which is what Success tests.
    // `@(...)` forces array semantics so a single result and a collection
    // serialise the same shape.
    let capture = format!(
        "$__wmuxE=$null;try{{$__wmuxO=@({command})}}catch{{$__wmuxO=$null;$__wmuxE=$_.ToString()}};\
         $__wmuxR=[pscustomobject]@{{Success=($null -eq $__wmuxE);Error=$__wmuxE;\
         ExitCode=$LASTEXITCODE;Output=$__wmuxO}};"
    );

    let serialise = match format {
        Format::Clixml => format!("$__wmuxR|Export-Clixml -LiteralPath {out_lit} -Depth {depth};"),
        Format::Json => format!(
            "$__wmuxR|ConvertTo-Json -Depth {depth} -Compress|\
             Set-Content -LiteralPath {out_lit} -Encoding utf8;"
        ),
        Format::Text => format!(
            "($__wmuxR.Output|Out-String)|Set-Content -LiteralPath {out_lit} -Encoding utf8;"
        ),
    };

    // The completion marker is written last and separately, so wmux never
    // reads a payload that is still being flushed.
    let finish = format!(
        "New-Item -ItemType File -Path {done_lit} -Force|Out-Null;\
         Remove-Variable __wmuxO,__wmuxE,__wmuxR -ErrorAction SilentlyContinue"
    );

    format!("{capture}{serialise}{finish}")
}

/// Generates a collision-resistant identifier without pulling in a rand crate.
fn unique_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos:x}-{seq:x}", std::process::id())
}

/// Default serialisation depth.
///
/// Matches `Export-Clixml` and `ConvertTo-Json`, and for the same reason: rich
/// .NET objects have recursive graphs. `FileInfo.Directory` leads to
/// `DirectoryInfo.Parent` and onward, so a plain `dir` serialised at depth 8
/// produced 688 MB of CLIXML and appeared to hang the session. Raise it
/// deliberately with `--depth`, never by default.
pub const DEFAULT_DEPTH: u32 = 2;

/// Age at which an orphaned artifact is considered abandoned and swept.
const STALE_ARTIFACT_AGE: Duration = Duration::from_secs(60 * 60);

/// Where a run's payload and completion marker live.
struct Artifacts {
    out: PathBuf,
    done: PathBuf,
}

impl Artifacts {
    fn new() -> Artifacts {
        let dir = std::env::temp_dir();
        let id = unique_id();
        Artifacts {
            out: dir.join(format!("wmux-run-{id}.out")),
            done: dir.join(format!("wmux-run-{id}.done")),
        }
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.out);
        let _ = std::fs::remove_file(&self.done);
    }
}

/// Runs `command` in the named session and returns its serialised result.
pub fn run(
    session_name: &str,
    command: &str,
    format: Format,
    depth: u32,
    timeout: Duration,
) -> Result<String> {
    if command.trim().is_empty() {
        bail!("nothing to run");
    }
    if command.contains(['\r', '\n']) {
        bail!("the command must be a single line; it is delivered as keystrokes");
    }

    // A run that times out leaves its files to be written later by a session
    // that is still working, so they cannot be cleaned up at the time. Sweep
    // abandoned ones on the next run instead.
    sweep_stale_artifacts();

    let artifacts = Artifacts::new();
    let line = wrapper(command, &artifacts.out, &artifacts.done, format, depth);

    client::send_keys(session_name, &format!("{line}\r"))
        .context("failed to deliver the command to the session")?;

    let result = wait_for_result(&artifacts, timeout);
    artifacts.cleanup();
    result
}

fn wait_for_result(artifacts: &Artifacts, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if artifacts.done.exists() {
            let bytes = std::fs::read(&artifacts.out).with_context(|| {
                format!(
                    "the session signalled completion but {} could not be read",
                    artifacts.out.display()
                )
            })?;
            return Ok(strip_bom(&String::from_utf8_lossy(&bytes)).to_string());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!(
        "the session did not finish within {timeout:?}.\n\
         It may still be busy, waiting at a prompt, or serialising a very large \
         object graph. Check with `wmux capture`, and press Ctrl-C in the session \
         if it is stuck.\n\
         If the command returns rich objects such as files or processes, pipe it \
         through `Select-Object` to pick the properties you need rather than \
         raising --depth: deep graphs serialise into gigabytes."
    )
}

/// Removes run artifacts left behind by earlier timed-out runs.
fn sweep_stale_artifacts() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("wmux-run-") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| {
                t.elapsed()
                    .map(|age| age > STALE_ARTIFACT_AGE)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// PowerShell may prefix a UTF-8 BOM depending on version and cmdlet.
fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_strings_are_wrapped_in_single_quotes() {
        assert_eq!(ps_single_quote(r"C:\Temp\a.out"), r"'C:\Temp\a.out'");
    }

    #[test]
    fn embedded_quotes_are_doubled() {
        assert_eq!(ps_single_quote("it's"), "'it''s'");
        // A path crafted to break out of the literal stays inside it.
        assert_eq!(
            ps_single_quote("';Remove-Item C:\\ -Recurse;'"),
            "''';Remove-Item C:\\ -Recurse;'''"
        );
    }

    #[test]
    fn backslashes_are_not_escapes_in_powershell_literals() {
        assert_eq!(ps_single_quote(r"a\b"), r"'a\b'");
    }

    #[test]
    fn formats_parse_from_their_names() {
        use std::str::FromStr;
        assert_eq!(Format::from_str("json").unwrap(), Format::Json);
        assert_eq!(Format::from_str("CLIXML").unwrap(), Format::Clixml);
        assert_eq!(Format::from_str("xml").unwrap(), Format::Clixml);
        assert_eq!(Format::from_str("text").unwrap(), Format::Text);
        assert!(Format::from_str("yaml").is_err());
    }

    #[test]
    fn the_wrapper_is_a_single_line() {
        let line = wrapper(
            "Get-Process",
            Path::new(r"C:\Temp\o"),
            Path::new(r"C:\Temp\d"),
            Format::Json,
            4,
        );
        assert!(
            !line.contains('\n') && !line.contains('\r'),
            "a newline would submit the command early: {line}"
        );
    }

    #[test]
    fn the_wrapper_contains_the_command_and_both_paths() {
        let line = wrapper(
            "1..3",
            Path::new(r"C:\Temp\o"),
            Path::new(r"C:\Temp\d"),
            Format::Clixml,
            2,
        );
        assert!(line.contains("1..3"));
        assert!(line.contains(r"'C:\Temp\o'"));
        assert!(line.contains(r"'C:\Temp\d'"));
        assert!(line.contains("Export-Clixml"));
    }

    #[test]
    fn each_format_selects_its_own_serialiser() {
        let paths = (Path::new("o"), Path::new("d"));
        let json = wrapper("x", paths.0, paths.1, Format::Json, 3);
        assert!(json.contains("ConvertTo-Json") && json.contains("-Depth 3"));

        let text = wrapper("x", paths.0, paths.1, Format::Text, 3);
        assert!(text.contains("Out-String") && !text.contains("ConvertTo-Json"));
    }

    #[test]
    fn the_completion_marker_is_written_after_the_payload() {
        let line = wrapper(
            "x",
            Path::new("payload"),
            Path::new("marker"),
            Format::Json,
            2,
        );
        let payload_at = line.find("'payload'").expect("payload path");
        let marker_at = line.find("'marker'").expect("marker path");
        assert!(
            payload_at < marker_at,
            "reading the payload before it is flushed would be a race"
        );
    }

    #[test]
    fn the_default_depth_matches_powershell() {
        // Not a style choice. `dir` serialised at depth 8 produced 688 MB of
        // CLIXML and looked like a hung session, because FileInfo.Directory
        // leads to DirectoryInfo.Parent and onward. Export-Clixml defaults to
        // 2 for the same reason.
        assert_eq!(DEFAULT_DEPTH, 2);
    }

    #[test]
    fn the_wrapper_uses_the_depth_it_is_given() {
        let json = wrapper("x", Path::new("o"), Path::new("d"), Format::Json, 2);
        assert!(json.contains("-Depth 2"));
        let clixml = wrapper("x", Path::new("o"), Path::new("d"), Format::Clixml, 5);
        assert!(clixml.contains("-Depth 5"));
    }

    #[test]
    fn identifiers_do_not_repeat() {
        let a = unique_id();
        let b = unique_id();
        assert_ne!(a, b);
    }

    #[test]
    fn a_bom_is_stripped() {
        assert_eq!(strip_bom("\u{feff}{}"), "{}");
        assert_eq!(strip_bom("{}"), "{}");
    }
}
