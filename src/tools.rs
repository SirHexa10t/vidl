//! Where the external programs live.
//!
//! `vidl` shells out to yt-dlp (and, for a zipapp install, the python that runs it). By default it
//! finds them the way any tool does — by bare name, on `PATH` — which is all a standalone user
//! needs. An embedder that manages its own pinned copies — because yt-dlp breaks often enough that
//! pinning is the point — calls [`install`] first to say where they are.
//!
//! These are *installation* paths, fixed for the life of the process — the same shape as
//! `std::env::args()` — so they live in a `OnceLock` rather than being threaded through the
//! twelve call sites that build a yt-dlp command line. The tradeoff, stated plainly: one process
//! cannot drive two different yt-dlp installations. For a download tool that isn't a real case,
//! and the alternative distorted every signature in the crate to serve a hypothetical.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The programs this crate runs. `None` means "look on `PATH` by the usual name".
#[derive(Clone, Default)]
pub struct Tools {
    /// The yt-dlp binary, or the zipapp to hand to [`Tools::python`].
    pub ytdlp: Option<OsString>,
    /// The interpreter to run a yt-dlp *zipapp* through. A yt-dlp that is a real binary ignores
    /// this; a zipapp's `env python3` shebang would otherwise pick up whatever the caller's PATH
    /// offers, which is how the wrong interpreter (without curl_cffi) gets used.
    pub python: Option<OsString>,
    /// The directory holding `ffmpeg` and `ffprobe` — the muxing and inspection behind every
    /// thumbnail and subtitle pass.
    pub ffmpeg_dir: Option<PathBuf>,
    /// A JavaScript runtime (deno). yt-dlp must execute YouTube's obfuscated player JS to work
    /// out a media URL's signature; without one, YouTube downloads lose formats or throttle.
    pub js_runtime: Option<PathBuf>,
}

static TOOLS: OnceLock<Tools> = OnceLock::new();

/// Declare where the tools are. The first call wins; later ones are ignored, so an embedder sets
/// this once at startup and nothing downstream can move the ground under a run in progress.
pub fn install(tools: Tools) {
    let _ = TOOLS.set(tools);
}

/// The configured tools, or PATH defaults when [`install`] was never called.
fn configured() -> &'static Tools {
    TOOLS.get_or_init(Tools::default)
}

/// The yt-dlp to run, and the interpreter to run it through when it's a zipapp.
pub(crate) fn ytdlp() -> (OsString, Option<OsString>) {
    let tools = configured();
    let ytdlp = tools.ytdlp.clone().unwrap_or_else(|| OsString::from("yt-dlp"));
    // Only a zipapp needs an explicit interpreter; a plain binary runs itself. Treated as a
    // zipapp when an embedder named a python AND the yt-dlp path isn't the bare PATH name.
    let python = tools.python.clone().filter(|_| tools.ytdlp.is_some());
    (ytdlp, python)
}

/// The `ffmpeg` to run — inside a bundled ffmpeg's `bin/` when an embedder named one, otherwise
/// the bare name for PATH lookup.
pub(crate) fn ffmpeg() -> OsString {
    in_dir(configured().ffmpeg_dir.as_deref(), "ffmpeg")
}

/// The `ffprobe` beside [`ffmpeg`] — the two always ship together, so one directory names both.
pub(crate) fn ffprobe() -> OsString {
    in_dir(configured().ffmpeg_dir.as_deref(), "ffprobe")
}

/// The JS runtime for yt-dlp's YouTube extractor, when an embedder bundled one.
pub(crate) fn js_runtime() -> Option<&'static Path> {
    configured().js_runtime.as_deref()
}

/// The bundled ffmpeg's directory, for handing to yt-dlp's `--ffmpeg-location`. `None` leaves
/// yt-dlp to its own PATH search.
pub(crate) fn ffmpeg_dir() -> Option<&'static Path> {
    configured().ffmpeg_dir.as_deref()
}

fn in_dir(dir: Option<&Path>, name: &str) -> OsString {
    dir.map(|dir| dir.join(name).into_os_string()).unwrap_or_else(|| OsString::from(name))
}
