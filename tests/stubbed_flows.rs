//! Offline end-to-end tests of failure handling and collection orchestration, driven by a
//! scripted yt-dlp stand-in (tests/fixtures/yt_dlp_stub.sh) placed first on the child's PATH.
//! Deterministic and network-free — these run in the default `cargo test`, and complement the
//! live tests in media_flags.rs / live_extended.rs rather than replacing them.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const YT_VIDEO: &str = "https://www.youtube.com/watch?v=stubvid0000";

/// One test's world: a scratch root holding a download dir, the stub's state dir, and a bin dir
/// with the stub installed as `yt-dlp`. Removed on drop.
struct Rig {
    root: PathBuf,
    into: PathBuf,
    stub: PathBuf,
    /// An extra dir for the child's PATH, for tests whose flow probes real media with ffprobe.
    extra_path: Option<PathBuf>,
}

impl Rig {
    fn new(tag: &str) -> Rig {
        let root = std::env::temp_dir().join(format!("vidl_stub_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (into, stub, bin) = (root.join("into"), root.join("stub"), root.join("bin"));
        for dir in [&into, &stub, &bin] {
            fs::create_dir_all(dir).unwrap();
        }
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/yt_dlp_stub.sh");
        let ytdlp = bin.join("yt-dlp");
        fs::copy(&script, &ytdlp).unwrap();
        fs::set_permissions(&ytdlp, fs::Permissions::from_mode(0o755)).unwrap();
        Rig { root, into, stub, extra_path: None }
    }

    /// Run `vidl <url> --into <rig into> <flags…>` with the rig's PATH/stub environment.
    fn dl(&self, mode: &str, url: &str, flags: &[&str]) -> Output {
        let extra = self
            .extra_path
            .as_ref()
            .map(|dir| format!("{}:", dir.display()))
            .unwrap_or_default();
        let path = format!(
            "{}:{extra}{}",
            self.root.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new(env!("CARGO_BIN_EXE_vidl"))
            .arg(url)
            .arg("--into")
            .arg(&self.into)
            .args(flags)
            .env("PATH", path)
            .env("VIDL_STUB_DIR", &self.stub)
            .env("VIDL_STUB_MODE", mode)
            .output()
            .expect("run vidl")
    }

    /// Every yt-dlp invocation the run made, one argv per line.
    fn calls(&self) -> String {
        fs::read_to_string(self.stub.join("calls.log")).unwrap_or_default()
    }

    /// An empty Netscape cookie jar inside the rig, as a string for the arg list. Its contents
    /// never matter — what the failure advice keys off is whether cookies were *tried*.
    fn jar(&self) -> String {
        let jar = self.root.join("cookies.txt");
        if !jar.exists() {
            fs::write(&jar, "# Netscape HTTP Cookie File\n").unwrap();
        }
        jar.display().to_string()
    }

    fn ledger(&self) -> String {
        fs::read_to_string(self.into.join(".dl_video_failed_download.txt")).unwrap_or_default()
    }

    /// The shortened-name record inside `sub` (a collection folder, or `""` for the destination
    /// root). Empty when there is no record — which is the normal case.
    fn modified(&self, sub: &str) -> String {
        fs::read_to_string(self.into.join(sub).join(".dl_video_modified")).unwrap_or_default()
    }

    /// Script one successful download's own account of itself: the id, the file it landed at
    /// (named relative to the destination), and the title in full — exactly the three fields
    /// yt-dlp's `--print-to-file after_move:` writes. The stub creates that file too.
    fn names(&self, id: &str, file: &str, title: &str) {
        let line = format!("{id}\t{}\t{title}", self.into.join(file).display());
        fs::write(self.stub.join("names_adds.txt"), line).unwrap();
    }

    /// Script what a `--multi` page scan finds: the page's own text, and the id yt-dlp resolves it
    /// to. The stub writes the page into its working directory the way `--write-pages` does.
    fn page(&self, resolved: &str, body: &str) {
        fs::write(self.stub.join("page.txt"), body).unwrap();
        fs::write(self.stub.join("page_id.txt"), format!("{resolved}\n")).unwrap();
    }

    /// Have the first page fetch refused, as a cold client's is.
    fn refuse_first_scan(&self) {
        fs::write(self.stub.join("refuse_scan_once"), "").unwrap();
    }

    /// The names of everything sitting in the destination root, bookkeeping dotfiles aside.
    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.into)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|name| !name.starts_with('.'))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

/// A title longer than any of the naming strategies can keep, built rather than pasted so the test
/// reads as the boundary it is testing. It has to clear the roomiest of them — the generic path's
/// rename, which seats a title in everything the filesystem allows bar the id and extension.
fn overlong_title() -> String {
    let title = format!("A caption that keeps going {}", "and going ".repeat(40));
    assert!(title.len() > 300, "the fixture must exceed every strategy's room");
    title
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn text(out: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

/// The *download* invocations among a run's calls, told apart by the one flag only a download
/// carries: `--output`. Not by the absence of `--print` — every download argv now also carries
/// `--print-to-file`, which is how a run learns the name each file was given.
fn downloads(calls: &str) -> Vec<&str> {
    calls.lines().filter(|line| line.contains("--output")).collect()
}

/// The `--output` template of each download invocation. Read as its own value rather than by
/// searching the whole line: the record format legitimately mentions `%(title)s`, so a line-wide
/// match cannot tell a naming template from the bookkeeping riding beside it.
fn output_templates(calls: &str) -> Vec<String> {
    downloads(calls)
        .iter()
        .filter_map(|line| line.split_once("--output ")?.1.split(' ').next().map(str::to_string))
        .collect()
}

/// Every non-empty invocation in a run, for the paths whose contract is a specific *number* of them.
fn invocations(calls: &str) -> Vec<&str> {
    calls.lines().filter(|line| !line.trim().is_empty()).collect()
}

/// Skip-with-notice: `Some(None)` when PATH carries a usable ffmpeg+ffprobe (the only place this
/// crate looks for them by default), or `None` after a visible SKIP line when it doesn't. The
/// `Option<PathBuf>` inside is the dir to prepend to the child's PATH — always `None` here, kept
/// so the rig's `extra_path` plumbing has one shape.
fn ffmpeg_or_skip(test: &str) -> Option<Option<PathBuf>> {
    let works = ["ffmpeg", "ffprobe"].iter().all(|name| {
        Command::new(name).arg("-version").output().is_ok_and(|out| out.status.success())
    });
    if works {
        return Some(None);
    }
    eprintln!("SKIPPED {test}: no usable ffmpeg/ffprobe available");
    None
}

/// A tiny real mkv at `path` (needs [`ffmpeg_or_skip`] to have passed).
fn build_mkv(dir: Option<&Path>, path: &Path) {
    let ffmpeg = dir.map(|dir| dir.join("ffmpeg")).unwrap_or_else(|| PathBuf::from("ffmpeg"));
    let ok = Command::new(ffmpeg)
        .stdin(Stdio::null()) // see util::exec — concurrent ffmpegs race over the terminal
        .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", "color=c=blue:s=64x64:d=1"])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
        .arg(path)
        .status()
        .is_ok_and(|status| status.success());
    assert!(ok, "could not build a test mkv");
}

#[test]
fn a_youtube_geo_block_is_reported_as_ip_enforced_without_any_xff_sweep() {
    let rig = Rig::new("geo_ip");
    let out = rig.dl("geo", YT_VIDEO, &[]);
    assert!(!out.status.success(), "a dead geo-block must exit nonzero:\n{}", text(&out));
    let ledger = rig.ledger();
    assert!(
        ledger.contains("[stubvid0000]") && ledger.contains("geo-blocked (enforced by IP"),
        "honest IP-enforced line, keyed by id: {ledger}"
    );
    assert!(!ledger.contains("tried"), "no region list on the IP-enforced path: {ledger}");
    assert!(!rig.calls().contains("--xff"), "spoofing must not even be attempted:\n{}", rig.calls());
}

#[test]
fn a_generic_geo_block_sweeps_xff_regions_and_stops_at_the_first_win() {
    let rig = Rig::new("geo_xff");
    let out = rig.dl("geo_unless_xff_us", "https://media.example.com/v/1", &[]);
    assert!(out.status.success(), "the US spoof rescues the download:\n{}", text(&out));
    assert!(text(&out).contains("region US worked"), "{}", text(&out));
    let calls = rig.calls();
    assert_eq!(calls.matches("--xff").count(), 1, "stops at the first working region:\n{calls}");
    assert!(calls.contains("--xff US"), "US is the first region tried:\n{calls}");
    assert!(rig.ledger().is_empty(), "a rescued download must not be ledgered: {}", rig.ledger());
}

#[test]
fn a_finished_download_says_so_in_green() {
    let rig = Rig::new("done_line");
    let out = rig.dl("ok", YT_VIDEO, &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("stubvid0000: downloaded"),
        "the run must end with an explicit success, not yt-dlp's last postprocessor line: {}",
        text(&out)
    );
}

#[test]
fn a_run_clears_unfinishable_leftovers_from_the_destination_before_downloading() {
    // The litter an interrupted older run leaves behind: partial files named without a rendition,
    // which the current naming can never resume and so would otherwise sit there forever.
    let rig = Rig::new("stale_sweep");
    let stale = rig.into.join("1_niery1hs.mp4.part");
    let mine = rig.into.join("S6E01__1_aaaaaaaa.mp4.part");
    let theirs = rig.into.join("quarterly report.pdf.part");
    for path in [&stale, &mine, &theirs] {
        fs::write(path, vec![b'x'; 4096]).unwrap();
    }
    let out = rig.dl("ok", YT_VIDEO, &[]);
    assert!(out.status.success(), "{}", text(&out));
    let all = text(&out);
    assert!(!stale.exists(), "the unfinishable leftover goes: {:?}", rig.files());
    assert!(mine.exists(), "a resumable partial stays: {:?}", rig.files());
    assert!(theirs.exists(), "another program's partial stays: {:?}", rig.files());
    // Deleting a user's file silently would be indefensible — it has to be said, by full path and
    // with what it recovered.
    assert!(all.contains("unfinishable partial download"), "the sweep announces itself: {all}");
    assert!(all.contains("1_niery1hs.mp4.part"), "and names what it removed: {all}");
    assert!(all.contains("4.0 KiB"), "and what that recovered: {all}");
}

#[test]
fn a_transient_failure_succeeds_on_the_diagnostic_retry() {
    let rig = Rig::new("retry");
    let out = rig.dl("fail_once_then_ok", YT_VIDEO, &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("succeeded on retry"), "{}", text(&out));
    assert!(rig.ledger().is_empty(), "a retried success must not be ledgered: {}", rig.ledger());
}

#[test]
fn a_resume_the_server_refuses_restarts_the_file_instead_of_giving_up() {
    // An interrupted download leaves a `.part`. If the interrupt landed after the last byte, that
    // file is already as long as the remote one, so resuming asks for a range that cannot exist —
    // and gets a 416 every single time, which is why re-running the command never cleared it.
    let rig = Rig::new("range_refused");
    let out = rig.dl("range_refused_unless_restarted", YT_VIDEO, &[]);
    assert!(out.status.success(), "starting the file over rescues it: {}", text(&out));
    let all = text(&out);
    assert!(all.contains("cannot be resumed — starting it over"), "the restart is announced: {all}");
    // What it costs, in the units yt-dlp prints two lines above: 749233884 B = 714.5 MiB. Silently
    // re-fetching that much would be worse than saying so.
    assert!(all.contains("714.5 MiB already downloaded will be fetched again"), "the cost is stated: {all}");
    assert!(all.contains("succeeded after starting the file over"), "and the outcome: {all}");
    assert!(rig.ledger().is_empty(), "a rescued download must not be ledgered: {}", rig.ledger());
    let calls = rig.calls();
    assert_eq!(
        calls.matches("--no-continue").count(),
        1,
        "only the restart carries it — resuming stays the default for genuinely partial files:\n{calls}"
    );
}

#[test]
fn a_restart_that_clears_the_part_and_then_fails_blames_the_real_error() {
    // Found by running it for real: the resume was refused, starting over fetched the whole file,
    // and the run still failed — because ffmpeg was missing. Telling the user to delete a `.part`
    // the restart has already consumed would send them after the wrong thing entirely.
    let rig = Rig::new("range_refused_then_other");
    let out = rig.dl("range_refused_then_other", YT_VIDEO, &[]);
    assert!(!out.status.success(), "it did fail, and must say so: {}", text(&out));
    let ledger = rig.ledger();
    assert!(ledger.contains("started over"), "the range refusal is still explained: {ledger}");
    assert!(ledger.contains("ffmpeg not found"), "but the REAL cause is named: {ledger}");
    assert!(
        !ledger.contains("delete the leftover"),
        "and no advice to delete a file the restart already consumed: {ledger}"
    );
}

#[test]
fn gated_failures_land_honest_cookie_aware_ledger_lines() {
    // Members-only: no retry pretence, just the release-later reality.
    let rig = Rig::new("members");
    rig.dl("members", YT_VIDEO, &[]);
    assert!(rig.ledger().contains("members-only"), "{}", rig.ledger());

    // Age gate, no cookies anywhere: the fix (import) is named.
    let rig = Rig::new("age_plain");
    rig.dl("age", YT_VIDEO, &[]);
    assert!(
        rig.ledger().contains("needs cookies from a signed-in 18+ account"),
        "{}",
        rig.ledger()
    );

    // Age gate WITH cookies in play: "add cookies" would be a lie — the harder-gate wording
    // must appear instead. The jar below is empty, which is fine: only its presence on the argv
    // decides the wording, and the stub never reads it.
    let rig = Rig::new("age_cookies");
    rig.dl("age", YT_VIDEO, &["--cookies", &rig.jar()]);
    assert!(rig.ledger().contains("age-restricted despite cookies"), "{}", rig.ledger());

    // Bot-wall: honest that it may be undownloadable.
    let rig = Rig::new("botwall");
    rig.dl("botwall", YT_VIDEO, &[]);
    assert!(rig.ledger().contains("anti-bot/CAPTCHA"), "{}", rig.ledger());

    // DRM, no cookies: the tv-client quirk makes "import cookies" a genuine first fix.
    let rig = Rig::new("drm_plain");
    rig.dl("drm", YT_VIDEO, &[]);
    assert!(rig.ledger().contains("cookies sometimes unlock non-DRM"), "{}", rig.ledger());

    // DRM with cookies in play: terminal — no advice the user already took, no retry pretence.
    let rig = Rig::new("drm_cookies");
    rig.dl("drm", YT_VIDEO, &["--cookies", &rig.jar()]);
    assert!(rig.ledger().contains("DRM-protected even with cookies"), "{}", rig.ledger());
}

#[test]
fn patch_flags_on_a_non_youtube_site_name_the_supported_platforms_and_skip() {
    let rig = Rig::new("notice");
    let out = rig.dl("ok", "https://media.example.com/v/2", &["--thumbnail", "--subtitles"]);
    assert!(out.status.success(), "{}", text(&out));
    let all = text(&out);
    assert!(all.contains("supported platforms: youtube"), "names what IS supported: {all}");
    assert!(
        !all.contains("thumbnails: scanning") && !all.contains("subtitles: scanning"),
        "the passes themselves must not run off-platform: {all}"
    );
}

#[test]
fn a_playlist_scan_reports_tombstones_skips_archived_and_downloads_pending_in_one_group() {
    let rig = Rig::new("playlist");
    // Four entries: one pending, one tombstone, one already archived, one more pending — the
    // two pending share a subtitle plan, so they must download as ONE grouped invocation.
    fs::write(
        rig.stub.join("scan.txt"),
        "1\tvidpend0001\tKeep One\tStub List\n\
         2\tvidpriv0002\t[Private video]\tStub List\n\
         3\tvidarch0003\tAlready Have\tStub List\n\
         4\tvidpend0004\tKeep Two\tStub List\n\
         Stub List [PLstub]\n",
    )
    .unwrap();
    fs::write(
        rig.stub.join("probe.txt"),
        "1\tvidpend0001\ten\t{\"en\": [{\"name\": \"English\"}]}\t{}\n\
         4\tvidpend0004\ten\t{\"en\": [{\"name\": \"English\"}]}\t{}\n",
    )
    .unwrap();
    let home = rig.into.join("Stub List [PLstub]");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join(".dl_video_archive.txt"), "youtube vidarch0003\n").unwrap();
    // What a successful group download archives (the real yt-dlp marks each completed entry) —
    // the post-mortem reads this to know nothing needs a diagnostic re-run.
    fs::write(rig.stub.join("archive_adds.txt"), "youtube vidpend0001\nyoutube vidpend0004\n")
        .unwrap();

    let out = rig.dl("ok", "https://www.youtube.com/playlist?list=PLstub", &[]);
    assert!(out.status.success(), "{}", text(&out));
    let all = text(&out);
    assert!(all.contains("1 of 4 entries are unplayable"), "{all}");
    let report = fs::read_to_string(home.join("unplayable__PLstub.txt")).expect("report written");
    assert!(report.contains("vidpriv0002"), "the tombstone is traced by id: {report}");
    assert!(all.contains("2 entries already archived (or unplayable) — skipped"), "{all}");
    assert!(all.contains("probing subtitles of 2 entries"), "{all}");
    assert!(all.contains("downloading 2 entries (subs: en)"), "{all}");
    let calls = rig.calls();
    assert_eq!(
        calls.lines().filter(|line| line.contains("--flat-playlist")).count(),
        1,
        "one scan:\n{calls}"
    );
    let downloads = downloads(&calls);
    assert_eq!(downloads.len(), 1, "the shared subtitle plan downloads as one group:\n{calls}");
    assert!(downloads[0].contains("--playlist-items 1,4"), "both pending entries: {}", downloads[0]);
    // Two entries sit far under the pacing threshold — the probe must not sleep between requests.
    let probe = calls.lines().find(|l| l.contains("--print") && l.contains("--playlist-items"));
    assert!(!probe.unwrap().contains("--sleep-requests"), "small probes must not pace:\n{calls}");
}

#[test]
fn a_playlist_refused_on_the_first_ask_still_gets_the_full_scanned_treatment() {
    // A refused scan used to cost the whole scan: the playlist fell back to one undifferentiated
    // pass with EN-only subtitles and no unplayable report. That is far too much to lose to a
    // request that works when asked again a moment later.
    let rig = Rig::new("playlist_scan_retry");
    fs::write(
        rig.stub.join("scan.txt"),
        "1\tvidpend0001\tKeep One\tStub List\n\
         2\tvidpriv0002\t[Private video]\tStub List\n\
         Stub List [PLstub]\n",
    )
    .unwrap();
    fs::write(
        rig.stub.join("probe.txt"),
        "1\tvidpend0001\the\t{\"he\": [{\"name\": \"Hebrew\"}]}\t{}\n",
    )
    .unwrap();
    fs::write(rig.stub.join("archive_adds.txt"), "youtube vidpend0001\n").unwrap();
    rig.refuse_first_scan();

    let out = rig.dl("ok", "https://www.youtube.com/playlist?list=PLstub", &[]);
    assert!(out.status.success(), "{}", text(&out));
    let all = text(&out);
    assert!(all.contains("asking again"), "the retry is announced: {all}");
    assert!(all.contains("the playlist came through on attempt 2"), "and its success: {all}");
    assert!(
        !all.contains("could not scan the playlist"),
        "the degraded one-pass fallback must NOT be reached: {all}"
    );
    // The three things the fallback would have cost, all present: the unplayable report, the
    // per-entry subtitle plan (`he`, which EN-only would have missed), and the archive skip list.
    assert!(all.contains("1 of 2 entries are unplayable"), "{all}");
    assert!(all.contains("downloading 1 entry (subs: he)"), "the scanned plan, not EN-only: {all}");
    let home = rig.into.join("Stub List [PLstub]");
    assert!(home.join("unplayable__PLstub.txt").is_file(), "report written: {:?}", rig.files());
    let calls = rig.calls();
    assert_eq!(
        calls.lines().filter(|line| line.contains("--flat-playlist")).count(),
        2,
        "asked twice, not more:\n{calls}"
    );
}

#[test]
fn a_channel_walks_every_tab_downloading_reporting_missing_and_failed_ones() {
    let rig = Rig::new("channel");
    // The stub keys tab behaviour off the URL: videos → this scan; shorts → yt-dlp's "no such
    // tab" error; streams → a hard failure; playlists → reachable but empty (no
    // scan_playlists.txt). One run exercises every TabScan outcome.
    fs::write(
        rig.stub.join("scan.txt"),
        "1\tvidchan0001\tClip One\tStub Channel\n\
         2\tvidchan0002\tClip Two\tStub Channel\n\
         Stub Channel [UCstub]\n",
    )
    .unwrap();
    fs::write(
        rig.stub.join("probe.txt"),
        "1\tvidchan0001\ten\t{\"en\": [{\"name\": \"English\"}]}\t{}\n\
         2\tvidchan0002\ten\t{\"en\": [{\"name\": \"English\"}]}\t{}\n",
    )
    .unwrap();
    fs::write(rig.stub.join("archive_adds.txt"), "youtube vidchan0001\nyoutube vidchan0002\n")
        .unwrap();

    let out = rig.dl("ok", "https://www.youtube.com/@stubchannel", &[]);
    assert!(out.status.success(), "one good tab means a good run: {}", text(&out));
    let all = text(&out);
    for tab in ["videos", "shorts", "streams", "playlists"] {
        assert!(
            all.contains(&format!("=== https://www.youtube.com/@stubchannel/{tab} ===")),
            "every tab is announced: {all}"
        );
    }
    assert!(all.contains("the channel has no `shorts` tab"), "{all}");
    assert_eq!(
        all.matches("Unable to download webpage").count(),
        1,
        "a failed tab's real error passes through — ONCE, not once per attempt: {all}"
    );
    assert!(all.contains("could not read the `streams` tab — moving on"), "{all}");
    assert!(all.contains("the channel has no `playlists` tab"), "reachable-but-empty reads as absent: {all}");
    assert!(all.contains("downloading 2 entries (subs: en)"), "{all}");
    // The channel's own folder (named by the first readable tab's scan) holds the shared archive.
    let archive = fs::read_to_string(rig.into.join("Stub Channel [UCstub]/.dl_video_archive.txt"))
        .expect("archive under the channel home");
    assert!(archive.contains("vidchan0001") && archive.contains("vidchan0002"), "{archive}");
    let calls = rig.calls();
    // One scan per tab — except the tab that fails HARD, which could be a cold client's first
    // request being refused and so is asked again. "No such tab" is an answer, not a refusal, and
    // asking three times would not change it.
    let scans = |tab: &str| {
        calls
            .lines()
            .filter(|line| line.contains("--flat-playlist") && line.ends_with(tab))
            .count()
    };
    assert_eq!(scans("/videos"), 1, "a tab that answers is asked once:\n{calls}");
    assert_eq!(scans("/shorts"), 1, "`no such tab` is an answer, not a refusal:\n{calls}");
    assert_eq!(scans("/playlists"), 1, "reachable-but-empty likewise:\n{calls}");
    assert_eq!(scans("/streams"), 3, "a hard failure is asked again:\n{calls}");
    let downloads = downloads(&calls);
    assert_eq!(downloads.len(), 1, "the videos tab downloads as one group:\n{calls}");
}

#[test]
fn a_probe_over_the_pacing_threshold_sleeps_between_requests() {
    let rig = Rig::new("pacing");
    // 21 pending entries (threshold is 20): a metadata burst that big paces itself.
    let mut scan = String::new();
    let mut archive_adds = String::new();
    for i in 1..=21 {
        scan.push_str(&format!("{i}\tvidpend{i:04}\tEntry {i}\tBig List\n"));
        archive_adds.push_str(&format!("youtube vidpend{i:04}\n"));
    }
    scan.push_str("Big List [PLbig]\n");
    fs::write(rig.stub.join("scan.txt"), scan).unwrap();
    fs::write(rig.stub.join("probe.txt"), "").unwrap(); // probe answers nothing → EN fallback
    fs::write(rig.stub.join("archive_adds.txt"), archive_adds).unwrap();

    let out = rig.dl("ok", "https://www.youtube.com/playlist?list=PLbig", &[]);
    assert!(out.status.success(), "{}", text(&out));
    let calls = rig.calls();
    let probe = calls
        .lines()
        .find(|line| line.contains("--print") && line.contains("--playlist-items"))
        .expect("a batch probe ran");
    assert!(probe.contains("--sleep-requests 1"), "big probes must pace themselves:\n{probe}");
}


#[test]
fn the_subtitle_pass_reports_audio_files_as_tag_carriers_and_skips_them() {
    let rig = Rig::new("subs_audio");
    fs::write(rig.into.join("song__stubvid0000.opus"), b"OPUSDATA").unwrap();
    let out = rig.dl("ok", YT_VIDEO, &["--subtitles"]);
    assert!(out.status.success(), "{}", text(&out));
    let all = text(&out);
    assert!(all.contains("already downloaded — scanning subtitles"), "{all}");
    assert!(all.contains("audio — subtitles kept as tags; nothing to patch"), "{all}");
    assert_eq!(
        fs::read(rig.into.join("song__stubvid0000.opus")).unwrap(),
        b"OPUSDATA",
        "the audio file stays untouched"
    );
}

#[test]
fn an_unreadable_video_file_is_reported_and_left_alone() {
    let rig = Rig::new("subs_junk");
    fs::write(rig.into.join("v__stubvid0000.mkv"), b"not really an mkv").unwrap();
    let out = rig.dl("ok", YT_VIDEO, &["--subtitles"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("could not read (ffprobe) — skipping"), "{}", text(&out));
    assert_eq!(
        fs::read(rig.into.join("v__stubvid0000.mkv")).unwrap(),
        b"not really an mkv",
        "an unreadable file must not be modified"
    );
}

#[test]
fn a_subtitle_fetch_that_yields_no_sidecars_is_reported_as_rate_limited() {
    let Some(ffmpeg_dir) = ffmpeg_or_skip("no-sidecars branch") else { return };
    let mut rig = Rig::new("subs_dry");
    rig.extra_path = ffmpeg_dir.clone();
    build_mkv(ffmpeg_dir.as_deref(), &rig.into.join("v__stubvid0000.mkv"));
    // The probe advertises an `en` track, but the stub's "download" writes no sidecar files.
    fs::write(
        rig.stub.join("video_probe.txt"),
        "stubvid0000\nen\n{\"en\": [{\"name\": \"English\"}]}\n{}\n",
    )
    .unwrap();
    let out = rig.dl("ok", YT_VIDEO, &["--subtitles"]);
    assert!(out.status.success(), "{}", text(&out));
    let all = text(&out);
    assert!(all.contains("missing subtitle(s): en"), "{all}");
    assert!(all.contains("no subtitles arrived (rate-limited?)"), "{all}");
}

#[test]
fn a_video_with_no_subtitles_anywhere_reads_as_nothing_to_embed_not_all_zero() {
    let Some(ffmpeg_dir) = ffmpeg_or_skip("no-subtitles wording") else { return };
    let mut rig = Rig::new("subs_none");
    rig.extra_path = ffmpeg_dir.clone();
    build_mkv(ffmpeg_dir.as_deref(), &rig.into.join("v__stubvid0000.mkv"));
    // Default probe: NA everywhere → the expected set is empty.
    let out = rig.dl("ok", YT_VIDEO, &["--subtitles"]);
    assert!(out.status.success(), "{}", text(&out));
    let all = text(&out);
    assert!(all.contains("no subtitles exist for this video — nothing to embed"), "{all}");
    assert!(!all.contains("all 0 expected"), "the absurd wording must be gone: {all}");
}

/// The host, not the path, decides which pipeline a URL takes — and a non-YouTube URL must reach
/// the *generic* one. Worth an end-to-end test because the two paths diverge early and quietly:
/// routing a normal site down the YouTube path still downloads something, it just does it with a
/// pointless subtitle probe and a filename template built for a site this isn't.
#[test]
fn a_non_youtube_url_takes_the_flat_generic_path_not_the_youtube_one() {
    let rig = Rig::new("generic_route");
    let out = rig.dl("ok", "https://media.example.com/v/2", &[]);
    assert!(out.status.success(), "{}", text(&out));
    let calls = rig.calls();

    assert!(calls.contains("--no-playlist"), "the generic path pins one video:\n{calls}");
    // Off YouTube the download goes to a name that cannot overrun — the id alone — and the real
    // name is put on afterwards, once the title and extension are known rather than guessed.
    let templates = output_templates(&calls);
    assert_eq!(templates.len(), 1, "one download, one template: {templates:?}");
    assert!(templates[0].contains("%(id)."), "named by its id: {}", templates[0]);
    assert!(
        !templates[0].contains("%(title)"),
        "and not by a title whose length nothing has measured yet: {}",
        templates[0]
    );
    assert!(
        !calls.contains("%(upload_date)s"),
        "the YouTube filename template must not appear off-platform:\n{calls}"
    );
    assert!(
        !calls.contains("--write-subs") && !calls.contains("--sub-langs"),
        "the caption matrix is a YouTube feature — no subtitle requests here:\n{calls}"
    );
    // No metadata probe off YouTube: the generic path is ONE invocation end to end. Asserted as a
    // count rather than "no --print", because the download argv legitimately carries
    // `--print-to-file` — which is precisely how the name record avoids costing a second request.
    assert_eq!(
        invocations(&calls).len(),
        1,
        "the generic path downloads in one invocation:\n{calls}"
    );
    assert!(
        !calls.contains("--print %("),
        "and it must not run a standalone metadata print:\n{calls}"
    );
    assert!(
        !text(&out).contains("probing subtitles"),
        "and it must not announce a probe it never runs:\n{}",
        text(&out)
    );
}

/// The bug this machinery exists for, end to end. Off YouTube the "title" is routinely a whole
/// post caption, and an unbounded one produced a file name of 648 bytes against a 255-byte limit —
/// the download failed outright with `File name too long`. Now the download goes to a name that
/// cannot overrun and the real one is put on afterwards, so it succeeds; a caption that long still
/// won't fit, so the run says so in red and keeps the title in full beside the media.
#[test]
fn a_title_too_long_for_the_filesystem_is_cut_on_rename_announced_and_recorded_in_full() {
    let rig = Rig::new("long_name");
    let title = overlong_title();
    rig.names("genlong00001", "genlong00001.mp4", &title);

    let out = rig.dl("ok", "https://media.example.com/v/3", &[]);
    assert!(out.status.success(), "a long title must no longer fail the download:\n{}", text(&out));

    // Downloaded under its id, then renamed to carry as much of the title as fits.
    let files = rig.files();
    assert_eq!(files.len(), 1, "one file, renamed in place: {files:?}");
    let name = &files[0];
    assert!(name.starts_with("A caption that keeps going"), "the title leads the name: {name}");
    assert!(name.ends_with(" [genlong00001].mp4"), "and the id closes it: {name}");
    assert!(name.len() <= 255, "{} bytes overruns the limit: {name}", name.len());

    // The whole point of renaming instead of bounding a template: by now the id and extension are
    // known and yt-dlp's working suffixes are gone, so the title gets nearly the entire component
    // rather than what a template had to reserve against every unknown.
    let kept = name.len() - " [genlong00001].mp4".len();
    assert!(kept > 200, "the rename should seat ~236 bytes of title, kept only {kept}");

    // Announced in red as it happens, and the full title survives in the record.
    let all = text(&out);
    assert!(all.contains("genlong00001"), "the notice names the video: {all}");
    assert!(all.contains(&format!("{} bytes", title.len())), "and how long its title was: {all}");
    assert!(all.contains("\u{1b}[1;31m"), "the notice must be red: {all:?}");
    let record = rig.modified("");
    assert!(record.contains(&format!("genlong00001\t{title}")), "the title in full: {record}");
    assert_eq!(record.lines().filter(|l| l.contains('\t')).count(), 1, "one entry: {record}");

    // The scratch yt-dlp wrote is consumed, not left beside the media as a second bookkeeping file.
    assert!(!rig.into.join(".dl_video_names").exists(), "the scratch must not survive the run");
}

/// The other half of the contract, and the one that keeps the record useful: a title that fits
/// leaves no trace at all. A file recording every download would bury the handful of answers it
/// exists to give.
#[test]
fn a_title_that_fits_is_downloaded_without_a_word_about_names() {
    let rig = Rig::new("short_name");
    rig.names("genfine00001", "genfine00001.mp4", "A perfectly ordinary title");

    let out = rig.dl("ok", "https://media.example.com/v/4", &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        rig.files(),
        ["A perfectly ordinary title [genfine00001].mp4"],
        "a title that fits is seated whole"
    );
    assert!(rig.modified("").is_empty(), "nothing was cut, so nothing is recorded");
    assert!(
        !text(&out).contains("shortened"),
        "and nothing is said about it: {}",
        text(&out)
    );
    assert!(!rig.into.join(".dl_video_names").exists(), "the scratch is consumed regardless");
}

/// A collection's record belongs beside its media, not at the destination root — the same rule the
/// download archive and the failure ledger follow, so everything this crate knows about one
/// collection is in one folder.
#[test]
fn a_collections_shortened_names_are_recorded_beside_its_own_media() {
    let rig = Rig::new("long_name_playlist");
    fs::write(
        rig.stub.join("scan.txt"),
        "1\tvidlong0001\tClip One\tStub List\n\
         Stub List [PLstub]\n",
    )
    .unwrap();
    fs::write(rig.stub.join("probe.txt"), "1\tvidlong0001\ten\t{\"en\": [{\"name\": \"English\"}]}\t{}\n")
        .unwrap();
    fs::write(rig.stub.join("archive_adds.txt"), "youtube vidlong0001\n").unwrap();
    let title = overlong_title();
    rig.names("vidlong0001", "Stub List [PLstub]/1.mkv", &title);

    let out = rig.dl("ok", "https://www.youtube.com/playlist?list=PLstub", &[]);
    assert!(out.status.success(), "{}", text(&out));
    let record = rig.modified("Stub List [PLstub]");
    assert!(record.contains(&format!("vidlong0001\t{title}")), "recorded in the collection: {record}");
    assert!(rig.modified("").is_empty(), "and not at the destination root: {}", rig.modified(""));
}

/// The safety net. With the title bounded, a name the filesystem still refuses means the length is
/// coming from somewhere this crate does not shorten — so the diagnosis must say that instead of
/// dumping the offending 650-byte path and calling it advice.
#[test]
fn a_name_the_filesystem_refuses_is_diagnosed_as_a_path_length_not_a_site_problem() {
    let rig = Rig::new("nametoolong");
    let out = rig.dl("nametoolong", "https://media.example.com/v/5", &[]);
    assert!(!out.status.success(), "a refused name is a real failure:\n{}", text(&out));

    let ledger = rig.ledger();
    assert!(ledger.contains("255-byte"), "names the limit in force: {ledger}");
    assert!(ledger.contains("--into"), "and where the length can come from: {ledger}");
    assert!(ledger.contains("--name-max"), "and the flag for a smaller real limit: {ledger}");
    assert!(ledger.contains("%(title)."), "showing what a bounded title looks like: {ledger}");
    // No shorter attempt is offered here, and the line must not pretend one was: off YouTube the
    // download is already named by its id, so a refusal was never about the title and shrinking
    // one would send the reader after the wrong thing.
    assert!(
        !text(&out).contains("retrying with a shorter one"),
        "there is no title bound to shrink on this path:\n{}",
        text(&out)
    );
    assert!(!ledger.contains("halved"), "so the advice must not claim one was tried: {ledger}");
    assert!(
        !ledger.to_lowercase().contains("retry"),
        "the same name is refused every time — never suggest a retry: {ledger}"
    );
    // It must not be mistaken for one of the site-side gates, whose advice is all about cookies.
    assert!(!ledger.contains("cookie"), "a local failure needs no cookie advice: {ledger}");
    // And it must not sweep regions: nothing about a geo-block is involved.
    assert!(!rig.calls().contains("--xff"), "no geo rescue for a local failure:\n{}", rig.calls());
}

/// The title bound each download invocation asked for, in order — the number inside
/// `%(title).<n>B`. What the retry logic changes, so it is what a test on that logic reads.
fn title_bounds(calls: &str) -> Vec<usize> {
    downloads(calls)
        .iter()
        .filter_map(|line| {
            let rest = line.split_once("%(title).")?.1;
            rest.split_once('B')?.0.parse().ok()
        })
        .collect()
}

/// The recoverable half of a refused name. The budget cannot defend against every compounding
/// worst case — a format id longer than reserved, heavy sanitizer widening, an unusual mount — so
/// when the filesystem does refuse a name, one shorter attempt turns a lost download into a file
/// with an uglier name. That is worth far more than a wider default budget.
#[test]
fn a_refused_name_earns_one_shorter_attempt_and_the_download_lands() {
    let rig = Rig::new("nametoolong_retry");
    let out = rig.dl("nametoolong_unless_shorter", YT_VIDEO, &[]);
    assert!(out.status.success(), "the shorter name must rescue the download:\n{}", text(&out));

    let all = text(&out);
    assert!(all.contains("retrying with a shorter one"), "the attempt is announced: {all}");
    assert!(all.contains("succeeded with a shortened name"), "and so is the rescue: {all}");
    assert!(rig.ledger().is_empty(), "a rescued download must not be ledgered: {}", rig.ledger());

    // The retry must actually have asked for less. The stub refuses on the argv alone, so the
    // plain diagnostic re-run of the unchanged name was refused too — nothing but a genuinely
    // shorter template could have got through.
    let bounds = title_bounds(&rig.calls());
    assert!(bounds.len() >= 3, "first attempt, plain re-run, then the shorter one: {bounds:?}");
    assert_eq!(bounds[0], bounds[1], "the diagnostic re-run repeats the name verbatim");
    assert_eq!(*bounds.last().unwrap(), bounds[0] / 2, "and the rescue halves it: {bounds:?}");
}

/// `--name-max` exists for a destination that accepts less than it reports — an encrypted-filename
/// mount, typically. It has to reach the *budget*, not just be accepted as a flag, or it does
/// nothing: a smaller limit must produce a smaller title bound in the template yt-dlp is handed.
#[test]
fn a_smaller_name_max_shrinks_the_title_bound_it_asks_yt_dlp_for() {
    let wide = Rig::new("namemax_default");
    wide.dl("ok", YT_VIDEO, &[]);
    let wide_bound = *title_bounds(&wide.calls()).first().expect("a bounded template");

    let narrow = Rig::new("namemax_143");
    let out = narrow.dl("ok", YT_VIDEO, &["--name-max", "143"]);
    let narrow_bound = *title_bounds(&narrow.calls()).first().expect("a bounded template");

    assert!(
        narrow_bound < wide_bound,
        "--name-max 143 must tighten the title bound, not sit beside it ({narrow_bound} vs {wide_bound})"
    );
    // And the difference is exactly the difference in limits: the budget is derived from it, so
    // every byte taken off the limit comes off the title.
    assert_eq!(wide_bound - narrow_bound, 255 - 143, "the budget must track the limit byte for byte");
    assert!(
        text(&out).contains("limited to 143 bytes"),
        "a non-default limit is worth saying out loud: {}",
        text(&out)
    );
}

/// A page in the shape `--multi` exists for: a homogeneous list of entries behind one URL, plus the
/// player script whose host carries the account the media actually lives under. Synthetic — what
/// matters is the structure, which is what the sweep reads.
fn collection_page(ids: &[&str]) -> String {
    let titles: Vec<String> = (0..ids.len()).map(|i| format!("Entry {i}")).collect();
    let refs: Vec<&str> = titles.iter().map(String::as_str).collect();
    collection_page_titled(ids, &refs)
}

/// The same page, with the entry titles named explicitly — for the ordering cases.
fn collection_page_titled(ids: &[&str], titles: &[&str]) -> String {
    let mut page = String::from("<html><head><title>Some Series, Season 6</title></head><body>");
    // The site's own account, declared and repeated — the decoy that must not win.
    page.push_str(r#"<script>{"partner_id":"5031"}</script>"#);
    for _ in 0..4 {
        page.push_str(r#"<img src="https://cdn.site.test/image/p/5031/thumb.jpg">"#);
    }
    page.push_str(r#"<script src="https://cdnapisec.kaltura.com/p/2748741/embedPlaykitJs/x">"#);
    page.push_str(r#"<script>{"items":["#);
    for (title, id) in titles.iter().zip(ids) {
        page.push_str(&format!(
            r#"{{"title":"{title}","description":"a synopsis","entryId":"{id}"}},"#
        ));
    }
    page.push_str("]}</script></body></html>");
    page
}

/// The whole feature end to end: a page that resolves to one video but lists several is collected
/// into a folder of its own, in the order the page lists them, addressed through the account the
/// player script names rather than the one the page declares about itself.
#[test]
fn a_page_hiding_a_collection_is_walked_into_a_folder_in_page_order() {
    let rig = Rig::new("multi_collection");
    let ids = ["1_aaaaaaaa", "1_bbbbbbbb", "1_cccccccc"];
    rig.page(ids[0], &collection_page(&ids));

    let out = rig.dl("ok", "https://media.example.com/series/42/", &["--multi"]);
    assert!(out.status.success(), "{}", text(&out));
    let all = text(&out);
    assert!(all.contains("lists 3 Kaltura videos"), "the count is announced: {all}");
    // These entries carry their own numbers, so those are what the files are numbered by rather
    // than the order the page happened to list them in. The run says which it used, and these
    // names carry no episode *word*, so the fallback is what should be announced.
    assert!(
        all.contains("numbering them by the last number in each name"),
        "the numbering, and how it was arrived at, is announced: {all}"
    );

    // A folder of its own, named for what the entries have in common plus the page's identifier —
    // the shape a playlist gets. The entries here are titled "Entry 0/1/2", so the opening they
    // share is "Entry"; the page's own <title> loses to it, which is the whole point of the
    // layering (on a real page that title is the *featured* episode).
    let home = rig.into.join("Entry[42]");
    assert!(home.is_dir(), "the collection needs a folder: {:?}", rig.files());
    assert!(
        !rig.into.join("Some Series, Season 6[42]").exists(),
        "the page title must not win while the entries share an opening"
    );

    // One download per entry, addressed through the player's account (2748741) and NOT the one the
    // page declares as its own (5031), which Kaltura rejects.
    let calls = rig.calls();
    let downloads = downloads(&calls);
    assert_eq!(downloads.len(), 3, "one download each:\n{calls}");
    for (position, id) in ids.iter().enumerate() {
        let target = format!("kaltura:2748741:{id}");
        assert!(
            downloads[position].contains(&target),
            "entry {} must be #{} in page order, got: {}",
            id,
            position + 1,
            downloads[position]
        );
    }
    assert!(!rig.calls().contains("kaltura:5031:"), "the page's own account must not be used");
    // And every file lands in the collection's folder, not the destination root.
    for line in &downloads {
        assert!(line.contains("Entry[42]"), "not under the folder: {line}");
    }
}

/// The bug this cost a user two runs of the same command: the page's first fetch was refused with a
/// 403 and the download that followed it, to the same URL, was not — so the collection was quietly
/// abandoned and one video came back. One refusal must not mean "this is not a collection".
#[test]
fn a_page_refused_on_the_first_ask_is_asked_again_rather_than_abandoned() {
    let rig = Rig::new("multi_scan_retry");
    let ids = ["1_aaaaaaaa", "1_bbbbbbbb", "1_cccccccc"];
    rig.page(ids[0], &collection_page(&ids));
    rig.refuse_first_scan();

    let out = rig.dl("ok", "https://media.example.com/series/42/", &["--multi"]);
    assert!(out.status.success(), "{}", text(&out));
    let all = text(&out);
    assert!(all.contains("asking again"), "the retry is announced: {all}");
    assert!(all.contains("came through on attempt 2"), "and so is its success: {all}");

    // And the collection really was walked — not the single-video fallback the refusal used to
    // cause. Three downloads, into the folder, exactly as an unrefused page gives.
    assert!(rig.into.join("Entry[42]").is_dir(), "the folder: {:?}", rig.files());
    let calls = rig.calls();
    assert_eq!(downloads(&calls).len(), 3, "one download per entry:\n{calls}");
    assert_eq!(
        calls.matches("--write-pages").count(),
        2,
        "asked twice, not more:\n{calls}"
    );
}

/// The promise of the flag. A page with one video on it must come back through the ordinary path,
/// untouched — that is what makes `--multi` safe to add to a tool that already works.
#[test]
fn multi_on_an_ordinary_page_falls_back_to_the_single_download() {
    let rig = Rig::new("multi_single");
    rig.page("1_aaaaaaaa", &collection_page(&["1_aaaaaaaa"]));

    let out = rig.dl("ok", "https://media.example.com/v/9", &["--multi"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("nothing for --multi to collect"),
        "it must say so rather than silently doing nothing: {}",
        text(&out)
    );
    // No folder, and the download is the plain one: named by its id, straight under `into`.
    assert!(rig.files().is_empty() || !rig.into.join("Some Series, Season 6[9]").exists());
    let templates = output_templates(&rig.calls());
    assert_eq!(templates.len(), 1, "exactly one download: {templates:?}");
    assert!(templates[0].contains("%(id)."), "the ordinary generic template: {}", templates[0]);
}

/// `--multi` is for sites that give yt-dlp no structure to read. YouTube gives it plenty, and this
/// crate walks it — so the flag says it is being ignored rather than appearing to have done work.
#[test]
fn multi_is_refused_on_youtube_where_collections_are_already_walked() {
    let rig = Rig::new("multi_youtube");
    let out = rig.dl("ok", YT_VIDEO, &["--multi"]);
    assert!(out.status.success(), "{}", text(&out));
    let all = text(&out);
    assert!(all.contains("--multi is for sites"), "the refusal is explained: {all}");
    assert!(all.contains("playlists and channels"), "and points at what does work: {all}");
    // It really is inert: no page scan, and the ordinary lone-video flow ran.
    assert!(!rig.calls().contains("--write-pages"), "no page scan on YouTube:\n{}", rig.calls());
    assert!(all.contains("probing subtitles"), "the YouTube path is unchanged: {all}");
}

/// A look-alike path on somebody else's host must not be mistaken for a YouTube channel — the
/// classifier checks the host first, and this is that rule end to end. Getting it wrong would
/// send a channel walk (every tab, recursively) at an unrelated site.
#[test]
fn a_youtube_shaped_path_on_another_host_is_still_generic() {
    for url in ["https://media.example.com/@someone", "https://media.example.com/watch?v=x&list=PL1"] {
        let rig = Rig::new("lookalike");
        let out = rig.dl("ok", url, &[]);
        assert!(out.status.success(), "{}", text(&out));
        let calls = rig.calls();
        assert!(calls.contains("--no-playlist"), "{url} must download flat:\n{calls}");
        assert!(
            !calls.contains("/videos") && !calls.contains("--flat-playlist"),
            "{url} must not be walked as a channel or playlist:\n{calls}"
        );
    }
}

/// Cookies are passed through, not interpreted: whatever the caller names arrives at yt-dlp
/// verbatim and on EVERY invocation, since a probe that can't see gated content reports the wrong
/// answer for the download that follows.
#[test]
fn cookie_flags_reach_yt_dlp_verbatim_on_every_invocation() {
    let rig = Rig::new("cookies_file");
    let jar = rig.jar();
    rig.dl("ok", YT_VIDEO, &["--cookies", &jar]);
    let calls = rig.calls();
    assert!(calls.lines().count() > 1, "the YouTube path probes before downloading:\n{calls}");
    for line in calls.lines().filter(|line| !line.trim().is_empty()) {
        assert!(line.contains(&format!("--cookies {jar}")), "every call carries the jar: {line}");
    }

    let rig = Rig::new("cookies_browser");
    rig.dl("ok", YT_VIDEO, &["--cookies-from-browser", "firefox:/some/dir"]);
    let calls = rig.calls();
    for line in calls.lines().filter(|line| !line.trim().is_empty()) {
        assert!(
            line.contains("--cookies-from-browser firefox:/some/dir"),
            "the spec is passed through untouched: {line}"
        );
    }
}

/// `--taglist` is a reference lookup, not a download: it must not reach the network, and must not
/// need a URL to be useful.
#[test]
fn the_taglist_prints_without_a_url_and_without_downloading() {
    let rig = Rig::new("taglist");
    let out = Command::new(env!("CARGO_BIN_EXE_vidl"))
        .arg("--taglist")
        .env("PATH", format!("{}:{}", rig.root.join("bin").display(), std::env::var("PATH").unwrap_or_default()))
        .env("VIDL_STUB_DIR", &rig.stub)
        .env("VIDL_STUB_MODE", "ok")
        .output()
        .expect("run vidl");
    let all = text(&out);
    assert!(all.contains("--"), "it lists flags:\n{all}");
    assert!(
        !rig.calls().contains("--download-archive"),
        "no download was started:\n{}",
        rig.calls()
    );
}

/// A bare invocation is refused with a message, not a panic or a download of the empty string.
#[test]
fn a_missing_url_is_refused_with_a_message() {
    let out = Command::new(env!("CARGO_BIN_EXE_vidl")).output().expect("run vidl");
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("URL is required"), "{}", text(&out));
}

