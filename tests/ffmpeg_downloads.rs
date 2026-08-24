//! The download paths that only work because of *how* this crate drives ffmpeg, each pinned against
//! the regression that broke it in the wild.
//!
//! Nothing in the stubbed suite can catch either: it replaces yt-dlp with a script, so it never
//! reaches ffmpeg at all. Both of these were found by running the real thing, and both are the kind
//! of fault that returns quietly the next time the ffmpeg plumbing is touched.
//!
//! 1. **A video whose best formats are listed but not served.** Format ranking is done on the
//!    metadata a site advertises, which says nothing about whether the file behind it exists. When
//!    the winner 404s, the rescue is to make yt-dlp *check* each candidate before committing.
//!
//! 2. **An HLS stream handed to ffmpeg.** At its default log level ffmpeg names every segment it
//!    opens, and on a CDN that signs its URLs each of those lines runs to hundreds of bytes —
//!    measured at 161 lines and 144 KB for one 26-minute episode. Quieting it takes two separate
//!    `--downloader-args` keys, and getting only one of them looks exactly like success until a
//!    page that probes further along comes past.
//!
//! ```text
//! cargo test --test ffmpeg_downloads                          # the hermetic one
//! cargo test --test ffmpeg_downloads -- --ignored --nocapture # plus the live one
//! ```

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Whether `program` answers its version flag. Prints a skip notice and returns false when it
/// doesn't, so a machine without the tool reports honestly instead of failing.
///
/// The flag is passed in rather than assumed: ffmpeg takes `-version` and yt-dlp takes `--version`,
/// and using one for the other is a check that always says "absent" — a test that skips itself
/// forever while reporting `ok`.
fn tool_or_skip(test: &str, program: &str, flag: &str) -> bool {
    let works = Command::new(program)
        .arg(flag)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    if !works {
        eprintln!("SKIPPED {test}: no {program} on PATH");
    }
    works
}

/// A scratch directory of this test's own, emptied first so a rerun starts clean.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vidl_ff_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch dir");
    dir
}

/// Serve `dir` over HTTP on localhost and give back its base URL.
///
/// A file server in thirty lines of `std::net` rather than a dependency, for the same reason as the
/// one in `page::tests`: this is the only kind of test that needs a *real* fetch by a *real*
/// downloader, and the alternative is a rotting third-party URL. Every public AES-128 HLS sample
/// this was tried against is now dead or access-denied, which is the argument in miniature.
///
/// Absent paths get a 404 rather than a hang, so a mistake in a fixture fails fast.
fn serve(dir: PathBuf) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a local port");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let dir = dir.clone();
            // A connection per thread: ffmpeg and yt-dlp both keep one open while reading the next,
            // and serving them in turn on one thread would deadlock the pair.
            std::thread::spawn(move || {
                let mut stream = stream;
                let mut buffer = [0u8; 2048];
                let read = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                let name = path.trim_start_matches('/').split('?').next().unwrap_or("").to_string();
                let body = std::fs::read(dir.join(&name)).ok();
                let head = match &body {
                    Some(bytes) => format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
                         Connection: close\r\n\r\n",
                        if name.ends_with(".m3u8") { "application/vnd.apple.mpegurl" } else { "video/mp2t" },
                        bytes.len()
                    ),
                    None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string(),
                };
                let _ = stream.write_all(head.as_bytes());
                if let Some(bytes) = body {
                    let _ = stream.write_all(&bytes);
                }
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// A three-second HLS stream in `dir`, and the name of its playlist.
///
/// `mpeg2video` on purpose: it is built into every ffmpeg, where `libx264` is a compile-time option
/// this test has no business requiring. The clip only has to be a *real* HLS stream with more than
/// one segment — what is being tested is how its segments get announced, not what is in them.
fn build_hls(dir: &Path) -> &'static str {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-f", "lavfi", "-i", "testsrc=duration=3:size=128x96:rate=5"])
        .args(["-c:v", "mpeg2video", "-q:v", "8", "-f", "hls", "-hls_time", "1", "-hls_list_size", "0"])
        .arg("-hls_segment_filename")
        .arg(dir.join("s%d.ts"))
        .arg(dir.join("v.m3u8"))
        .output()
        .expect("run ffmpeg");
    assert!(out.status.success(), "ffmpeg could not build the fixture: {}", String::from_utf8_lossy(&out.stderr));
    let segments = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".ts"))
        .count();
    assert!(segments >= 2, "a one-segment stream would not exercise the narration ({segments} made)");
    "v.m3u8"
}

/// Everything the run printed, both streams together — the narration under test goes to stderr.
fn text(out: &std::process::Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

/// How many segments ffmpeg announced opening.
fn narrated(output: &str) -> usize {
    output.matches("Opening '").count()
}

#[test]
fn an_hls_stream_handed_to_ffmpeg_is_downloaded_without_narrating_every_segment() {
    let test = "an_hls_stream_handed_to_ffmpeg_is_downloaded_without_narrating_every_segment";
    if !tool_or_skip(test, "ffmpeg", "-version") || !tool_or_skip(test, "yt-dlp", "--version") {
        return;
    }
    let dir = scratch("hls_quiet");
    let playlist = build_hls(&dir);
    let base = serve(dir.clone());
    let url = format!("{base}/{playlist}");

    // A control run FIRST, straight through yt-dlp with none of this crate's arguments. It proves
    // the fixture really reaches ffmpeg's HLS demuxer — without it, the assertion below would pass
    // just as happily against a stream that never got there, and pass forever.
    let control = Command::new("yt-dlp")
        .args(["--no-progress", "--hls-prefer-ffmpeg", "-o"])
        .arg(dir.join("control.mp4"))
        .arg(&url)
        .output()
        .expect("run yt-dlp");
    let control_landed = dir.join("control.mp4").exists();
    let control = text(&control);
    assert!(
        narrated(&control) > 0,
        "the control must narrate, or this test proves nothing: {control}"
    );
    // The control also establishes that this machine *can* do the thing at all. Some ffmpeg builds
    // cannot — the johnvansickle 7.0.2 static build segfaults (`code -11`) on every HLS stream it
    // was given here, after logging. With no working baseline there is nothing to compare against,
    // so say so rather than blame the argument under test.
    if !control_landed {
        eprintln!("SKIPPED {test}: this ffmpeg cannot download HLS at all (the control produced no file)");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }

    // The same download through `vidl`, which passes the quieting arguments.
    let into = scratch("hls_quiet_into");
    let out = Command::new(env!("CARGO_BIN_EXE_vidl"))
        .arg(&url)
        .arg("--into")
        .arg(&into)
        // The metadata postprocessors are switched off deliberately. What is under test is how the
        // *downloader* logs, and leaving them on makes the test fail on any ffmpeg build that
        // cannot postprocess — which is a fault in the build, not in the argument being pinned.
        // `media_flags.rs` is what covers the postprocessing.
        .args(["--", "--hls-prefer-ffmpeg", "--no-progress"])
        .args(["--no-embed-metadata", "--no-embed-chapters"])
        .output()
        .expect("run vidl");
    let all = text(&out);
    assert!(out.status.success(), "the download must still succeed: {all}");
    assert_eq!(
        narrated(&all),
        0,
        "ffmpeg named {} segment(s) it opened; the control named {}: {all}",
        narrated(&all),
        narrated(&control)
    );
    // And it really downloaded something, so "quiet" cannot be quiet-because-nothing-happened.
    let landed: Vec<String> = std::fs::read_dir(&into)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    assert!(!landed.is_empty(), "nothing was downloaded: {all}");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&into);
}

#[test]
#[ignore = "live: downloads from a broadcaster's CDN; run via TEST.sh"]
fn a_video_whose_listed_formats_are_not_served_falls_back_to_one_that_is() {
    let test = "a_video_whose_listed_formats_are_not_served_falls_back_to_one_that_is";
    if !tool_or_skip(test, "ffmpeg", "-version") || !tool_or_skip(test, "yt-dlp", "--version") {
        return;
    }
    // The download address, and deliberately nothing else about the video. This entry's three
    // progressive MP4 flavours are listed by the site and 404 at its origin, while all three of its
    // HLS renditions serve — and the MP4s rank higher because the HLS ones report their codecs as
    // unknown. So the best format by metadata is one whose file does not exist, and no plain retry
    // can help. It is here because it is a real example of the shape, not because the site matters:
    // if it ever stops failing this way, the test stops proving anything and should be repointed.
    const ADDRESS: &str = "kaltura:2748741:1_sufliqd1";

    let into = scratch("format_fallback");
    let out = Command::new(env!("CARGO_BIN_EXE_vidl"))
        .arg(ADDRESS)
        .arg("--into")
        .arg(&into)
        .args(["--", "--test", "--no-progress"])
        .output()
        .expect("run vidl");
    let all = text(&out);

    // If the origin has been repaired, the premise is gone and the test would pass vacuously.
    if !all.contains("missing from the server") {
        eprintln!("SKIPPED {test}: the origin now serves its best format — nothing to fall back from");
        let _ = std::fs::remove_dir_all(&into);
        return;
    }
    assert!(out.status.success(), "the fallback must rescue the download: {all}");
    assert!(all.contains("succeeded on another format"), "and say so: {all}");
    let landed: Vec<String> = std::fs::read_dir(&into)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    assert_eq!(landed.len(), 1, "exactly one file, from the format that serves: {landed:?}");
    // Nothing ledgered: a rescued download is a successful one.
    let ledger = std::fs::read_to_string(into.join(".dl_video_failed_download.txt")).unwrap_or_default();
    assert!(ledger.trim().is_empty(), "a rescued download must not be recorded as failed: {ledger}");
    let _ = std::fs::remove_dir_all(&into);
}
