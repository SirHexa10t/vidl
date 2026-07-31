//! The EXTENDED live category: real YouTube and very short videos. These verify the contracts
//! the stubbed suite freezes in time — yt-dlp's real scan output, the CDN's thumbnail
//! convention — plus one gated download through a real session.
//!
//! Run them via TEST.sh, or directly:
//!
//! ```text
//! cargo test --test live_extended -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! House rules, because these touch live services and real credentials:
//! - Everything is serialized (a mutex, belt-and-braces with `--test-threads=1`) and each
//!   network test opens with a courtesy pause — a burst of requests is how IPs get flagged.
//! - Fixtures are tiny and historically stable (the first YouTube channel's 19-second video);
//!   the playlist test pre-flights the entry count and refuses to run if the fixture grew.
//! - The one cookie-needing test skips-with-notice unless you supply a session (see
//!   [`cookie_flags_or_skip`]). It sends a real one ON PURPOSE — that is the thing under test —
//!   so it stays single, small, and paced.
//! - There is intentionally NO live channel test: a channel walk recurses into its playlists
//!   tab, and no public channel can promise that stays bounded. The channel path is covered by
//!   the stubbed suite; the uploads-playlist test validates the shared scan/download machinery
//!   live.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;

/// 19 seconds, public, real `en`+`de` subtitles — the historically stable sample.
const VIDEO: &str = "https://www.youtube.com/watch?v=jNQXAC9IVRw";
const VIDEO_ID: &str = "jNQXAC9IVRw";
/// The same channel's uploads playlist: exactly that one video, for 20 years.
const TINY_PLAYLIST: &str = "https://www.youtube.com/playlist?list=UU4QobU6STFB0P71PMvOGN5A";
const TINY_PLAYLIST_ID: &str = "UU4QobU6STFB0P71PMvOGN5A";
/// A known age-restricted video (public knowledge from yt-dlp's issue tracker) — the smallest
/// honest exercise of a cookie-gated download.
const RESTRICTED: &str = "https://www.youtube.com/watch?v=zykMWuCsKyw";
const RESTRICTED_ID: &str = "zykMWuCsKyw";

static SERIAL: Mutex<()> = Mutex::new(());

/// A moment of courtesy before each network-hitting test — spread the load, don't burst.
fn courtesy_pause() {
    std::thread::sleep(std::time::Duration::from_secs(3));
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vidl_ext_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run `vidl <url> --into <dir> <flags…>` against the real network, with whatever tools PATH
/// offers. No cookies unless a test passes them.
fn dl(url: &str, dir: &Path, flags: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vidl"))
        .arg(url)
        .arg("--into")
        .arg(dir)
        .args(flags)
        .output()
        .expect("run vidl")
}

fn text(out: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

/// An external tool, resolved the way this crate resolves it by default: by bare name, on PATH.
fn tool(name: &str) -> PathBuf {
    PathBuf::from(name)
}

/// The downloaded file carrying `__<id>.`, any media extension.
fn find_by_id(dir: &Path, id: &str) -> Option<PathBuf> {
    let marker = format!("__{id}.");
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|name| name.to_string_lossy().contains(&marker))
            {
                return Some(path);
            }
        }
    }
    None
}

/// Skip-with-notice: the cookie flag pair to pass, or `None` after a visible SKIP line when no
/// session is available.
///
/// Both shapes yt-dlp accepts are supported, because both are shapes people actually have:
///
/// - **a browser store** — a `browser.spec` naming the browser beside a `store/` directory
///   holding its cookie DB — becomes `--cookies-from-browser <browser>:<store>`. This is what a
///   tool that copies a profile's cookies produces, and what an embedder hands this crate.
/// - **a Netscape jar** (`youtube.txt`) becomes `--cookies <file>`. The portable shape, and what
///   browser extensions export.
///
/// Stores are kept one directory per site, so `<base>/<site>/` is checked before `<base>` itself
/// — drop a whole `youtube/` folder in and it works, and a later test for another domain just
/// passes a different `site` without the two sessions colliding.
///
/// Searched at `$VIDL_TEST_COOKIES` first — a directory in either arrangement, or a jar file
/// directly, so a live session can live outside this checkout — then at `tests/cookies/`, which
/// `.gitignore` keeps untracked. See `tests/cookies/README.md`.
fn cookie_flags_or_skip(test: &str, site: &str) -> Option<[String; 2]> {
    let supplied = std::env::var_os("VIDL_TEST_COOKIES").map(PathBuf::from);
    let base = match &supplied {
        // Pointed straight at a jar: use it, and let the emptiness check below have the last word.
        Some(path) if path.is_file() => return netscape_jar(path).or_else(|| skip(test, site)),
        Some(path) => path.clone(),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cookies"),
    };
    browser_store(&base.join(site))
        .or_else(|| browser_store(&base))
        .or_else(|| netscape_jar(&base.join(format!("{site}.txt"))))
        .or_else(|| skip(test, site))
}

fn skip(test: &str, site: &str) -> Option<[String; 2]> {
    eprintln!("SKIPPED {test}: no {site} cookies supplied (see tests/cookies/README.md)");
    None
}

/// `<dir>/browser.spec` + `<dir>/store/` as a `--cookies-from-browser` pair. The store must hold
/// at least one file: an empty directory would fail at the gate rather than here, which reads as
/// a broken session instead of a missing one.
fn browser_store(dir: &Path) -> Option<[String; 2]> {
    let browser = std::fs::read_to_string(dir.join("browser.spec")).ok()?;
    let browser = browser.trim();
    let store = dir.join("store");
    let populated = std::fs::read_dir(&store).is_ok_and(|mut entries| entries.next().is_some());
    (!browser.is_empty() && populated)
        .then(|| ["--cookies-from-browser".to_string(), format!("{browser}:{}", store.display())])
}

/// A Netscape jar as a `--cookies` pair. A file with no cookie rows counts as not supplied, for
/// the same reason as above.
fn netscape_jar(jar: &Path) -> Option<[String; 2]> {
    let rows = std::fs::read_to_string(jar).ok()?;
    rows.lines()
        .any(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .then(|| ["--cookies".to_string(), jar.display().to_string()])
}

#[test]
#[ignore = "live-extended: hits YouTube; run via TEST.sh"]
fn a_tiny_uploads_playlist_downloads_end_to_end() {
    let _serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    courtesy_pause();
    // Pre-flight: refuse to run if the fixture ever grows — a live collection test must never
    // be an unbounded download.
    let scan = Command::new(tool("yt-dlp"))
        .args(["--force-ipv4", "--flat-playlist", "--print", "%(id)s", TINY_PLAYLIST])
        .output()
        .expect("run yt-dlp");
    assert!(scan.status.success(), "pre-flight scan failed: {}", text(&scan));
    let entries = String::from_utf8_lossy(&scan.stdout).lines().count();
    if entries == 0 || entries > 5 {
        eprintln!("SKIPPED tiny-playlist: fixture has {entries} entries (expected 1..=5)");
        return;
    }

    let dir = scratch("playlist");
    let out = dl(TINY_PLAYLIST, &dir, &[]);
    assert!(out.status.success(), "{}", text(&out));
    // The playlist got its own folder, the archive recorded the entry, and the video landed.
    let home = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.is_dir() && path.file_name().is_some_and(|n| n.to_string_lossy().contains(TINY_PLAYLIST_ID))
        })
        .expect("a playlist folder named by the scan");
    let archive = std::fs::read_to_string(home.join(".dl_video_archive.txt")).expect("archive");
    assert!(archive.contains(VIDEO_ID), "the entry is archived: {archive}");
    assert!(find_by_id(&home, VIDEO_ID).is_some(), "the video file landed in the folder");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "live-extended: hits YouTube; run via TEST.sh"]
fn an_audio_download_lands_a_tagged_audio_file() {
    let _serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    courtesy_pause();
    let dir = scratch("audio");
    let out = dl(VIDEO, &dir, &["--audio"]);
    assert!(out.status.success(), "{}", text(&out));
    let file = find_by_id(&dir, VIDEO_ID).expect("an audio file landed");
    assert_ne!(file.extension().and_then(|e| e.to_str()), Some("mkv"), "audio mode, not video");
    // Whatever sidecars arrived must be folded into tags — never left as loose .vtt files.
    let stray_vtt = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .any(|entry| entry.path().extension().and_then(|e| e.to_str()) == Some("vtt"));
    assert!(!stray_vtt, "sidecars must be folded into tags:\n{}", text(&out));
    // Subtitle tracks are must-try: YouTube's caption endpoint 429s freely (datacenter IPs
    // especially), and a refused track downgrades to a warning by design. Only assert the
    // tagging when the subtitles actually arrived.
    if text(&out).contains("Unable to download video subtitles") {
        eprintln!("SKIPPED audio-tag verification: YouTube refused the subtitle tracks (429) this run");
    } else {
        let tags = Command::new(tool("ffprobe"))
            .args(["-v", "error", "-show_entries", "format_tags:stream_tags", "-of", "default=nw=1"])
            .arg(&file)
            .output()
            .expect("run ffprobe");
        let tags = String::from_utf8_lossy(&tags.stdout).to_lowercase();
        assert!(tags.contains("subtitles_en"), "the subtitle tag reads back: {tags}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "live-extended: hits the thumbnail CDN; run via TEST.sh"]
fn the_thumbnail_cdn_serves_hq_when_maxres_is_absent() {
    let _serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // Pins the convention fetch_youtube_thumbnail relies on: maxres is optional, hq is always
    // there. Two tiny HEAD-sized requests against the sample video.
    let code = |quality: &str| {
        let out = Command::new("curl")
            .args(["-fsSL", "-o", "/dev/null", "-w", "%{http_code}", "--max-time", "20"])
            .arg(format!("https://i.ytimg.com/vi/{VIDEO_ID}/{quality}.jpg"))
            .output()
            .expect("run curl");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    assert_eq!(code("hqdefault"), "200", "hqdefault must always exist");
    assert_eq!(code("maxresdefault"), "404", "this sample has no maxres — the fallback case");
}

#[test]
#[ignore = "live-extended (cookies): downloads a restricted video with the user's session; run via TEST.sh"]
fn an_age_restricted_video_downloads_with_supplied_cookies() {
    let _serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(cookies) = cookie_flags_or_skip("restricted download", "youtube") else { return };
    courtesy_pause();
    // The one test that sends a real session to YouTube — deliberately a single, small download.
    // It proves the whole cookie path end to end: cookies named on the command line reach yt-dlp
    // intact and clear a gate an anonymous run cannot.
    let dir = scratch("restricted");
    let flags: Vec<&str> = cookies.iter().map(String::as_str).collect();
    let out = dl(RESTRICTED, &dir, &flags);
    assert!(
        out.status.success(),
        "a signed-in, age-verified session should clear the gate:\n{}",
        text(&out)
    );
    assert!(find_by_id(&dir, RESTRICTED_ID).is_some(), "the restricted video landed");
    let _ = std::fs::remove_dir_all(&dir);
}
