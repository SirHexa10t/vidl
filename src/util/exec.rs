//! Running external programs, with the output and status handling this crate needs.
//!
//! A deliberately small process runner — a few dozen lines of `std::process` rather
//! than a dependency on it: three functions is less than a shared crate would cost either project,
//! and it keeps `vidl` answerable to nothing but the standard library.

use std::ffi::OsStr;
use std::io::{Read, Write};
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

/// [`capture_stdout`], but run from `dir`.
///
/// For the one invocation whose *side effect* is written relative to the working directory rather
/// than to a path we chose: `yt-dlp --write-pages` drops what it fetched beside the caller, so it
/// gets a directory of its own instead of littering wherever `vidl` was started from.
pub(crate) fn capture_stdout_in<P, I, S>(dir: &std::path::Path, program: P, args: I) -> Option<String>
where
    P: AsRef<OsStr>,
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let program = program.as_ref();
    match spawnable(program).current_dir(dir).args(args).output() {
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

/// How many times to ask for something before believing the answer is no.
///
/// Three, because the *first* request from a client address the far end hasn't seen can be refused
/// outright — a CDN's bot management commonly answers a cold address with a 403 and lets the next
/// request through. Seen twice over: a page scan refused while the download that followed it, to the
/// same URL, succeeded; and the same page refusing every request early in a session and answering
/// every one later, from an unchanged address. Reproducible by connecting and disconnecting a VPN,
/// which is what gives the game away — the refusal is keyed to the address, not to the run.
pub(crate) const COLD_ATTEMPTS: usize = 3;

/// Ask for something up to `attempts` times, saying so when a further go is needed.
///
/// For the requests a cold client loses: one refusal must not be taken for an answer. It says what
/// it is doing, because a *silent* retry is worse than none — a run that stalls twice with no
/// explanation looks broken. `what` names the thing being asked for and appears in the notices; what
/// to do when the answer never comes is left to the caller, since that differs by caller.
///
/// No pause between attempts. The evidence that an immediate retry works is that the download which
/// followed a refused scan, moments later, was one. A pause would only help against a rate limit,
/// and a 403 is not one.
pub(crate) fn ask_repeatedly<T>(
    what: &str,
    attempts: usize,
    mut ask: impl FnMut() -> Option<T>,
) -> Option<T> {
    for attempt in 1..=attempts {
        if let Some(answer) = ask() {
            if attempt > 1 {
                println!("{what} came through on attempt {attempt}");
            }
            return Some(answer);
        }
        if attempt < attempts {
            eprintln!(
                "vidl: could not read {what} (attempt {attempt} of {attempts}) — asking again"
            );
        }
    }
    None
}

/// Run `program`, letting BOTH streams through to the user as they arrive *and* capturing them.
///
/// For the retries. [`capture_output`] hides everything until the child exits, which is right for a
/// probe and wrong for a retry that downloads a whole video: minutes of silence, indistinguishable
/// from a hang, and the lines saying what is being tried arrive far too late to be read as progress.
///
/// Raw bytes rather than lines, because yt-dlp draws its progress with carriage returns *inside* one
/// line — a line-buffered echo would hold the entire download back and then dump it at the end,
/// which is the problem this is here to solve. The bytes are decoded once at the end instead of per
/// read, so a multi-byte character split across two reads is still decoded whole.
pub(crate) fn capture_echoing<P, I, S>(program: P, args: I) -> Option<(bool, String, String)>
where
    P: AsRef<OsStr>,
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let program = program.as_ref();
    let spawned = spawnable(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(err) => {
            eprintln!("could not run {}: {err}", program.to_string_lossy());
            return None;
        }
    };
    // stdout gets a thread of its own: draining one pipe to completion before starting on the other
    // deadlocks the moment the child fills the one nobody is reading.
    let echoing = child
        .stdout
        .take()
        .map(|stream| std::thread::spawn(move || drain(stream, std::io::stdout())));
    let errors = child.stderr.take().map(|stream| drain(stream, std::io::stderr()));
    let output = echoing.and_then(|thread| thread.join().ok()).unwrap_or_default();
    let status = child.wait().ok()?;
    Some((
        status.success(),
        String::from_utf8_lossy(&output).into_owned(),
        String::from_utf8_lossy(&errors.unwrap_or_default()).into_owned(),
    ))
}

/// Copy `source` to `sink` as it arrives, keeping a copy of everything that passed through.
///
/// Flushed per read, which is what puts the child's output on the terminal in step with the child
/// rather than in one burst when it exits. A read error ends the copy rather than failing the run:
/// whatever arrived is still worth returning, and the exit status is the real verdict.
fn drain(mut source: impl Read, mut sink: impl Write) -> Vec<u8> {
    let mut everything = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        match source.read(&mut buffer) {
            Ok(0) | Err(_) => return everything,
            Ok(read) => {
                let _ = sink.write_all(&buffer[..read]);
                let _ = sink.flush();
                everything.extend_from_slice(&buffer[..read]);
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn asking_repeatedly_stops_at_the_first_answer() {
        let asks = Cell::new(0);
        let answer = ask_repeatedly("the thing", 3, || {
            asks.set(asks.get() + 1);
            (asks.get() == 2).then_some("here")
        });
        assert_eq!(answer, Some("here"));
        assert_eq!(asks.get(), 2, "the third ask is not made once the second answers");
    }

    #[test]
    fn asking_repeatedly_gives_up_after_the_allowance_and_says_nothing_of_its_own() {
        let asks = Cell::new(0);
        let answer = ask_repeatedly("the thing", COLD_ATTEMPTS, || {
            asks.set(asks.get() + 1);
            None::<&str>
        });
        assert_eq!(answer, None, "no answer is not an answer");
        assert_eq!(asks.get(), COLD_ATTEMPTS, "asked its allowance, and not once more");
    }

    #[test]
    fn one_attempt_is_a_plain_call() {
        // The degenerate allowance has to behave, since a caller may compute it.
        let asks = Cell::new(0);
        assert_eq!(ask_repeatedly("x", 1, || { asks.set(asks.get() + 1); None::<u8> }), None);
        assert_eq!(asks.get(), 1);
        // And a nonsensical one must not loop forever or panic.
        assert_eq!(ask_repeatedly("x", 0, || Some(1u8)), None, "no attempts means no answer");
    }
}
