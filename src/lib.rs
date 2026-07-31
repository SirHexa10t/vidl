//! **vidl** — drive `yt-dlp`: work out what a URL points at, download it, and patch the result.
//!
//! The conductor is here: [`classify`] decides whether a URL is a video, a playlist or a whole
//! channel, and the `download_*` entry points run it, **one video at a time**. That costs an extra
//! metadata request per video and buys the thing a single batch invocation cannot express —
//! per-video subtitle selection against YouTube's ~157-language caption matrix (see [`subtitles`]).
//!
//! The work splits in two, and the directories say which half a file is in:
//!
//! - `ytdlp/` — talking *to* yt-dlp: argv assembly and launch, failure diagnosis and the
//!   failed-downloads ledger, playlist/channel-tab enumeration.
//! - `media/` — working on what came *back*: thumbnail, subtitle-track and audio-tag patching,
//!   and the per-video subtitle plan drawn from the caption matrix.
//! - `tools`/`util` — where the external programs live, and the handful of dependency-free
//!   helpers (process, colour, timestamp) the two halves share.
//!
//! Named for the job, not the site: the caption matrix and channel tabs are YouTube-shaped, but
//! [`download_generic`] serves any other site yt-dlp supports, reusing everything site-agnostic —
//! the argv base, the failure diagnosis and geo rescue, the ledger, and the download archive.
//!
//! # Using it
//!
//! Two ways in, and they can do the same things:
//!
//! - [`run`] takes the CLI's own [`Args`] — the whole binary is `run(Args::parse())`, so an
//!   embedder that wants `vidl`'s behaviour wholesale reuses the flags instead of restating them.
//! - [`classify`] plus the `download_*` functions, driven by an [`Env`], for a caller that wants
//!   to make the routing decision itself.
//!
//! yt-dlp is expected on `PATH`. An embedder that pins its own copy (or bundles ffmpeg or a JS
//! runtime) names them through [`tools::install`] once at startup. Cookies are *not* this crate's
//! business: it accepts a `--cookies` file or a `--cookies-from-browser` spec and passes it on,
//! leaving acquisition — finding browsers, paring a cookie DB to one site — to whoever has an
//! opinion about it.

pub mod tools;

mod cli;
mod media;
mod util;
mod ytdlp;

pub use cli::{Args, run};

/// What the user asked for, shared by every yt-dlp invocation of one run. The mirror of [`Args`]
/// (which is where a CLI-shaped caller starts) minus the parsing — *what to do*, never *where the
/// programs are*; that is [`tools`]'s job and is set once per process.
#[derive(Clone, Copy, Default)]
pub struct Env<'a> {
    /// The `--cookies` file, when the caller supplied one (explicit; wins over the browser spec).
    pub cookies: Option<&'a Path>,
    /// A `--cookies-from-browser` spec (`<browser>[:<dir>]`), used on every run unless an
    /// explicit `--cookies` file overrides it.
    pub cookies_from_browser: Option<&'a str>,
    /// Audio-only extraction (`-x`).
    pub audio: bool,
    /// Height cap, as a format-sort preference (`-S res:N`).
    pub res: Option<u32>,
    /// Let yt-dlp use IPv6. Off by default — every invocation adds `--force-ipv4`, because a
    /// broken or slow IPv6 route stalls each request ~5s on the happy-eyeballs fallback (measured);
    /// `--allow-ipv6` opts back in for an IPv6-only network.
    pub allow_ipv6: bool,
    /// Embed a cover-art thumbnail. Off by default — a late, idempotent pass handles it, so the
    /// main download stays fast and a re-run can patch previously-downloaded videos.
    pub thumbnail: bool,
    /// Force the subtitle patch pass: scan a video (fresh or already on disk) for its expected
    /// subtitles and embed any that are missing. A fresh download already embeds subtitles
    /// inline, so this is a no-op there and a patch on re-runs.
    pub subtitles: bool,
    /// Raw passthrough args, appended after every default so they win any flag repeated.
    pub extra: &'a [String],
}

/// Run yt-dlp with `args` and capture both streams — for a caller that needs to *ask yt-dlp
/// something* rather than download anything (say, "can you read this cookie store?").
///
/// Public because the answer depends on how yt-dlp is installed: a zipapp must be run through the
/// interpreter [`tools::install`] named, and a caller reimplementing that would get it wrong the
/// first time yt-dlp isn't a plain binary. `None` only when it couldn't be launched at all.
pub fn ask_ytdlp(args: Vec<OsString>) -> Option<(bool, String, String)> {
    ytdlp::failures::capture_ytdlp(args)
}

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::util::exec::run_reporting_code;
use crate::util::style;
use ytdlp::failures::{GeoRescue, diagnose_failure, scrub_ledger, write_ledger};
use ytdlp::{ARCHIVE_NAME, CHANNEL_TABS, channel_tab_argv, generic_argv, playlist_argv, launch, video_argv, ytdlp_invocation};
use media::{embed_subtitles, embed_thumbnails, find_by_id, finish_media, patch_collection_subtitles};
use ytdlp::scan::{ScanEntry, TabScan, archived_ids, is_tombstone, scan_playlist, scan_tab, unplayable_report};
use media::subtitles::{Pick, Planned, batch_probe, default_picks, group_by_langs, video_picks};

/// What a URL points at — each kind downloads differently.
#[derive(Debug, PartialEq)]
pub enum Link {
    /// A single YouTube video.
    Video,
    /// A YouTube playlist, by its `list=` id.
    Playlist { id: String },
    /// A YouTube channel, normalized to its root.
    Channel { root: String },
    /// Any other site yt-dlp supports. Downloaded flat, without the playlist/channel folder
    /// trees or the caption-matrix subtitle pass — both are YouTube-shaped and there is no
    /// equivalent structure to read off an arbitrary page.
    Generic,
}

/// Classify `url`.
///
/// A non-YouTube host is [`Link::Generic`] and nothing else — the host is checked *first*, so a
/// path that merely looks YouTube-shaped (`https://example.com/@someone`) can't be mistaken for
/// a channel. On YouTube: channel forms (`/@handle`, `/channel/…`, `/c/…`, `/user/…`) normalize
/// to the channel root — any tab suffix is dropped, since tabs are enumerated at download time —
/// a `list=` parameter (or an explicit `/playlist`) means playlist unless `single` opts out, and
/// everything else is a lone video.
pub fn classify(url: &str, single: bool) -> Link {
    if !is_youtube(url) {
        return Link::Generic;
    }
    for marker in ["/@", "/channel/", "/c/", "/user/"] {
        if let Some(start) = url.find(marker) {
            let name = start + marker.len();
            let end = url[name..].find(['/', '?', '#']).map(|i| name + i).unwrap_or(url.len());
            return Link::Channel { root: url[..end].to_string() };
        }
    }
    if !single {
        if let Some((_, rest)) = url.split_once("list=") {
            let id = rest.split(['&', '#']).next().unwrap_or_default();
            if !id.is_empty() {
                return Link::Playlist { id: id.to_string() };
            }
        }
    }
    Link::Video
}

/// Whether `url`'s host is YouTube — any subdomain of `youtube.com` / `youtube-nocookie.com`, or
/// `youtu.be`. Matched on the host alone: `notyoutube.com` and `youtube.com.example.test` are
/// other people's sites, and `example.com/youtube.com` is a path.
fn is_youtube(url: &str) -> bool {
    let host = host_of(url);
    host == "youtu.be"
        || host == "youtube.com"
        || host.ends_with(".youtube.com")
        || host == "youtube-nocookie.com"
        || host.ends_with(".youtube-nocookie.com")
}

/// The host part of `url`: scheme dropped, path/query/fragment dropped, any userinfo and port
/// dropped, lowercased. Not a URL parser — enough of one to tell whose site this is.
fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once('@').map(|(_, host)| host).unwrap_or(authority);
    host.split(':').next().unwrap_or_default().to_ascii_lowercase()
}

/// The 11-char video id in a lone YouTube URL — `watch?v=ID`, `youtu.be/ID`, `/shorts/ID`,
/// `/embed/ID`. `None` for a shape we don't recognize (the caller falls back to a probe). Lets
/// [`download_video`] tell "already on disk" without a yt-dlp call, so it can skip the subtitle
/// probe + download for a video it already has.
fn id_from_url(url: &str) -> Option<String> {
    let rest = ["v=", "youtu.be/", "/shorts/", "/embed/", "/live/"]
        .iter()
        .find_map(|marker| url.split_once(marker).map(|(_, rest)| rest))?;
    let id: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    (id.len() == 11).then_some(id)
}

/// A lone video: probe its subtitle situation, then download with the exact list.
pub fn download_video(url: &str, into: &Path, env: Env) -> i32 {
    // The subtitle probe and the download are both for a video we don't have yet. If its file is
    // already on disk (id parsed straight from the URL — no yt-dlp call), skip both: subtitle
    // handling rides the download, so there's nothing to probe for. A URL we can't pull an id from
    // falls back to the probe. The opt-in `--thumbnail` pass below still runs, patching a re-run.
    let url_id = id_from_url(url);
    let on_disk = url_id.as_deref().is_some_and(|id| find_by_id(into, id).is_some());
    let mut code = 0;
    // `picks` feed the opt-in `--subtitles` pass. A fresh download always probes (it needs the
    // list to embed inline); an already-downloaded video probes only when `--subtitles` forces a
    // scan; a plain re-run on an existing video probes nothing.
    let mut picks: Vec<Pick> = Vec::new();
    let id = if !on_disk {
        // Announced because the probe runs silently for a few seconds (its output is captured).
        println!("probing subtitles…");
        let (probe_id, probed) = video_picks(url, env);
        picks = probed;
        let keys: Vec<String> = picks.iter().map(|pick| pick.key.clone()).collect();
        code = launch(video_argv(url, into, env, &keys));
        if code != 0 {
            // The bracketed id is the ledger's stable key ([`scrub_ledger`]); a URL label still
            // carries the id for YouTube links via its `v=` parameter.
            let label =
                probe_id.as_ref().map(|id| format!("[{id}]")).unwrap_or_else(|| url.to_string());
            match diagnose_failure(video_argv(url, into, env, &keys), &label, GeoRescue::IpEnforced)
            {
                None => code = 0, // the plain retry came through
                Some(line) => write_ledger(into, &[line]),
            }
        }
        if code == 0 {
            if let Some(id) = &probe_id {
                finish_media(into, id, &picks, env);
            }
            // The explicit happy ending — yt-dlp's own output stops at its last postprocessor
            // ("[Metadata] …"), which reads unfinished.
            let done = probe_id.as_deref().or(url_id.as_deref()).unwrap_or(url);
            println!("{}", style::approved(&format!("{done}: downloaded")));
        }
        probe_id.or(url_id)
    } else if env.subtitles {
        println!("{}: already downloaded — scanning subtitles", url_id.as_deref().unwrap_or(url));
        let (probe_id, probed) = video_picks(url, env);
        picks = probed;
        probe_id.or(url_id)
    } else {
        println!("{}: already downloaded — skipping", url_id.as_deref().unwrap_or(url));
        url_id
    };
    if code == 0 {
        if let Some(id) = &id {
            if env.thumbnail {
                embed_thumbnails(into, std::slice::from_ref(id));
            }
            if env.subtitles {
                println!("subtitles: scanning 1 video…");
                embed_subtitles(into, id, url, &picks, env);
            }
        }
    }
    scrub_ledger(into);
    code
}

/// The sites the late `--thumbnail`/`--subtitles` patch passes support — YouTube only, today.
/// Kept as a list so the generic path's notice names exactly what IS covered as it grows.
const PATCHABLE_PLATFORMS: &[&str] = &["youtube"];

/// The generic single-video path for non-YouTube sites (`dl` routes here when the host isn't
/// YouTube). One download, flat into `into` — a generic page gives no playlist/channel structure
/// to build folders from — reusing this module's shared argv base ([`common`]), failure
/// diagnosis + rescue, ledger, and download archive. No subtitle probing: that's a
/// YouTube-caption-matrix affair, so a generic site just gets the media, metadata, thumbnail, and
/// chapters. The URL is the ledger label — the only stable key without a metadata probe (and
/// enough for [`scrub_ledger`] when it embeds the id).
pub fn download_generic(url: &str, into: &Path, env: Env) -> i32 {
    // The patch passes need a per-video id in the filename and a known thumbnail/caption source —
    // YouTube-machinery, today. Name what IS supported (the list will grow) instead of silently
    // ignoring the flag.
    if env.thumbnail || env.subtitles {
        let flags = match (env.thumbnail, env.subtitles) {
            (true, true) => "--thumbnail/--subtitles",
            (true, false) => "--thumbnail",
            _ => "--subtitles",
        };
        eprintln!(
            "vidl: {flags} supported platforms: {} — skipped for this site",
            PATCHABLE_PLATFORMS.join(", ")
        );
    }
    let mut code = launch(generic_argv(url, into, env));
    if code != 0 {
        match diagnose_failure(generic_argv(url, into, env), url, GeoRescue::XffSweep) {
            None => code = 0, // the plain retry or a spoofed region came through
            Some(line) => write_ledger(into, &[line]),
        }
    }
    scrub_ledger(into);
    code
}

/// Playlist mode. The flat scan comes first — it still *names* entries nothing can play
/// anymore, which downloads would only surface as opaque errors — the traceability report and
/// the download archive live inside the playlist's own folder, and every playable, not-yet-
/// archived entry is downloaded individually with its own subtitle list.
pub fn download_playlist(url: &str, id: &str, into: &Path, env: Env) -> i32 {
    let Some(scan) = scan_playlist(url, "%(title)S[%(id)S]", env) else {
        eprintln!(
            "vidl: could not scan the playlist — downloading in one pass, EN-only subtitles, \
             no unplayable report"
        );
        let keys: Vec<String> = default_picks().iter().map(|pick| pick.key.clone()).collect();
        return launch(playlist_argv(url, into, into, env, &keys, None));
    };
    // The playlist's folder, spelled exactly as yt-dlp's template expansion will spell it.
    let dir = scan.dirname.clone().unwrap_or_else(|| format!("[{id}]"));
    let home = into.join(&dir);
    if let Err(err) = std::fs::create_dir_all(&home) {
        eprintln!("vidl: cannot create {}: {err}", home.display());
        return 1;
    }

    let dead: Vec<&ScanEntry> =
        scan.entries.iter().filter(|entry| is_tombstone(&entry.title)).collect();
    if dead.is_empty() {
        println!("all {} playlist entries are playable", scan.entries.len());
    } else {
        let report = home.join(format!("unplayable__{id}.txt"));
        match std::fs::write(&report, unplayable_report(&scan.title, id, &dead)) {
            Ok(()) => println!(
                "{} of {} entries are unplayable — traces written to {}",
                dead.len(),
                scan.entries.len(),
                report.display()
            ),
            Err(err) => eprintln!("vidl: could not write {}: {err}", report.display()),
        }
    }

    let archived = archived_ids(&home.join(ARCHIVE_NAME));
    let pending: Vec<&ScanEntry> = scan
        .entries
        .iter()
        .filter(|entry| !is_tombstone(&entry.title) && !archived.contains(&entry.id))
        .collect();
    let skipped = scan.entries.len() - pending.len();
    if skipped > 0 {
        println!("{skipped} entries already archived (or unplayable) — skipped");
    }
    let code = if pending.is_empty() {
        0
    } else {
        download_pending(url, &pending, into, &home, env, |url, into, home, env, langs, items| {
            playlist_argv(url, into, home, env, langs, Some(items))
        })
    };
    // Late thumbnail pass (opt-in) over every playable entry — including archived ones, so a
    // re-run with `--thumbnail` patches previously-downloaded videos.
    if env.thumbnail {
        let ids: Vec<String> = scan
            .entries
            .iter()
            .filter(|entry| !is_tombstone(&entry.title))
            .map(|entry| entry.id.clone())
            .collect();
        embed_thumbnails(&home, &ids);
    }
    if env.subtitles {
        let entries: Vec<&ScanEntry> =
            scan.entries.iter().filter(|entry| !is_tombstone(&entry.title)).collect();
        patch_collection_subtitles(url, &entries, &home, env);
    }
    code
}

/// The shared batched tail of playlist and channel-tab downloads: ONE probe invocation covers
/// every pending entry's subtitles, entries are grouped by their computed list, and each group
/// downloads in one yt-dlp invocation — process startup and player work are the expensive
/// parts, so the invocation count is what matters. Probe-failed entries fall back to the EN
/// default rather than being dropped.
fn download_pending(
    url: &str,
    pending: &[&ScanEntry],
    into: &Path,
    home: &Path,
    env: Env,
    argv: impl Fn(&str, &Path, &Path, Env, &[String], &str) -> Vec<OsString>,
) -> i32 {
    println!("probing subtitles of {} entries…", pending.len());
    let indexes: Vec<String> = pending.iter().map(|entry| entry.index.clone()).collect();
    let probed = batch_probe(url, &indexes, env);
    let planned: Vec<Planned> = pending
        .iter()
        .map(|entry| {
            probed
                .iter()
                .find(|plan| plan.index == entry.index)
                .map(|plan| Planned {
                    index: plan.index.clone(),
                    id: plan.id.clone(),
                    picks: plan.picks.clone(),
                })
                .unwrap_or_else(|| Planned {
                    index: entry.index.clone(),
                    id: entry.id.clone(),
                    picks: default_picks(),
                })
        })
        .collect();
    let mut worst = 0;
    for group in group_by_langs(&planned) {
        let keys: Vec<String> =
            group[0].picks.iter().map(|pick| pick.key.clone()).collect();
        let items =
            group.iter().map(|plan| plan.index.as_str()).collect::<Vec<_>>().join(",");
        let subs = if keys.is_empty() { "none".to_string() } else { keys.join(",") };
        println!(
            "=== downloading {} entr{} (subs: {subs}) ===",
            group.len(),
            if group.len() > 1 { "ies" } else { "y" },
        );
        let code = launch(argv(url, into, home, env, &keys, &items));
        if code != 0 && worst == 0 {
            worst = code;
        }
        for plan in &group {
            finish_media(home, &plan.id, &plan.picks, env);
        }
    }
    // Post-mortem: whatever is still unarchived failed inside a group, where `--ignore-errors`
    // kept the rest flowing but discarded the why. Each gets a captured re-run to diagnose,
    // geo-blocks get the region rescue, and the stubborn ones go to the ledger — unarchived on
    // purpose, so future runs keep retrying them (and the scrub below clears their ledger lines
    // the moment a retry lands).
    let survivors = archived_ids(&home.join(ARCHIVE_NAME));
    let mut ledger = Vec::new();
    for plan in planned.iter().filter(|plan| !survivors.contains(&plan.id)) {
        let title = pending
            .iter()
            .find(|entry| entry.index == plan.index)
            .map(|entry| entry.title.as_str())
            .unwrap_or(&plan.id);
        // The bracketed id is the ledger's stable key — index and title both drift as the
        // playlist changes, so scrub_ledger matches on the id alone.
        let label = format!("#{} {title} [{}]", plan.index, plan.id);
        println!("--- diagnosing {label} ---");
        let keys: Vec<String> = plan.picks.iter().map(|pick| pick.key.clone()).collect();
        match diagnose_failure(argv(url, into, home, env, &keys, &plan.index), &label, GeoRescue::IpEnforced) {
            // A plain-retry success downloaded the file just now, after the group's finish pass
            // already ran — so post-process it here, or it would keep yt-dlp's default subtitle
            // titles and (in audio mode) miss its metadata tags.
            None => finish_media(home, &plan.id, &plan.picks, env),
            Some(line) => ledger.push(line),
        }
    }
    scrub_ledger(home);
    write_ledger(home, &ledger);
    worst
}

/// Channel mode: every tab in turn. The archive lives inside the channel's own folder (shared
/// by its tabs); the video-bearing tabs download entry by entry like a playlist, and the
/// playlists tab recurses into [`download_playlist`] per playlist — each with its own folder,
/// archive, and unplayable report, nested under `<channel>/playlists/`. A failing tab (most
/// channels lack a few) doesn't stop the rest; the run counts as success if any came through.
pub fn download_channel(root: &str, into: &Path, env: Env) -> i32 {
    let mut home: Option<PathBuf> = None; // settled by the first readable tab's scan
    let mut succeeded = 0;
    for tab in CHANNEL_TABS {
        let tab_url = format!("{root}/{tab}");
        println!("=== {tab_url} ===");
        let scan = match scan_tab(&tab_url, "%(uploader)S[%(channel_id)S]", env) {
            TabScan::Found(scan) => scan,
            TabScan::Missing => {
                println!("the channel has no `{tab}` tab");
                continue;
            }
            TabScan::Failed => {
                eprintln!("vidl: could not read the `{tab}` tab — moving on");
                continue;
            }
        };
        let home = home
            .get_or_insert_with(|| {
                // The channel's own folder holds the archive all tabs share; an unprobeable
                // name degrades to a shared root archive.
                let dir = match &scan.dirname {
                    Some(dir) => into.join(dir),
                    None => into.to_path_buf(),
                };
                let _ = std::fs::create_dir_all(&dir);
                dir
            })
            .clone();
        if *tab == "playlists" {
            // Entries here are playlists, not videos — recurse, nesting under the channel.
            let nest = home.join("playlists");
            for entry in &scan.entries {
                println!("--- playlist: {} ---", entry.title);
                let url = format!("https://www.youtube.com/playlist?list={}", entry.id);
                if download_playlist(&url, &entry.id, &nest, env) == 0 {
                    succeeded += 1;
                }
            }
            continue;
        }
        let archived = archived_ids(&home.join(ARCHIVE_NAME));
        let pending: Vec<&ScanEntry> =
            scan.entries.iter().filter(|entry| !archived.contains(&entry.id)).collect();
        let skipped = scan.entries.len() - pending.len();
        if skipped > 0 {
            println!("{skipped} entries already archived — skipped");
        }
        let tab_code = if pending.is_empty() {
            0
        } else {
            download_pending(&tab_url, &pending, into, &home, env, |url, into, home, env, langs, items| {
                channel_tab_argv(url, tab, into, home, env, langs, items)
            })
        };
        // Late thumbnail pass (opt-in) over the whole tab — archived entries included, so a re-run
        // with `--thumbnail` patches previously-downloaded videos.
        if env.thumbnail {
            let ids: Vec<String> = scan.entries.iter().map(|entry| entry.id.clone()).collect();
            embed_thumbnails(&home, &ids);
        }
        if env.subtitles {
            let entries: Vec<&ScanEntry> = scan.entries.iter().collect();
            patch_collection_subtitles(&tab_url, &entries, &home, env);
        }
        if tab_code == 0 {
            succeeded += 1;
        }
    }
    i32::from(succeeded == 0)
}

/// The `-t/--taglist` listing: the notable yt-dlp flags first, styled for a quick scan, then
/// yt-dlp's own full option list (the repetition is fine — this is the index, that's the book).
pub fn taglist() -> i32 {
    println!("{}", style::header("Notable yt-dlp flags — pass them after `--`:"));
    let width = NOTABLE_FLAGS.iter().map(|(flag, _)| flag.len()).max().unwrap_or(0);
    for (flag, blurb) in NOTABLE_FLAGS {
        let pad = " ".repeat(width - flag.len());
        println!("  {}{pad}  {blurb}", style::argname(flag));
    }
    println!("\n{}", style::header("Everything yt-dlp accepts:"));
    let (program, args) = ytdlp_invocation(vec![OsString::from("--help")]);
    run_reporting_code(program, args)
}

/// The flags worth knowing about, shown by [`taglist`]: `(flag as you'd type it, what it does)`.
const NOTABLE_FLAGS: &[(&str, &str)] = &[
    ("--sponsorblock-remove sponsor", "cut sponsored segments out of the video"),
    ("--write-description", "save the description as a sidecar file"),
    ("--write-info-json", "save every scrap of metadata as JSON"),
    ("--playlist-items 3,5-7", "download only a slice of a playlist"),
    ("--limit-rate 2M", "cap the download speed"),
    ("--merge-output-format webm/mkv", "prefer webm (forfeits embedded cover art)"),
    ("--sub-langs all", "every real subtitle language, not just EN"),
    ("--proxy URL", "route through a proxy"),
];

/// Test fixtures shared by the sibling modules' suites — the scratch dir, the
/// ffmpeg availability gate, and the language-key shorthand.
#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    /// The EN-family keys, as tests pass them to the argv builders.
    pub(crate) fn en_keys() -> Vec<String> {
        ["en", "en-US", "en-GB"].map(str::to_string).to_vec()
    }

    /// A fresh scratch directory under the system temp dir.
    pub(crate) fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vidl_yt_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Skip-with-notice: `true` when ffmpeg+ffprobe are runnable. Tests resolve them on `PATH`,
    /// which is this crate's own default (`Env::ffmpeg_dir` left unset) — an embedder's bundle is
    /// the embedder's business, and its tests should exercise its own wiring.
    pub(crate) fn ffmpeg_or_skip(test: &str) -> bool {
        let works = ["ffmpeg", "ffprobe"].iter().all(|name| {
            std::process::Command::new(ff_bin(name))
                .arg("-version")
                .output()
                .is_ok_and(|out| out.status.success())
        });
        if !works {
            eprintln!("SKIPPED {test}: no usable ffmpeg/ffprobe available");
        }
        works
    }

    /// An ffmpeg-family binary, resolved exactly the way the code under test resolves it. No
    /// bundle is installed in a test process, so both of these come off `PATH`.
    pub(crate) fn ff_bin(name: &str) -> std::ffi::OsString {
        match name {
            "ffprobe" => crate::tools::ffprobe(),
            _ => crate::tools::ffmpeg(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_classify_by_what_they_point_at() {
        let video = |url: &str| assert_eq!(classify(url, false), Link::Video, "{url}");
        video("https://www.youtube.com/watch?v=MFT4OgFxfes");
        video("http://www.youtube.com/watch?v=MFT4OgFxfes");
        video("https://youtu.be/pv21e6iEZUw?si=sl5UGl0DI00f-0_h");
        video("https://youtu.be/IYnsfV5N2n8?si=6xSF90BnXIJcdy04&t=39"); // timestamped share

        let playlist = classify(
            "https://www.youtube.com/watch?v=7jrKjkrX3Gw&list=PLvR1Vs9Qj4fk-VnGR2xUtNLvLcwBwGB6V",
            false,
        );
        assert_eq!(playlist, Link::Playlist { id: "PLvR1Vs9Qj4fk-VnGR2xUtNLvLcwBwGB6V".into() });
        assert_eq!(
            classify("https://www.youtube.com/playlist?list=PLxyz&feature=share", false),
            Link::Playlist { id: "PLxyz".into() }
        );
    }

    #[test]
    fn any_other_site_is_generic_and_the_host_decides_not_the_path() {
        let generic = |url: &str| assert_eq!(classify(url, false), Link::Generic, "{url}");
        generic("https://vimeo.com/12345");
        generic("https://twitter.com/u/status/1");
        generic("https://notyoutube.com/x");        // must not match by suffix-substring
        generic("https://youtube.com.evil.test/x"); // the host is evil.test
        generic("https://example.com/youtube.com"); // youtube.com only in the path
        // The host is checked before the channel/playlist markers, so a look-alike path on
        // someone else's site downloads flat instead of being walked as a channel.
        generic("https://example.com/@someone");
        generic("https://example.com/watch?v=x&list=PLxyz");

        let yt = |url: &str| assert_ne!(classify(url, false), Link::Generic, "{url}");
        yt("https://youtu.be/x");
        yt("https://music.youtube.com/watch?v=x");
        yt("https://m.youtube.com/watch?v=x");
        yt("http://youtube.com/playlist?list=y");
        yt("https://www.youtube-nocookie.com/embed/x");
        yt("https://YouTube.com/watch?v=x");  // hosts are case-insensitive
        yt("https://www.youtube.com:443/watch?v=x");
    }

    #[test]
    fn channel_urls_normalize_to_their_root_dropping_any_tab() {
        for url in [
            "https://www.youtube.com/@MontemayorChannel",
            "https://www.youtube.com/@MontemayorChannel/videos",
            "https://www.youtube.com/@MontemayorChannel/streams?view=0",
        ] {
            assert_eq!(
                classify(url, false),
                Link::Channel { root: "https://www.youtube.com/@MontemayorChannel".into() },
                "{url}"
            );
        }
        assert_eq!(
            classify("https://www.youtube.com/channel/UCabc123/playlists", false),
            Link::Channel { root: "https://www.youtube.com/channel/UCabc123".into() }
        );
        assert_eq!(
            classify("https://www.youtube.com/user/OldName", false),
            Link::Channel { root: "https://www.youtube.com/user/OldName".into() }
        );
    }

    #[test]
    fn id_from_url_pulls_the_11_char_video_id_or_gives_up() {
        assert_eq!(id_from_url("https://www.youtube.com/watch?v=MFT4OgFxfes").as_deref(), Some("MFT4OgFxfes"));
        assert_eq!(id_from_url("https://youtu.be/pv21e6iEZUw?si=x").as_deref(), Some("pv21e6iEZUw"));
        assert_eq!(
            id_from_url("https://www.youtube.com/watch?v=7jrKjkrX3Gw&list=PLxyz").as_deref(),
            Some("7jrKjkrX3Gw")
        );
        assert_eq!(id_from_url("https://www.youtube.com/shorts/abcDEF12345").as_deref(), Some("abcDEF12345"));
        assert_eq!(id_from_url("https://www.youtube.com/live/abcDEF12345?feature=share").as_deref(), Some("abcDEF12345"));
        assert_eq!(id_from_url("https://example.com/video/123"), None); // no recognizable id
    }

    #[test]
    fn the_notable_flag_menu_is_well_formed() {
        assert!(!NOTABLE_FLAGS.is_empty());
        for (flag, blurb) in NOTABLE_FLAGS {
            assert!(flag.starts_with("--"), "{flag}");
            assert!(!blurb.is_empty(), "{flag} needs a blurb");
        }
    }
}
