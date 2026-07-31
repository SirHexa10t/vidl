//! Everything that speaks to yt-dlp. This module builds the argv for — and launches — every
//! yt-dlp process the crate runs: the shared base (`common`), the per-mode argv builders, the
//! cookie/network flags, and the `run` wrapper with its 403 diagnosis.
//!
//! Its two children read what comes back: [`failures`] interprets a non-zero exit into advice
//! and a ledger line, [`scan`] turns a playlist or channel-tab listing into entries to download.

pub(crate) mod failures;
pub(crate) mod scan;

use std::ffi::OsString;
use std::path::Path;

use crate::Env;
use crate::util::exec::run_reporting_code;

/// Point yt-dlp at whatever an embedder bundled. It only ever searches `PATH` on its own, so a
/// bundled ffmpeg or JS runtime is invisible without these — and an embedder that bundles neither
/// gets an empty list and yt-dlp's own PATH lookup, which is the right default.
///
/// Read from [`crate::tools`] rather than passed in: they are installation paths, fixed for the
/// process, and every invocation in a run must agree on them.
fn bundle_flags() -> Vec<OsString> {
    bundle_flags_for(crate::tools::js_runtime(), crate::tools::ffmpeg_dir())
}

/// The pure half, split out because [`crate::tools`] is a process-wide `OnceLock`: a test that
/// installed a bundle would fix it for every other test in the binary.
fn bundle_flags_for(js_runtime: Option<&Path>, ffmpeg_dir: Option<&Path>) -> Vec<OsString> {
    let mut argv = Vec::new();
    if let Some(deno) = js_runtime {
        let mut runtime = OsString::from("deno:");
        runtime.push(deno.as_os_str());
        argv.push("--js-runtimes".into());
        argv.push(runtime);
    }
    if let Some(dir) = ffmpeg_dir {
        argv.push("--ffmpeg-location".into());
        argv.push(dir.as_os_str().to_owned());
    }
    argv
}

/// The cookie flags for an invocation: an explicit `--cookies` file wins; otherwise a
/// `--cookies-from-browser` spec from a prior `--cookie-import`; otherwise none. Shared by the
/// download argv ([`common`]) and the metadata invocations ([`seeded`]) so gated content
/// (age-restricted, members-only) is readable at *every* phase, not just the final download.
fn cookie_args(env: Env) -> Vec<OsString> {
    if let Some(file) = env.cookies {
        vec!["--cookies".into(), file.as_os_str().to_owned()]
    } else if let Some(spec) = env.cookies_from_browser {
        vec!["--cookies-from-browser".into(), spec.into()]
    } else {
        Vec::new()
    }
}

/// `--force-ipv4` unless the user opted into IPv6. Added to *every* network invocation (via
/// [`seeded`] and [`common`]): a broken or slow IPv6 route otherwise stalls each yt-dlp request
/// ~5s on the happy-eyeballs fallback (measured — the difference between a ~13s and a ~87s
/// download of one small video).
fn ipv4_flag(env: Env) -> Vec<OsString> {
    if env.allow_ipv6 {
        Vec::new()
    } else {
        vec!["--force-ipv4".into()]
    }
}

/// Starter argv for the metadata-side invocations (probes, scans): even those run the YouTube
/// extractor, which warns — and may miss formats — without a JS runtime, warns again without an
/// ffmpeg, and can't see gated content without cookies. Hand it the bundles and cookies.
pub(crate) fn seeded(env: Env) -> Vec<OsString> {
    let mut argv = ipv4_flag(env);
    argv.extend(bundle_flags());
    argv.extend(cookie_args(env));
    argv
}

/// The file name every YouTube mode shares: sortable upload date, title, and the video id that
/// keeps any file traceable back to its source (ideas kept from the old dl_youtube.py).
const YT_NAME: &str = "%(upload_date)s__%(title)s__%(id)s.%(ext)s";

/// Output template for a generic (non-YouTube) single download: title + id, flat under the
/// destination. Simpler than [`YT_NAME`] — a random site rarely carries a reliable upload date,
/// and there's no collection to sort into. The `[id]` also gives [`scrub_ledger`] a key to match.
const GENERIC_NAME: &str = "%(title)s [%(id)s].%(ext)s";

/// The download-archive's file name, dropped inside whatever folder owns the collection. Named
/// for the `dl` command, not YouTube — every platform's downloads log here now.
pub(crate) const ARCHIVE_NAME: &str = ".dl_video_archive.txt";

/// The flags every yt-dlp run shares: keep going past broken entries, parallel fragments, the
/// video's requested subtitles fetched-and-embedded (sidecars cleaned up), its title/uploader/
/// date/description and thumbnail embedded as tags and cover art (thumbnail converted to jpg —
/// the one format the mkv cover-art convention reliably recognizes), mkv output, and a
/// download-archive (at `archive_dir`) so interrupted or repeated runs resume instead of
/// redoing. yt-dlp marks the archive only after post-processing finishes — verified by racing
/// the archive file against the output directory.
///
/// mkv beats webm as the merge container even though both box the same codecs at the same
/// quality: mkv accepts any codec yt-dlp picks (h264 fallbacks included) and embeds the
/// thumbnail, where webm refuses attachments and leaves the art as loose image files.
pub(crate) fn common(archive_dir: &Path, env: Env, langs: &[String]) -> Vec<OsString> {
    // `--ignore-errors` also keeps subtitle picks must-try: a 429'd track warns and is skipped
    // without failing the video (see `sub_picks_for`).
    let mut argv: Vec<OsString> = ["--ignore-errors", "--concurrent-fragments", "4"]
        .into_iter()
        .map(OsString::from)
        .collect();
    argv.extend(ipv4_flag(env));
    if !langs.is_empty() {
        argv.extend(["--write-subs", "--write-auto-subs", "--sub-langs"].map(OsString::from));
        argv.push(langs.join(",").into());
        if !env.audio {
            // Audio mode skips these: subtitles can't embed into an extracted audio track, so
            // the `.vtt` sidecars are deliberately kept for [`embed_subtitle_tags`] to fold
            // into the file's metadata afterwards.
            argv.extend(["--embed-subs", "--compat-options", "no-keep-subs"].map(OsString::from));
        }
        // YouTube rate-limits its subtitle endpoint (429s, particularly on auto-translations);
        // a short pause per subtitle fetch stays under its radar and only taxes subbed videos.
        argv.extend(["--sleep-subtitles", "2"].map(OsString::from));
    }
    argv.extend(
        [
            "--embed-metadata", "--embed-chapters",
            "--merge-output-format", "mkv",
            "--download-archive",
        ]
        .map(OsString::from),
    );
    argv.push(archive_dir.join(ARCHIVE_NAME).into_os_string());
    argv.extend(bundle_flags());
    argv.extend(cookie_args(env));
    if env.audio {
        argv.push("-x".into());
    }
    if let Some(height) = env.res {
        argv.push("-S".into());
        argv.push(format!("res:{height}").into());
    }
    argv
}

/// How to invoke yt-dlp: the bundled zipapp is run through the bundled python *explicitly* —
/// its `env python3` shebang would otherwise pick whatever python the caller's PATH offers,
/// and the curl_cffi impersonation support lives in ours. A system yt-dlp runs as itself.
pub(crate) fn ytdlp_invocation(args: Vec<OsString>) -> (OsString, Vec<OsString>) {
    let (ytdlp, python) = crate::tools::ytdlp();
    match python {
        // A zipapp: run it through the interpreter the embedder named.
        Some(python) => {
            let mut full = vec![ytdlp];
            full.extend(args);
            (python, full)
        }
        None => (ytdlp, args),
    }
}

pub(crate) fn launch(argv: Vec<OsString>) -> i32 {
    let (program, args) = ytdlp_invocation(argv);
    let code = run_reporting_code(program, args);
    if code != 0 {
        // The interpreter's name in the line above obscures the real actor; and the most common
        // hard failure deserves its diagnosis spelled out.
        eprintln!(
            "vidl: yt-dlp failed (exit {code}). A `403 Forbidden` on media usually means the site \
             has blocklisted this network's IP (VPN / datacenter exits often are) — switching \
             the node/network tends to fix it, and --cookies is the other lever."
        );
    }
    code
}

/// A lone video, named [`YT_NAME`] directly under the destination (which also holds the
/// archive). `--no-playlist` pins the meaning even when the link happens to carry a `list=`
/// (the `--single` case).
pub(crate) fn video_argv(url: &str, into: &Path, env: Env, langs: &[String]) -> Vec<OsString> {
    let mut argv = common(into, env, langs);
    argv.extend(["--no-playlist", "--output"].map(OsString::from));
    argv.push(into.join(YT_NAME).into_os_string());
    argv.extend(env.extra.iter().map(OsString::from));
    argv.push(url.into());
    argv
}

/// A generic single download: [`common`] with no subtitle list (off YouTube there's no caption
/// matrix to resolve), flat under `into` via [`GENERIC_NAME`]. `--no-playlist` keeps a page that
/// happens to expose a playlist to the one video asked for — override with `-- --yes-playlist`,
/// since `extra` is appended last and wins.
pub(crate) fn generic_argv(url: &str, into: &Path, env: Env) -> Vec<OsString> {
    let mut argv = common(into, env, &[]);
    argv.extend(["--no-playlist", "--output"].map(OsString::from));
    argv.push(into.join(GENERIC_NAME).into_os_string());
    argv.extend(env.extra.iter().map(OsString::from));
    argv.push(url.into());
    argv
}

/// A playlist entry (or, `item`-less, a whole playlist in one pass — the scanless fallback):
/// its own `title[id]` folder under `into`, entries ordered by playlist position, the archive
/// at `archive_dir`.
pub(crate) fn playlist_argv(
    url: &str,
    into: &Path,
    archive_dir: &Path,
    env: Env,
    langs: &[String],
    item: Option<&str>,
) -> Vec<OsString> {
    let mut argv = common(archive_dir, env, langs);
    if let Some(index) = item {
        argv.extend([OsString::from("--playlist-items"), index.into()]);
    }
    argv.push("--output".into());
    argv.push(
        into.join(format!("%(playlist_title)s[%(playlist_id)s]/%(playlist_index)03d__{YT_NAME}"))
            .into_os_string(),
    );
    argv.extend(env.extra.iter().map(OsString::from));
    argv.push(url.into());
    argv
}

/// The channel tabs, each downloaded into its own folder under `uploader[channel_id]/`; the
/// playlists tab is handled by recursion instead (see [`download_channel`]).
pub(crate) const CHANNEL_TABS: &[&str] = &["videos", "shorts", "streams", "playlists"];

pub(crate) fn channel_tab_argv(
    tab_url: &str,
    tab: &str,
    into: &Path,
    archive_dir: &Path,
    env: Env,
    langs: &[String],
    item: &str,
) -> Vec<OsString> {
    let mut argv = common(archive_dir, env, langs);
    argv.extend([OsString::from("--playlist-items"), item.into()]);
    argv.push("--output".into());
    argv.push(
        into.join(format!("%(uploader)s[%(channel_id)s]/{tab}/{YT_NAME}")).into_os_string(),
    );
    argv.extend(env.extra.iter().map(OsString::from));
    argv.push(tab_url.into());
    argv
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Link, classify};
    
    
    use crate::testutil::en_keys;

    #[test]
    fn the_single_flag_pins_a_video_carrying_a_playlist_to_just_the_video() {
        let url = "https://www.youtube.com/watch?v=7jrKjkrX3Gw&list=PLxyz";
        assert_eq!(classify(url, true), Link::Video);
        let argv = video_argv(url, Path::new("."), Env::default(), &en_keys());
        assert!(argv.contains(&OsString::from("--no-playlist")), "{argv:?}");
    }

    #[test]
    fn a_bundled_tool_is_named_to_ytdlp_because_it_only_searches_path() {
        let flags = |js, ff| {
            bundle_flags_for(js, ff)
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            flags(Some(Path::new("/dn/deno")), Some(Path::new("/ff/bin"))),
            ["--js-runtimes", "deno:/dn/deno", "--ffmpeg-location", "/ff/bin"]
        );
        assert_eq!(flags(None, Some(Path::new("/ff/bin"))), ["--ffmpeg-location", "/ff/bin"]);
        assert_eq!(flags(Some(Path::new("/dn/deno")), None), ["--js-runtimes", "deno:/dn/deno"]);
        assert!(flags(None, None).is_empty(), "no bundle means yt-dlp's own PATH lookup");
    }

    #[test]
    fn every_yt_run_embeds_metadata_resumes_and_targets_mkv() {
        let argv = common(
            Path::new("/dl"),
            Env {
                cookies: Some(Path::new("/c.txt")),
                ..Default::default()
            },
            &en_keys(),
        );
        let text: Vec<String> = argv.iter().map(|arg| arg.to_string_lossy().into_owned()).collect();
        for expected in [
            "--write-subs", "--write-auto-subs", "--embed-subs",
            "--sub-langs", "en,en-US,en-GB",
            "--sleep-subtitles", "2",
            "--embed-metadata", "--embed-chapters",
            "--merge-output-format", "mkv",
            "--ignore-errors",
            "--download-archive", "/dl/.dl_video_archive.txt",
            "--cookies", "/c.txt",
        ] {
            assert!(text.iter().any(|arg| arg == expected), "missing {expected}: {text:?}");
        }
        // With no cookies, no flag appears — nor the knobs. And a video with no subtitles
        // anywhere requests none. (No bundle is installed in a test process, so the
        // `--ffmpeg-location`/`--js-runtimes` pair is absent from both argvs above; what puts
        // them there is covered by `a_bundled_tool_is_named_to_ytdlp_because_it_only_searches_path`.)
        let bare = common(Path::new("/dl"), Env::default(), &[]);
        for absent in [
            "--ffmpeg-location", "--cookies", "--cookies-from-browser", "--js-runtimes", "-x", "-S",
            "--write-subs", "--sub-langs", "--sleep-subtitles",
        ] {
            assert!(!bare.iter().any(|arg| arg == absent), "{absent} leaked in");
        }
        // Thumbnails are a late opt-in pass now, never inline in the download argv.
        assert!(!text.iter().any(|a| a == "--embed-thumbnail"), "thumbnail must not be inline");
    }


    // --- ffmpeg-backed round-trips (offline; skip-with-notice when no ffmpeg is available) -----

    #[test]
    fn ipv4_is_forced_by_default_on_probe_and_download_unless_allow_ipv6() {
        // Default: every network invocation forces IPv4 — a broken IPv6 route otherwise stalls
        // each request ~5s on the happy-eyeballs fallback.
        assert!(seeded(Env::default()).iter().any(|a| a == "--force-ipv4"), "probe forces v4");
        let dl = common(Path::new("/dl"), Env::default(), &[]);
        assert!(dl.iter().any(|a| a == "--force-ipv4"), "download forces v4");
        // `--allow-ipv6` opts back out, on both paths.
        let v6 = Env { allow_ipv6: true, ..Default::default() };
        assert!(!seeded(v6).iter().any(|a| a == "--force-ipv4"), "probe honors --allow-ipv6");
        let dl6 = common(Path::new("/dl"), v6, &[]);
        assert!(!dl6.iter().any(|a| a == "--force-ipv4"), "download honors --allow-ipv6");
    }

    #[test]
    fn the_generic_argv_reuses_the_shared_base_flat_no_subs_no_playlist() {
        let extra = vec!["--yes-playlist".to_string()];
        let argv = generic_argv(
            "https://vimeo.com/12345",
            Path::new("/dl"),
            Env { res: Some(720), extra: &extra, ..Default::default() },
        );
        let text: Vec<String> = argv.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        // Same shared knobs + log files as the YouTube path (metadata, thumbnail, mkv, archive).
        for expected in ["--embed-metadata", "--merge-output-format", "mkv", "--download-archive", "/dl/.dl_video_archive.txt"] {
            assert!(text.contains(&expected.to_string()), "missing {expected}: {text:?}");
        }
        // But no subtitle probing off YouTube, and a flat output template (no folder tree).
        for absent in ["--write-subs", "--write-auto-subs", "--sub-langs", "--embed-subs"] {
            assert!(!text.contains(&absent.to_string()), "{absent} leaked into the generic path");
        }
        let out = text.iter().position(|a| a == "--output").expect("has --output");
        assert_eq!(text[out + 1], "/dl/%(title)s [%(id)s].%(ext)s", "flat generic name under `into`");
        // Quality knob still applies; the URL lands last; `-- --yes-playlist` follows our
        // --no-playlist so a user can override the single-video default.
        assert!(text.contains(&"res:720".to_string()));
        assert_eq!(text.last().unwrap(), "https://vimeo.com/12345");
        let no = text.iter().position(|a| a == "--no-playlist").expect("defaults to single");
        let yes = text.iter().rposition(|a| a == "--yes-playlist").expect("extra passed through");
        assert!(yes > no, "user's --yes-playlist must come after our --no-playlist to win");
    }

    #[test]
    fn the_metadata_seed_carries_cookies_so_gated_content_is_readable_while_probing() {
        // Probes and scans authenticate too — an age-restricted video's subtitle probe or a
        // members-only tab's scan would otherwise run signed-out.
        let seed = seeded(Env { cookies_from_browser: Some("firefox:/store"), ..Default::default() });
        let text: Vec<String> = seed.iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let at = text.iter().position(|a| a == "--cookies-from-browser").expect("cookies in seed");
        assert_eq!(text[at + 1], "firefox:/store");
        // Nothing configured → a bare seed (bundles absent in the test env too).
        assert!(seeded(Env::default()).iter().all(|a| a != "--cookies" && a != "--cookies-from-browser"));
    }

    #[test]
    fn imported_browser_cookies_are_used_but_an_explicit_file_wins() {
        let pair = |argv: &[OsString], flag: &str| {
            argv.iter().position(|a| a == flag).map(|i| argv[i + 1].to_string_lossy().into_owned())
        };
        // A prior --cookie-import with no explicit file: the browser spec is passed.
        let imported = common(
            Path::new("/dl"),
            Env { cookies_from_browser: Some("firefox:/store"), ..Default::default() },
            &[],
        );
        assert_eq!(pair(&imported, "--cookies-from-browser").as_deref(), Some("firefox:/store"));
        assert!(!imported.iter().any(|a| a == "--cookies"));
        // An explicit --cookies file overrides the import — never both.
        let explicit = common(
            Path::new("/dl"),
            Env {
                cookies: Some(Path::new("/c.txt")),
                cookies_from_browser: Some("firefox:/store"),
                ..Default::default()
            },
            &[],
        );
        assert_eq!(pair(&explicit, "--cookies").as_deref(), Some("/c.txt"));
        assert!(!explicit.iter().any(|a| a == "--cookies-from-browser"), "explicit file must win alone");
    }

    #[test]
    fn the_optional_knobs_shape_the_run_and_extras_land_last() {
        let extra = vec!["--merge-output-format".to_string(), "webm/mkv".to_string()];
        let env = Env { audio: true, res: Some(1080), extra: &extra, ..Default::default() };
        let argv = video_argv("https://u", Path::new("/dl"), env, &en_keys());
        assert!(argv.contains(&OsString::from("-x")), "audio-only: {argv:?}");
        assert!(
            !argv.contains(&OsString::from("--embed-subs")),
            "audio can't hold subs — requesting them would strand sidecars: {argv:?}"
        );
        let sort = argv.iter().position(|arg| arg == "-S").expect("-S");
        assert_eq!(argv[sort + 1], OsString::from("res:1080"));
        // Extras sit after every default (so a repeated flag resolves their way) and only
        // the URL follows them.
        let output = argv.iter().position(|arg| arg == "--output").unwrap();
        let webm = argv.iter().position(|arg| arg == "webm/mkv").unwrap();
        assert!(output < webm, "extras must come after the defaults they override");
        assert_eq!(argv.last().unwrap(), &OsString::from("https://u"));
        assert_eq!(argv[argv.len() - 2], OsString::from("webm/mkv"));
    }

    #[test]
    fn each_mode_shapes_its_own_output_tree_and_archive_home() {
        let last = |argv: &[OsString]| argv.last().unwrap().to_string_lossy().into_owned();
        let template = |argv: &[OsString]| {
            let at = argv.iter().position(|a| a == "--output").expect("--output");
            argv[at + 1].to_string_lossy().into_owned()
        };
        let archive = |argv: &[OsString]| {
            let at = argv.iter().position(|a| a == "--download-archive").expect("archive flag");
            argv[at + 1].to_string_lossy().into_owned()
        };
        let langs = en_keys();

        let video = video_argv("https://youtu.be/x", Path::new("/dl"), Env::default(), &langs);
        assert_eq!(template(&video), format!("/dl/{YT_NAME}"));
        assert_eq!(last(&video), "https://youtu.be/x", "the url comes last");

        // A playlist entry: files under the playlist folder, the archive inside it, and the
        // specific entry selected.
        let entry = playlist_argv(
            "https://l",
            Path::new("/dl"),
            Path::new("/dl/My List[PL1]"),
            Env::default(),
            &langs,
            Some("4"),
        );
        assert!(template(&entry).starts_with("/dl/%(playlist_title)s[%(playlist_id)s]/"));
        assert!(template(&entry).contains("%(playlist_index)03d__"));
        assert_eq!(archive(&entry), "/dl/My List[PL1]/.dl_video_archive.txt");
        let items = entry.iter().position(|a| a == "--playlist-items").expect("item selection");
        assert_eq!(entry[items + 1], OsString::from("4"));

        let tab = channel_tab_argv(
            "https://c/videos",
            "videos",
            Path::new("/dl"),
            Path::new("/dl/Chan[UC1]"),
            Env::default(),
            &langs,
            "7",
        );
        assert!(template(&tab).starts_with("/dl/%(uploader)s[%(channel_id)s]/videos/"));
        assert_eq!(archive(&tab), "/dl/Chan[UC1]/.dl_video_archive.txt");
        assert_eq!(last(&tab), "https://c/videos", "the tab url comes last");
    }

}
