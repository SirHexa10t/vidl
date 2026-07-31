//! Running external programs, with the output and status handling this crate needs.
//!
//! A deliberately small process runner — a few dozen lines of `std::process` rather
//! than a dependency on it: three functions is less than a shared crate would cost either project,
//! and it keeps `vidl` answerable to nothing but the standard library.

use std::ffi::OsStr;
use std::process::{Command, Stdio};

/// Every child this crate spawns gets a **null stdin**, and this is the reason.
///
/// ffmpeg reads stdin for interactive keys (`q` to quit) and briefly puts the terminal into raw
/// mode — `ECHO`, `ICANON`, `ICRNL`, `IEXTEN` and `IXON` off — restoring the settings it saved
/// when it exits. With one ffmpeg that is invisible. With several overlapping, the save/restore
/// interleaves: the second saves the *already-raw* state and restores that, and the terminal is
/// left with no echo, so the user's shell types blind until they run `stty sane`.
///
/// Nothing here needs to read from the user, so a null stdin costs nothing and removes the race
/// at the root — including for programs we launch that spawn their OWN ffmpeg (yt-dlp merges
/// streams that way), since a child inherits it. Direct ffmpeg calls pass `-nostdin` as well, to
/// say the same thing where it can be seen in a command line. Do not add `-nostdin` to *ffprobe*:
/// it rejects the option and exits 1.
fn spawnable(program: &OsStr) -> Command {
    let mut cmd = Command::new(program);
    cmd.stdin(Stdio::null());
    cmd
}

/// Run `program`, letting its output through to the user, and return its exit code (`-1` if it
/// couldn't be spawned at all). The code matters: yt-dlp's tells success from a bot-wall.
pub(crate) fn run_reporting_code<P, I, S>(program: P, args: I) -> i32
where
    P: AsRef<OsStr>,
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let program = program.as_ref();
    match spawnable(program).args(args).status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(err) => {
            eprintln!("could not run {}: {err}", program.to_string_lossy());
            -1
        }
    }
}

/// Run `program` and say only whether it succeeded — for the media passes, where a failure means
/// "leave the file alone" and the exit code carries nothing else. Output passes through.
pub(crate) fn run_ok<P, I, S>(program: P, args: I) -> bool
where
    P: AsRef<OsStr>,
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    matches!(spawnable(program.as_ref()).args(args).status(), Ok(status) if status.success())
}

/// Run `program`, capturing stdout for parsing; its stderr passes through to the user.
/// `None` on a non-zero exit or a spawn failure.
pub(crate) fn capture_stdout<P, I, S>(program: P, args: I) -> Option<String>
where
    P: AsRef<OsStr>,
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let program = program.as_ref();
    match spawnable(program).args(args).output() {
        Ok(out) => {
            if !out.stderr.is_empty() {
                eprint!("{}", String::from_utf8_lossy(&out.stderr));
            }
            out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
        }
        Err(err) => {
            eprintln!("could not run {}: {err}", program.to_string_lossy());
            None
        }
    }
}

/// Run `program`, capturing BOTH streams — for callers that must read the failure text before
/// deciding whether the user should see it (telling "no such tab" from a real network error).
/// `None` only on a spawn failure.
pub(crate) fn capture_output<P, I, S>(program: P, args: I) -> Option<(bool, String, String)>
where
    P: AsRef<OsStr>,
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let program = program.as_ref();
    match spawnable(program).args(args).output() {
        Ok(out) => Some((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )),
        Err(err) => {
            eprintln!("could not run {}: {err}", program.to_string_lossy());
            None
        }
    }
}
