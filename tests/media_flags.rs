//! End-to-end tests of `dl`'s media-patching flags, pinning the agreed behaviour table — which
//! actions each command performs against each on-disk state ("—" = touch nothing):
//!
//! | command                            | not downloaded     | video only          | video+subs | video+thumb | video+thumb+subs |
//! |------------------------------------|--------------------|---------------------|------------|-------------|------------------|
//! | `vidl <vid>`                         | dl vid + dl subs   | —                   | —          | —           | —                |
//! | `vidl <vid> --thumbnail`             | … + dl thumb       | dl thumb            | dl thumb   | —           | —                |
//! | `vidl <vid> --subtitles`             | dl vid + dl subs   | dl subs             | —          | dl subs     | —                |
//! | `vidl <vid> --thumbnail --subtitles` | … + dl thumb       | dl subs + dl thumb  | dl thumb   | dl subs     | —                |
//!
//! Subtitles ride every real download; thumbnails are opt-in; both flags are late, idempotent
//! patch passes that ignore the download archive (so a re-run fixes up already-downloaded files).
//!
//! These tests download from YouTube for real, driving whatever yt-dlp/ffmpeg the PATH offers,
//! so they are `#[ignore]`d out of the default suite. Run them with:
//!
//! ```text
//! cargo test --test media_flags -- --ignored --test-threads=1
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

/// The sample: 19 seconds, public, and carrying real `en`+`de` subtitle tracks — small enough to
/// download repeatedly, gated behind nothing (an age-restricted sample would tie the tests to
/// whatever cookies the running machine has imported), and subtitled (the table's subtitle
/// columns need tracks to exist).
const URL: &str = "https://www.youtube.com/watch?v=jNQXAC9IVRw";
const ID: &str = "jNQXAC9IVRw";

/// One test downloads at a time — parallel runs would hammer YouTube with the same video and
/// invite rate-limiting.
static SERIAL: Mutex<()> = Mutex::new(());

/// An empty unique temp directory for one state-walk.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vidl_media_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run `vidl <URL> --into <dir> <flags…>` through the real binary, asserting success; returns the
/// combined output (status lines carry ANSI colour, but the message text stays contiguous, so
/// plain `contains` matches). No cookie flag is ever passed: the sample is public, and this crate
/// sends nothing it wasn't handed — so there is no private session for a test to leak.
fn dl(dir: &Path, flags: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_vidl"))
        .args([URL, "--into"])
        .arg(dir)
        .args(flags)
        .output()
        .expect("run vidl");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "vidl {flags:?} failed:\n{text}");
    text
}

/// A bundled tool when the bundle exists (these tests assume it does — `dl` itself needs it),
/// else the bare PATH name.
fn tool(name: &str) -> PathBuf {
    let bundled = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".bashrs/tools/bin")
        .join(name);
    if bundled.exists() {
        bundled
    } else {
        PathBuf::from(name)
    }
}

/// The downloaded mkv (found by the `__<id>.` marker `dl` names files with).
fn video_file(dir: &Path) -> PathBuf {
    let marker = format!("__{ID}.");
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.contains(&marker) && name.ends_with(".mkv")
            })
        })
        .expect("the downloaded mkv on disk")
}

fn ffprobe(file: &Path, args: &[&str]) -> String {
    let out = Command::new(tool("ffprobe"))
        .args(["-v", "error"])
        .args(args)
        .arg(file)
        .output()
        .expect("run ffprobe");
    assert!(out.status.success(), "ffprobe failed on {}", file.display());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn subtitle_count(file: &Path) -> usize {
    ffprobe(file, &["-select_streams", "s", "-show_entries", "stream=index", "-of", "csv=p=0"])
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}

fn has_thumbnail(file: &Path) -> bool {
    ffprobe(file, &["-show_entries", "stream_disposition=attached_pic", "-of", "csv=p=0"])
        .lines()
        .any(|line| line.trim() == "1")
}

fn mtime(file: &Path) -> std::time::SystemTime {
    std::fs::metadata(file).unwrap().modified().unwrap()
}

/// Whether yt-dlp reported YouTube's caption endpoint refusing tracks (429s freely, datacenter
/// IPs especially). A live-environment condition, not a product failure: subtitle tracks are
/// must-try by design, so subtitle-content assertions degrade to a skip-with-notice while
/// everything else stays strict.
fn subs_refused(out: &str) -> bool {
    out.contains("Unable to download video subtitles")
}

/// Remux `file` in place down to its primary video + audio — dropping subtitles and the attached
/// cover — manufacturing the table's "video only" state from a fully-dressed file. (The other
/// intermediate states are reached through the flags themselves: `--thumbnail` on a bare video
/// yields "video+thumb", a plain download yields "video+subs".)
fn strip_to_bare(file: &Path) {
    let tmp = file.with_extension("stripped.mkv");
    let mut cmd = Command::new(tool("ffmpeg"));
    // see util::exec — concurrent ffmpegs race over the terminal's raw-mode settings
    cmd.stdin(std::process::Stdio::null());
    cmd.args(["-nostdin", "-v", "error", "-y", "-i"]).arg(file);
    cmd.args(["-map", "0:v:0", "-map", "0:a", "-c", "copy"]).arg(&tmp);
    assert!(cmd.status().expect("run ffmpeg").success(), "strip remux failed");
    std::fs::rename(&tmp, file).unwrap();
}

/// Walks a single downloaded file through every "already exists" column of the table, asserting
/// each command patches exactly what its cell says — and nothing else.
#[test]
#[ignore = "network: downloads from YouTube; needs the bundled yt-dlp/ffmpeg/deno"]
fn existing_video_states_get_patched_only_where_the_table_says_so() {
    let _serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = scratch("states");

    // not downloaded × `dl` → the video lands with subtitles embedded, and no thumbnail.
    let out = dl(&dir, &[]);
    let file = video_file(&dir);
    let subs_ok = !subs_refused(&out);
    if subs_ok {
        assert!(subtitle_count(&file) > 0, "a plain download embeds subtitles:\n{out}");
    } else {
        eprintln!("SKIPPED subtitle-content assertions: YouTube 429'd the caption endpoint this run");
    }
    assert!(!has_thumbnail(&file), "a plain download must not embed a thumbnail:\n{out}");

    // video+subs × `dl` → — (skipped outright, file untouched, no patch passes).
    let before = mtime(&file);
    let out = dl(&dir, &[]);
    assert!(out.contains("already downloaded — skipping"), "{out}");
    assert!(!out.contains("thumbnails:") && !out.contains("subtitles:"), "no passes without flags:\n{out}");
    assert_eq!(mtime(&file), before, "a plain re-run must not touch the file");

    // video+subs × `--thumbnail` → dl thumb (and only that — subtitles stay as they were).
    let subs = subtitle_count(&file);
    let out = dl(&dir, &["--thumbnail"]);
    assert!(out.contains("missing a thumbnail") && out.contains("embedded"), "{out}");
    assert!(has_thumbnail(&file), "thumbnail patched in:\n{out}");
    assert_eq!(subtitle_count(&file), subs, "the thumbnail pass must not touch subtitles");

    // video+thumb+subs × `--thumbnail` → — (idempotent: seen as present, not re-embedded).
    let before = mtime(&file);
    let out = dl(&dir, &["--thumbnail"]);
    assert!(out.contains("already has a thumbnail"), "{out}");
    assert_eq!(mtime(&file), before, "no re-embed when the thumbnail is already there");

    // video+thumb+subs × `--subtitles` → — (the forced scan finds every expected track present).
    // Only meaningful when the subs actually landed; under a caption 429 the state is video+thumb.
    if subs_ok {
        let before = mtime(&file);
        let out = dl(&dir, &["--subtitles"]);
        assert!(out.contains("already downloaded — scanning subtitles"), "{out}");
        assert!(out.contains("expected subtitle(s) already embedded"), "{out}");
        assert!(!out.contains("missing subtitle"), "{out}");
        assert_eq!(mtime(&file), before, "no re-mux when the subtitles are already there");
    }

    // video only × `--thumbnail` → dl thumb (reaching the "video+thumb" state through the flag).
    strip_to_bare(&file);
    assert_eq!(subtitle_count(&file), 0, "state manufactured: bare video");
    assert!(!has_thumbnail(&file), "state manufactured: bare video");
    let out = dl(&dir, &["--thumbnail"]);
    assert!(out.contains("missing a thumbnail") && out.contains("embedded"), "{out}");
    assert!(has_thumbnail(&file), "thumbnail patched onto the bare video:\n{out}");
    assert_eq!(subtitle_count(&file), 0, "the thumbnail pass must not add subtitles");

    // video+thumb × `--subtitles` → dl subs (and the mux must not drop the cover art).
    // The patch fetch hits the same throttled endpoint, so it too is gated on captions flowing.
    if subs_ok {
        let out = dl(&dir, &["--subtitles"]);
        assert!(out.contains("missing subtitle(s):") && out.contains("embedded"), "{out}");
        assert!(subtitle_count(&file) > 0, "subtitles patched back in:\n{out}");
        assert!(has_thumbnail(&file), "the subtitle mux must not drop the cover art");
    }

    // video only × `--thumbnail --subtitles` → dl subs + dl thumb.
    strip_to_bare(&file);
    let out = dl(&dir, &["--thumbnail", "--subtitles"]);
    assert!(out.contains("missing a thumbnail"), "{out}");
    assert!(out.contains("missing subtitle(s):"), "the probe still names the expected tracks: {out}");
    assert!(has_thumbnail(&file), "the thumbnail half patched:\n{out}");
    if subs_ok {
        assert!(subtitle_count(&file) > 0, "the subtitle half patched:\n{out}");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// The "not downloaded" column with both flags: one run lands the video with subtitles (which
/// ride the download itself — fetched once, so the forced scan then finds nothing missing) and
/// the thumbnail (which never rides a download — its pass fetches it).
#[test]
#[ignore = "network: downloads from YouTube; needs the bundled yt-dlp/ffmpeg/deno"]
fn a_fresh_download_with_both_flags_lands_video_subs_and_thumbnail_in_one_run() {
    let _serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = scratch("fresh_both");

    let out = dl(&dir, &["--thumbnail", "--subtitles"]);
    let file = video_file(&dir);
    assert!(has_thumbnail(&file), "the thumbnail pass ran after the download:\n{out}");
    assert!(out.contains("missing a thumbnail"), "the thumbnail is never part of the download:\n{out}");
    if subs_refused(&out) {
        eprintln!("SKIPPED subtitle-content assertions: YouTube 429'd the caption endpoint this run");
    } else {
        assert!(subtitle_count(&file) > 0, "subtitles rode the download:\n{out}");
        // Subtitles arrived inline, so the pass reports them present rather than fetching twice.
        assert!(out.contains("expected subtitle(s) already embedded"), "subs fetched once, not twice:\n{out}");
    }

    let _ = std::fs::remove_dir_all(&dir);
}
