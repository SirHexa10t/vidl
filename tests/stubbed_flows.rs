//! Offline end-to-end tests of failure handling and collection orchestration, driven by a
//! scripted yt-dlp stand-in (tests/fixtures/yt_dlp_stub.sh) placed first on the child's PATH,
//! beside a curl one that fails every fetch (tests/fixtures/curl_stub.sh). Deterministic and
//! network-free — these run in the default `cargo test`, and complement the live tests in
//! media_flags.rs / live_extended.rs rather than replacing them.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const YT_VIDEO: &str = "https://www.youtube.com/watch?v=stubvid0000";

/// One test's world: a scratch root holding a download dir, the stubs' state dir, and a bin dir
/// with the stand-ins installed as `yt-dlp` and `curl`. Removed on drop.
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
        // curl too, because every YouTube run makes the thumbnail pass: given a video without a
        // cover, it would otherwise fetch one from the real image CDN.
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        for (name, script) in [("yt-dlp", "yt_dlp_stub.sh"), ("curl", "curl_stub.sh")] {
            let installed = bin.join(name);
            fs::copy(fixtures.join(script), &installed).unwrap();
            fs::set_permissions(&installed, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Rig { root, into, stub, extra_path: None }
    }

    /// `vidl` with the rig's PATH and stub environment, its arguments still to give.
    fn vidl(&self, mode: &str) -> Command {
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
        let mut vidl = Command::new(env!("CARGO_BIN_EXE_vidl"));
        vidl.env("PATH", path).env("VIDL_STUB_DIR", &self.stub).env("VIDL_STUB_MODE", mode);
        vidl
    }

    /// Run `vidl <url> --into <rig into> <flags…>` with the rig's PATH/stub environment.
    fn dl(&self, mode: &str, url: &str, flags: &[&str]) -> Output {
        self.vidl(mode).arg(url).arg("--into").arg(&self.into).args(flags).output().expect("run vidl")
    }

    /// Run `vidl repair <rig into> <flags…>`, in the environment [`Self::dl`] runs in.
    fn repair(&self, flags: &[&str]) -> Output {
        self.vidl("ok").arg("repair").arg(&self.into).args(flags).output().expect("run vidl")
    }

    /// Every yt-dlp invocation the run made, one argv per line.
    fn calls(&self) -> String {
        fs::read_to_string(self.stub.join("calls.log")).unwrap_or_default()
    }

    /// Every curl invocation the run made, one argv per line — each of them failed.
    fn curl_calls(&self) -> String {
        fs::read_to_string(self.stub.join("curl.log")).unwrap_or_default()
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

/// The ffmpeg the test builds media with — from `dir` when [`ffmpeg_or_skip`] named one, else
/// PATH's — quiet, and kept off the terminal (see util::exec: concurrent ffmpegs race over it).
fn ffmpeg(dir: Option<&Path>) -> Command {
    let mut command =
        Command::new(dir.map(|dir| dir.join("ffmpeg")).unwrap_or_else(|| PathBuf::from("ffmpeg")));
    command.stdin(Stdio::null()).args(["-nostdin", "-v", "error", "-y"]);
    command
}

/// What of a download a test video holds, as [`lay_video`] lays it on disk.
#[derive(Clone, Copy)]
struct Laid<'a> {
    cover: Cover,
    /// Its subtitle tracks: `(language, title)`.
    subtitles: &'a [(&'a str, &'a str)],
    /// Every tag a download embeds ([`DOWNLOAD_TAGS`]), or none.
    tags: bool,
    /// The two chapters of [`CHAPTERS`], or none.
    chapters: bool,
}

/// A video holding its picture and sound, and nothing else of a download.
const BARE: Laid<'static> = Laid { cover: Cover::None, subtitles: &[], tags: false, chapters: false };

#[derive(Clone, Copy, PartialEq, Eq)]
enum Cover {
    None,
    Attached,
    /// Attached, then put through the plain `-map 0 -c copy` remux that demotes a cover: the
    /// Matroska muxer writes it back as one more video track.
    Demoted,
}

/// The metadata keys a YouTube download embeds, as yt-dlp hands them to ffmpeg — the ones
/// [`facts_line`] gives a value for.
const DOWNLOAD_TAGS: [&str; 8] = ["title", "date", "description", "synopsis", "purl", "comment", "artist", "genre"];

/// Two chapters in ffmpeg's ffmetadata form: what a laid video holds when it holds chapters, and
/// what [`facts_line`] says YouTube has.
const CHAPTERS: &str = ";FFMETADATA1\n\
                        [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=1000\ntitle=One\n\
                        [CHAPTER]\nTIMEBASE=1/1000\nSTART=1000\nEND=2000\ntitle=Two\n";

/// A one-colour JPEG at `path` — distinct colours, distinct pictures.
fn draw_picture(dir: Option<&Path>, path: &Path, colour: &str) {
    let drawn = ffmpeg(dir)
        .args(["-f", "lavfi", "-i", &format!("color=c={colour}:s=32x32"), "-frames:v", "1", "-update", "1"])
        .arg(path)
        .status()
        .is_ok_and(|status| status.success());
    assert!(drawn, "could not draw a picture");
}

/// A downloaded video at `path`: an mkv of two seconds' picture and sound, holding what `laid`
/// says (needs [`ffmpeg_or_skip`] to have passed).
fn lay_video(dir: Option<&Path>, path: &Path, laid: Laid) {
    let parts = path.with_extension("parts");
    fs::create_dir_all(&parts).unwrap();
    let mut command = ffmpeg(dir);
    command.args(["-f", "lavfi", "-i", "color=c=blue:s=64x64:d=2", "-f", "lavfi", "-i", "sine=frequency=440:duration=2"]);
    for (number, (language, _)) in laid.subtitles.iter().enumerate() {
        let track = parts.join(format!("{number}.vtt"));
        fs::write(&track, format!("WEBVTT\n\n00:00.000 --> 00:01.000\n{language} words\n")).unwrap();
        command.arg("-i").arg(track);
    }
    if laid.chapters {
        let chapters = parts.join("chapters.ffmeta");
        fs::write(&chapters, CHAPTERS).unwrap();
        command.args(["-f", "ffmetadata", "-i"]).arg(chapters);
        command.arg("-map_chapters").arg((2 + laid.subtitles.len()).to_string());
    }
    command.args(["-map", "0", "-map", "1"]);
    for number in 0..laid.subtitles.len() {
        command.arg("-map").arg((2 + number).to_string());
    }
    command.args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-c:s", "webvtt"]);
    for (number, (language, title)) in laid.subtitles.iter().enumerate() {
        command.arg(format!("-metadata:s:s:{number}")).arg(format!("language={language}"));
        command.arg(format!("-metadata:s:s:{number}")).arg(format!("title={title}"));
    }
    if laid.tags {
        for key in DOWNLOAD_TAGS {
            command.arg("-metadata").arg(format!("{key}={key} as downloaded"));
        }
    }
    if laid.cover != Cover::None {
        let cover = parts.join("cover.jpg");
        draw_picture(dir, &cover, "red");
        command.arg("-attach").arg(cover);
        command.args(["-metadata:s:t", "mimetype=image/jpeg", "-metadata:s:t", "filename=cover.jpg"]);
    }
    let laid_out = command.arg(path).status().is_ok_and(|status| status.success());
    assert!(laid_out, "could not lay out {}", path.display());
    if laid.cover == Cover::Demoted {
        let demoted = parts.join("demoted.mkv");
        let remuxed = ffmpeg(dir)
            .arg("-i")
            .arg(path)
            .args(["-map", "0", "-c", "copy"])
            .arg(&demoted)
            .status()
            .is_ok_and(|status| status.success());
        assert!(remuxed && fs::rename(&demoted, path).is_ok(), "could not demote the cover");
    }
    fs::remove_dir_all(&parts).unwrap();
}

/// A downloaded audio extraction at `path` — two seconds of Opus, holding every tag a download
/// embeds when `tags` — with neither subtitle tags nor chapters, nor a cover, which an audio
/// download never gets.
fn lay_audio(dir: Option<&Path>, path: &Path, tags: bool) {
    let mut command = ffmpeg(dir);
    command.args(["-f", "lavfi", "-i", "sine=frequency=440:duration=2", "-c:a", "libopus"]);
    if tags {
        for key in DOWNLOAD_TAGS {
            command.arg("-metadata").arg(format!("{key}={key} as downloaded"));
        }
    }
    let laid_out = command.arg(path).status().is_ok_and(|status| status.success());
    assert!(laid_out, "could not lay out {}", path.display());
}

/// A tiny real mkv at `path` (needs [`ffmpeg_or_skip`] to have passed). It has no cover, so a
/// YouTube run over it makes the default thumbnail pass try to fetch one (from the failing curl
/// stand-in); a test about something else passes `--no-thumbnail` to keep to its subject.
fn build_mkv(dir: Option<&Path>, path: &Path) {
    lay_video(dir, path, BARE);
}

/// [`build_mkv`] with a cover attached the way the thumbnail pass attaches one — a file the pass
/// has nothing to do to.
fn build_covered_mkv(dir: Option<&Path>, path: &Path) {
    lay_video(dir, path, Laid { cover: Cover::Attached, ..BARE });
}

/// [`build_covered_mkv`] put through the plain `-map 0 -c copy` remux that demotes a cover: the
/// Matroska muxer writes it back as one more video track, which the thumbnail pass restores.
fn build_demoted_mkv(dir: Option<&Path>, path: &Path) {
    lay_video(dir, path, Laid { cover: Cover::Demoted, ..BARE });
}

/// What a file holds, read back with ffprobe for an assertion — never by the code under test.
#[derive(Debug, Default, PartialEq)]
struct Held {
    /// Pictures held as covers (`attached_pic`).
    covers: usize,
    /// Pictures a remux demoted to plain video tracks.
    demoted: usize,
    /// Its subtitle tracks' titles, in order.
    subtitles: Vec<String>,
    /// Its tags' keys, lowercased: an mkv's container tags; an audio file's, its streams' too.
    tags: Vec<String>,
    chapters: usize,
}

fn ffprobe_output(path: &Path, args: &[&str]) -> String {
    let out = Command::new("ffprobe").args(["-v", "error"]).args(args).arg(path).output().expect("run ffprobe");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn held(path: &Path) -> Held {
    let mut held = Held::default();
    let streams = ffprobe_output(path, &["-show_entries", "stream=codec_name:stream_disposition=attached_pic", "-of", "default=nw=1"]);
    let mut codec = String::new();
    for line in streams.lines() {
        if let Some(name) = line.strip_prefix("codec_name=") {
            codec = name.to_string();
        } else if line == "DISPOSITION:attached_pic=1" {
            held.covers += 1;
        } else if line == "DISPOSITION:attached_pic=0" && matches!(codec.as_str(), "mjpeg" | "png") {
            held.demoted += 1;
        }
    }
    held.subtitles = ffprobe_output(path, &["-select_streams", "s", "-show_entries", "stream_tags=title", "-of", "csv=p=0"])
        .lines()
        .map(str::to_string)
        .collect();
    let entries = if path.extension().is_some_and(|ext| ext == "mkv") { "format_tags" } else { "format_tags:stream_tags" };
    let tags = ffprobe_output(path, &["-show_entries", entries, "-of", "default=nw=1"]);
    held.tags = tags
        .lines()
        .filter_map(|line| line.strip_prefix("TAG:")?.split_once('=').map(|(key, _)| key.to_lowercase()))
        .collect();
    held.tags.sort();
    held.tags.dedup();
    held.chapters = ffprobe_output(path, &["-show_chapters", "-of", "csv=p=0"]).lines().filter(|line| !line.is_empty()).count();
    held
}

/// The picture `path` holds as its cover, as bytes.
fn cover_bytes(path: &Path) -> Option<Vec<u8>> {
    let streams = ffprobe_output(path, &["-show_entries", "stream=index:stream_disposition=attached_pic", "-of", "csv=p=0"]);
    let index = streams.lines().find_map(|line| {
        let (index, attached) = line.split_once(',')?;
        (attached == "1").then(|| index.to_string())
    })?;
    let out = path.with_extension("cover-check.jpg");
    let taken = ffmpeg(None)
        .arg("-i")
        .arg(path)
        .args(["-map", &format!("0:{index}"), "-c", "copy", "-f", "image2", "-update", "1"])
        .arg(&out)
        .status()
        .is_ok_and(|status| status.success());
    let bytes = if taken { fs::read(&out).ok() } else { None };
    let _ = fs::remove_file(&out);
    bytes
}

/// The line yt-dlp prints about a video when a repair asks what YouTube offers now (vidl's facts
/// template): the uploader's subtitles and the automatic ones, each `(key, name)`; a value for
/// every key of [`DOWNLOAD_TAGS`]; and the chapters of [`CHAPTERS`].
fn facts_line(id: &str, language: &str, subtitles: &[(&str, &str)], automatic: &[(&str, &str)]) -> String {
    let captions = |tracks: &[(&str, &str)]| {
        let entries: Vec<String> =
            tracks.iter().map(|(key, name)| format!(r#""{key}": [{{"ext": "vtt", "name": "{name}"}}]"#)).collect();
        format!("{{{}}}", entries.join(", "))
    };
    [
        id.to_string(),
        language.to_string(),
        captions(subtitles),
        captions(automatic),
        format!(r#""Title of {id}""#),
        "20200101".to_string(),
        r#""First line\nsecond line""#.to_string(),
        format!("https://www.youtube.com/watch?v={id}"),
        r#""The Uploader""#.to_string(),
        r#"["Education"]"#.to_string(),
        "NA".to_string(),
        r#"[{"start_time": 0.0, "title": "One", "end_time": 1.0}, {"start_time": 1.0, "title": "Two", "end_time": 2.0}]"#.to_string(),
    ]
    .join("\t")
}

/// The lines a repair printed under one file's heading — the heading naming `file`.
fn under(all: &str, file: &str) -> String {
    let mut lines = all.lines().skip_while(|line| !(line.starts_with("  ") && !line.starts_with("    ") && line.contains(file)));
    assert!(lines.next().is_some(), "no heading for {file} in:\n{all}");
    lines.take_while(|line| line.starts_with("    ")).collect::<Vec<_>>().join("\n")
}

/// The files and folders inside `dir`, every name — sidecars and temporary files included.
fn everything_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> =
        fs::read_dir(dir).unwrap().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
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
fn a_stream_url_with_no_title_is_named_from_its_path_or_from_the_caller() {
    // A bare manifest gives yt-dlp nothing to read a title from, so it names the file after the
    // manifest and the download lands as `master [master].mp4`. Two ways out, and this pins both.
    const URL: &str = "https://cdn.example/hls/kung-fury/master.m3u8";

    // Without a name: the path supplies one. The record is what yt-dlp reports for such a URL —
    // title and id both the manifest's own stem.
    let rig = Rig::new("name_from_path");
    rig.names("master", "master__0.mp4", "master");
    let out = rig.dl("ok", URL, &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        rig.files().iter().any(|name| name == "kung-fury [master].mp4"),
        "the path above the manifest names it: {:?}",
        rig.files()
    );

    // With one: exactly what was asked for, and the id stays so the file is still traceable.
    let rig = Rig::new("name_given");
    rig.names("master", "master__0.mp4", "master");
    let out = rig.dl("ok", URL, &["--name", "Kung Fury"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        rig.files().iter().any(|name| name == "Kung Fury [master].mp4"),
        "the caller's name wins: {:?}",
        rig.files()
    );
}

#[test]
fn a_cookie_store_from_nowhere_is_refused_with_the_flag_that_would_work() {
    // Nothing in the path says which browser wrote it, and the browser decides which keyring entry
    // holds the decryption key — so guessing would reproduce the very failure this replaces.
    let rig = Rig::new("cookie_orphan");
    let store = rig.root.join("harvested");
    fs::write(&store, b"SQLite format 3\0binary").unwrap();
    let out = rig.dl("ok", YT_VIDEO, &["--cookies-use", store.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let all = text(&out);
    assert!(all.contains("not a Netscape cookies file"), "{all}");
    assert!(all.contains("--cookies-use <browser>:"), "and the form to use instead: {all}");
    assert!(rig.calls().is_empty(), "refused before yt-dlp was ever run:\n{}", rig.calls());
}

#[test]
fn a_netscape_cookies_file_is_passed_through_untouched() {
    // The common case must not be disturbed by any of the above.
    let rig = Rig::new("cookie_text");
    let file = rig.root.join("cookies.txt");
    fs::write(&file, "# Netscape HTTP Cookie File\n.x\tTRUE\t/\tTRUE\t0\ta\tb\n").unwrap();
    let out = rig.dl("ok", YT_VIDEO, &["--cookies-use", file.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out));
    let calls = rig.calls();
    assert!(calls.contains(&format!("--cookies {}", file.display())), "{calls}");
    assert!(!calls.contains("--cookies-from-browser"), "{calls}");
}

#[test]
fn a_name_for_many_files_is_refused_rather_than_half_applied() {
    // `--name` names ONE file. Anything that downloads several would put the same name on all of
    // them, so it stops instead — with the lever that does work for those.
    let rig = Rig::new("name_refused");
    for (label, args) in [
        ("--multi", vec!["--name", "X", "--multi"]),
        ("a playlist", vec!["--name", "X"]),
    ] {
        let url = if label == "--multi" {
            "https://cdn.example/hls/kung-fury/master.m3u8"
        } else {
            "https://www.youtube.com/playlist?list=PLstub"
        };
        let out = rig.dl("ok", url, &args);
        assert_eq!(out.status.code(), Some(2), "{label} must be refused: {}", text(&out));
        let all = text(&out);
        assert!(all.contains("--name is for one video"), "{label}: {all}");
        assert!(all.contains("-o <template>"), "{label}: and points at what works: {all}");
    }
}

#[test]
fn a_run_clears_unfinishable_leftovers_from_the_destination_before_downloading() {
    // The litter an interrupted older run leaves behind: partial files named without a rendition,
    // which the current naming can never resume and so would otherwise sit there forever.
    let rig = Rig::new("stale_sweep");
    let stale = rig.into.join("1_m3n4o5p6.mp4.part");
    let mine = rig.into.join("S3E01__1_aaaaaaaa.mp4.part");
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
    assert!(all.contains("1_m3n4o5p6.mp4.part"), "and names what it removed: {all}");
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
fn a_format_the_site_lists_but_cannot_serve_falls_through_to_one_it_can() {
    // Seen in the wild on a Kaltura entry: all three progressive MP4 flavours 404 while all three
    // HLS renditions serve, and the MP4s rank higher only because HLS reports unknown codecs. So
    // the best format by metadata is one whose file does not exist.
    let rig = Rig::new("format_missing");
    let out = rig.dl("format_missing_unless_checked", YT_VIDEO, &[]);
    assert!(out.status.success(), "another format rescues it: {}", text(&out));
    let all = text(&out);
    assert!(all.contains("missing from the server — trying the others"), "announced: {all}");
    assert!(all.contains("succeeded on another format"), "and its outcome: {all}");
    assert!(rig.ledger().is_empty(), "a rescued download must not be ledgered: {}", rig.ledger());
    let calls = rig.calls();
    assert_eq!(
        calls.matches("--check-formats").count(),
        1,
        "only the retry pays for probing — a healthy download must not:\n{calls}"
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
    rig.dl("age", YT_VIDEO, &["--cookies-use", &rig.jar()]);
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
    rig.dl("drm", YT_VIDEO, &["--cookies-use", &rig.jar()]);
    assert!(rig.ledger().contains("DRM-protected even with cookies"), "{}", rig.ledger());
}

#[test]
fn subtitles_on_a_non_youtube_site_names_the_supported_platforms_and_skips() {
    let rig = Rig::new("notice");
    let out = rig.dl("ok", "https://media.example.com/v/2", &["--subtitles"]);
    assert!(out.status.success(), "{}", text(&out));
    let all = text(&out);
    assert!(
        all.contains("--subtitles supported platforms: youtube"),
        "names what IS supported: {all}"
    );
    assert!(
        !all.contains("thumbnails: scanning") && !all.contains("subtitles: scanning"),
        "the passes themselves must not run off-platform: {all}"
    );
}

/// The thumbnail pass is a default, not a request — so a site it cannot serve is left out of it
/// without a word, where an asked-for `--subtitles` is told why.
#[test]
fn a_plain_run_on_a_non_youtube_site_makes_no_thumbnail_pass_and_no_notice_about_it() {
    let rig = Rig::new("no_notice");
    let out = rig.dl("ok", "https://media.example.com/v/2", &[]);
    assert!(out.status.success(), "{}", text(&out));
    let all = text(&out);
    assert!(!all.contains("thumbnails: scanning"), "no pass off-platform: {all}");
    assert!(!all.contains("supported platforms"), "and no notice for what nobody asked: {all}");
}

/// Cover art is the default in each of YouTube's three flows: a run makes the thumbnail pass
/// unless told `--no-thumbnail` — or downloading audio, which has no Matroska video to embed a
/// cover in.
#[test]
fn every_youtube_flow_makes_the_thumbnail_pass_unless_told_not_to_or_downloading_audio() {
    let flows = [
        ("video", YT_VIDEO),
        ("playlist", "https://www.youtube.com/playlist?list=PLstub"),
        ("channel", "https://www.youtube.com/@stubchannel"),
    ];
    let cases: [(&[&str], bool); 3] =
        [(&[], true), (&["--no-thumbnail"], false), (&["--audio"], false)];
    for (flow, url) in flows {
        for (case, (flags, makes_the_pass)) in cases.iter().enumerate() {
            let rig = Rig::new(&format!("thumb_{flow}_{case}"));
            // One entry for a collection's pass to cover; a lone video needs no scan.
            fs::write(rig.stub.join("scan.txt"), "1\tvidthumb001\tClip\tStub\nStub [PLstub]\n")
                .unwrap();
            let out = rig.dl("ok", url, flags);
            assert!(out.status.success(), "{flow} {flags:?}: {}", text(&out));
            assert_eq!(
                text(&out).contains("thumbnails: scanning"),
                *makes_the_pass,
                "{flow} {flags:?}: {}",
                text(&out)
            );
        }
    }
}

/// A video without a cover has one asked of YouTube's image CDN by its id: full size first, then
/// the `hqdefault` every video has. A fetch that fails leaves the file as it was, and nothing
/// beside it. (The rig's curl fails every fetch, which is what lets both asks show.)
#[test]
fn a_video_without_a_cover_asks_the_cdn_for_full_size_then_hq_and_a_miss_changes_nothing() {
    let Some(ffmpeg_dir) = ffmpeg_or_skip("thumbnail fetch order") else { return };
    let mut rig = Rig::new("thumb_fetch");
    rig.extra_path = ffmpeg_dir.clone();
    let video = rig.into.join("v__stubvid0000.mkv");
    build_mkv(ffmpeg_dir.as_deref(), &video);
    let before = fs::read(&video).unwrap();

    let out = rig.dl("ok", YT_VIDEO, &[]);
    assert!(out.status.success(), "a cover it cannot have fails nothing else: {}", text(&out));
    let curl = rig.curl_calls();
    let asked: Vec<&str> = curl.lines().filter_map(|line| line.split(' ').next_back()).collect();
    assert_eq!(
        asked,
        [
            "https://i.ytimg.com/vi/stubvid0000/maxresdefault.jpg",
            "https://i.ytimg.com/vi/stubvid0000/hqdefault.jpg",
        ],
        "{curl}"
    );
    assert!(text(&out).contains("could not embed a thumbnail"), "{}", text(&out));
    assert_eq!(fs::read(&video).unwrap(), before, "a missed fetch must leave the file untouched");
    assert_eq!(rig.files(), ["v__stubvid0000.mkv"], "and no fetched-image leftover beside it");
}

/// What a default run over a library says: a line for each video the thumbnail pass acts on or
/// cannot, none for one it leaves alone — every run makes this pass, so a re-run over a whole
/// channel must not list the channel back — and a closing count of all of them.
#[test]
fn the_thumbnail_pass_names_only_the_videos_it_acts_on_or_skips_and_counts_the_rest() {
    let Some(ffmpeg_dir) = ffmpeg_or_skip("thumbnail pass report") else { return };
    let mut rig = Rig::new("thumb_report");
    rig.extra_path = ffmpeg_dir.clone();
    fs::write(
        rig.stub.join("scan.txt"),
        "1\tvidcover001\tCovered\tStub List\n\
         2\tviddemo0002\tDemoted\tStub List\n\
         3\tvidbare0003\tBare\tStub List\n\
         4\tvidgone0004\tGone\tStub List\n\
         Stub List [PLstub]\n",
    )
    .unwrap();
    let home = rig.into.join("Stub List [PLstub]");
    fs::create_dir_all(&home).unwrap();
    // All four archived, so the run downloads nothing and goes straight to its passes.
    fs::write(
        home.join(".dl_video_archive.txt"),
        "youtube vidcover001\nyoutube viddemo0002\nyoutube vidbare0003\nyoutube vidgone0004\n",
    )
    .unwrap();
    build_covered_mkv(ffmpeg_dir.as_deref(), &home.join("Covered__vidcover001.mkv"));
    build_demoted_mkv(ffmpeg_dir.as_deref(), &home.join("Demoted__viddemo0002.mkv"));
    build_mkv(ffmpeg_dir.as_deref(), &home.join("Bare__vidbare0003.mkv"));

    let out = rig.dl("ok", "https://www.youtube.com/playlist?list=PLstub", &[]);
    assert!(out.status.success(), "{}", text(&out));
    let all = text(&out);
    let lines_of = |id: &str| -> Vec<&str> {
        all.lines().filter(|line| line.contains(&format!("[{id}]"))).collect()
    };
    assert!(lines_of("vidcover001").is_empty(), "a video left alone gets no line: {all}");
    let demoted = lines_of("viddemo0002");
    assert!(demoted.iter().any(|line| line.contains("thumbnail restored")), "{all}");
    let bare = lines_of("vidbare0003");
    assert!(bare.iter().any(|line| line.contains("missing a thumbnail")), "{all}");
    assert!(bare.iter().any(|line| line.contains("could not embed a thumbnail")), "{all}");
    let gone = lines_of("vidgone0004");
    assert!(gone.iter().any(|line| line.contains("no file found — skipping")), "{all}");
    assert!(
        all.contains("thumbnails: 1 present, 1 restored, 1 failed, 1 skipped"),
        "and the count of all four: {all}"
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

/// A video beside its own audio extraction — the two downloads one id can have — is the one the
/// passes patch: the thumbnail pass asks the CDN for its cover, and the subtitle pass reads its
/// tracks, rather than either stopping at the audio as the first file the folder listed.
#[test]
fn a_video_beside_its_audio_extraction_is_the_one_the_passes_patch() {
    let Some(ffmpeg_dir) = ffmpeg_or_skip("video beside its audio") else { return };
    let mut rig = Rig::new("video_and_audio");
    rig.extra_path = ffmpeg_dir.clone();
    fs::write(rig.into.join("v__stubvid0000.opus"), b"OPUSDATA").unwrap();
    build_mkv(ffmpeg_dir.as_deref(), &rig.into.join("v__stubvid0000.mkv"));
    let out = rig.dl("ok", YT_VIDEO, &["--subtitles"]);
    assert!(out.status.success(), "{}", text(&out));
    let all = text(&out);
    assert!(all.contains("missing a thumbnail") && !all.contains("not an mkv"), "{all}");
    assert!(rig.curl_calls().contains("/vi/stubvid0000/"), "its cover is asked for: {}", rig.curl_calls());
    assert!(all.contains("no subtitles exist for this video") && !all.contains("audio — subtitles kept as tags"), "{all}");
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
    let out = rig.dl("ok", YT_VIDEO, &["--subtitles", "--no-thumbnail"]);
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
    let out = rig.dl("ok", YT_VIDEO, &["--subtitles", "--no-thumbnail"]);
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
    page.push_str(r#"<script>{"partner_id":"8642"}</script>"#);
    for _ in 0..4 {
        page.push_str(r#"<img src="https://cdn.site.test/image/p/8642/thumb.jpg">"#);
    }
    page.push_str(r#"<script src="https://cdnapisec.kaltura.com/p/1234567/embedPlaykitJs/x">"#);
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

    // One download per entry, addressed through the player's account (1234567) and NOT the one the
    // page declares as its own (8642), which Kaltura rejects.
    let calls = rig.calls();
    let downloads = downloads(&calls);
    assert_eq!(downloads.len(), 3, "one download each:\n{calls}");
    for (position, id) in ids.iter().enumerate() {
        let target = format!("kaltura:1234567:{id}");
        assert!(
            downloads[position].contains(&target),
            "entry {} must be #{} in page order, got: {}",
            id,
            position + 1,
            downloads[position]
        );
    }
    assert!(!rig.calls().contains("kaltura:8642:"), "the page's own account must not be used");
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
    rig.dl("ok", YT_VIDEO, &["--cookies-use", &jar]);
    let calls = rig.calls();
    assert!(calls.lines().count() > 1, "the YouTube path probes before downloading:\n{calls}");
    for line in calls.lines().filter(|line| !line.trim().is_empty()) {
        assert!(line.contains(&format!("--cookies {jar}")), "every call carries the jar: {line}");
    }

    let rig = Rig::new("cookies_browser");
    rig.dl("ok", YT_VIDEO, &["--cookies-use", "firefox:/some/dir"]);
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


/// A library with one of each kind of lack — a video holding nothing but picture and sound, a cover
/// a remux demoted, an auto-generated track under YouTube's own name — gets what a download of each
/// would hold now, said file by file; the whole one is left untouched and unmentioned. A second run
/// then finds every video whole and changes nothing.
#[test]
fn a_repair_adds_what_each_video_lacks_and_a_second_run_finds_nothing_to_do() {
    let Some(ffmpeg_dir) = ffmpeg_or_skip("repair") else { return };
    let mut rig = Rig::new("repair");
    rig.extra_path = ffmpeg_dir.clone();
    let dir = ffmpeg_dir.as_deref();
    let english = [("en", "English")];
    let whole = Laid { cover: Cover::Attached, subtitles: &english, tags: true, chapters: true };
    let files = [
        ("20200101__Whole__vidwhole001.mkv", whole),
        ("20200101__Bare__vidbare0002.mkv", BARE),
        ("20200101__Demoted__viddemo0003.mkv", Laid { cover: Cover::Demoted, ..whole }),
        ("20200101__Unmarked__vidmark0004.mkv", Laid { subtitles: &[("ko", "Korean")], ..whole }),
    ];
    for (name, laid) in files {
        lay_video(dir, &rig.into.join(name), laid);
    }
    fs::write(
        rig.into.join(".dl_video_archive.txt"),
        "youtube vidwhole001\nyoutube vidbare0002\nyoutube viddemo0003\nyoutube vidmark0004\n\
         youtube vidgone0005\ngeneric somewhere01\n",
    )
    .unwrap();
    let facts = [
        facts_line("vidwhole001", "en", &english, &[]),
        facts_line("vidbare0002", "ko", &english, &[("ko-orig", "Korean (Original)")]),
        facts_line("viddemo0003", "en", &english, &[]),
        facts_line("vidmark0004", "ko", &[], &[("ko", "Korean")]),
    ];
    fs::write(rig.stub.join("facts.txt"), facts.join("\n")).unwrap();
    fs::write(rig.stub.join("subs_served.txt"), "en\nko-orig\n").unwrap();
    draw_picture(dir, &rig.stub.join("thumb.jpg"), "green");
    let whole_before = fs::read(rig.into.join(files[0].0)).unwrap();

    let out = rig.repair(&[]);
    let all = text(&out);
    assert!(out.status.success(), "{all}");
    assert!(!all.contains("vidwhole001]"), "a whole video gets no line: {all}");
    assert_eq!(fs::read(rig.into.join(files[0].0)).unwrap(), whole_before, "and is left as it was");

    let bare = under(&all, "vidbare0002");
    for said in [
        "missing: thumbnail, subtitles (en, ko-orig), metadata (title, date, description, synopsis, purl, comment, artist, genre), chapters (2)",
        "thumbnail embedded",
        "subtitles embedded: en, ko-orig",
        "added metadata (title, date, description, synopsis, purl, comment, artist, genre) and 2 chapter(s)",
    ] {
        assert!(bare.contains(said), "{said:?} in:\n{bare}");
    }
    let held_bare = held(&rig.into.join(files[1].0));
    assert_eq!((held_bare.covers, held_bare.demoted, held_bare.chapters), (1, 0, 2), "{held_bare:?}");
    assert_eq!(held_bare.subtitles, ["English", "Korean (auto-generated)"]);
    assert!(DOWNLOAD_TAGS.iter().all(|key| held_bare.tags.iter().any(|held| held == key)), "{held_bare:?}");
    assert_eq!(cover_bytes(&rig.into.join(files[1].0)), fs::read(rig.stub.join("thumb.jpg")).ok(), "the picture YouTube served");

    let demoted = under(&all, "viddemo0003");
    assert!(demoted.contains("missing: thumbnail (a remux made a plain video track of it)"), "{demoted}");
    assert!(demoted.contains("thumbnail restored"), "{demoted}");
    let held_demoted = held(&rig.into.join(files[2].0));
    assert_eq!((held_demoted.covers, held_demoted.demoted), (1, 0), "{held_demoted:?}");
    assert!(!rig.curl_calls().contains("viddemo0003"), "a cover restored is not fetched again: {}", rig.curl_calls());

    let unmarked = under(&all, "vidmark0004");
    assert!(unmarked.contains("missing: auto-generated mark on 1 subtitle track(s)"), "{unmarked}");
    assert!(unmarked.contains("marked 1 subtitle track(s) auto-generated"), "{unmarked}");
    assert_eq!(held(&rig.into.join(files[3].0)).subtitles, ["Korean (auto-generated)"], "marked, not fetched twice");

    assert!(all.contains("[vidgone0005]") && all.contains("archived, but with no file"), "{all}");
    assert!(all.contains("1 archived from other sites"), "{all}");
    assert!(all.contains("repair: 1 already complete, 3 completed"), "{all}");
    let calls = rig.calls();
    let asked = calls.lines().filter(|line| line.contains("%(chapters)")).count();
    assert_eq!(asked, 1, "YouTube is asked once, about every video: {calls}");
    assert_eq!(everything_in(&rig.into), {
        let mut names: Vec<String> = files.iter().map(|(name, _)| name.to_string()).collect();
        names.push(".dl_video_archive.txt".into());
        names.sort();
        names
    }, "nothing left beside them");

    let after: Vec<Vec<u8>> = files.iter().map(|(name, _)| fs::read(rig.into.join(name)).unwrap()).collect();
    let again = rig.repair(&[]);
    let all = text(&again);
    assert!(again.status.success(), "{all}");
    assert!(all.contains("repair: 4 already complete") && !all.contains("missing:"), "{all}");
    let unchanged: Vec<Vec<u8>> = files.iter().map(|(name, _)| fs::read(rig.into.join(name)).unwrap()).collect();
    assert!(after == unchanged, "a second run must change nothing");
}

/// What a repair cannot get is said, with the reason it was given, and fails the run — while
/// everything else is still added: here, no picture from the image server and no subtitles served,
/// yet the metadata and chapters go in; and a video YouTube will not talk about is left exactly as
/// it was. A subtitle fetch refused outright says so as well.
#[test]
fn what_a_repair_cannot_get_is_said_the_rest_is_still_added_and_the_run_fails() {
    let Some(ffmpeg_dir) = ffmpeg_or_skip("repair failures") else { return };
    let mut rig = Rig::new("repair_fail");
    rig.extra_path = ffmpeg_dir.clone();
    let dir = ffmpeg_dir.as_deref();
    let lacking = "x__vidfail0001.mkv";
    let walled = "x__vidwall0002.mkv";
    lay_video(dir, &rig.into.join(lacking), BARE);
    lay_video(dir, &rig.into.join(walled), Laid { cover: Cover::Attached, ..BARE });
    fs::write(rig.stub.join("facts.txt"), facts_line("vidfail0001", "en", &[("en", "English")], &[])).unwrap();
    fs::write(rig.stub.join("facts_errors.txt"), "ERROR: [youtube] vidwall0002: Sign in to confirm you're not a bot").unwrap();
    let walled_before = fs::read(rig.into.join(walled)).unwrap();

    let out = rig.repair(&[]);
    let all = text(&out);
    assert!(!out.status.success(), "a repair left wanting fails: {all}");
    let failing = under(&all, "vidfail0001");
    for said in [
        "thumbnail: could not fetch it from YouTube's image server",
        "subtitles: none of en arrived (rate-limited?)",
        "added metadata (title, date, description, synopsis, purl, comment, artist, genre) and 2 chapter(s)",
    ] {
        assert!(failing.contains(said), "{said:?} in:\n{failing}");
    }
    let held_failing = held(&rig.into.join(lacking));
    assert_eq!((held_failing.covers, held_failing.chapters), (0, 2), "{held_failing:?}");
    assert!(held_failing.subtitles.is_empty());
    assert!(DOWNLOAD_TAGS.iter().all(|key| held_failing.tags.iter().any(|held| held == key)), "{held_failing:?}");

    let walled_said = under(&all, "vidwall0002");
    assert!(
        walled_said.contains("YouTube did not answer (Sign in to confirm you're not a bot) — its subtitles, metadata and chapters went unchecked"),
        "{walled_said}"
    );
    assert_eq!(fs::read(rig.into.join(walled)).unwrap(), walled_before, "a video not checked is not touched");
    assert!(all.contains("repair: 1 left incomplete, 1 not checked"), "{all}");
    assert_eq!(everything_in(&rig.into), [lacking, walled], "no picture, sidecar or half-written file left");

    // The fetch itself refused: said apart from tracks that merely did not arrive.
    fs::write(rig.stub.join("subs_refused"), "").unwrap();
    let refused = text(&rig.repair(&[]));
    assert!(under(&refused, "vidfail0001").contains("subtitles: could not fetch en (yt-dlp failed)"), "{refused}");
}

/// `--check` says what each video lacks and changes nothing — no picture fetched, no subtitles
/// asked for, no file rewritten — failing while anything is lacking, and passing once nothing is.
#[test]
fn checking_says_what_each_video_lacks_and_changes_nothing() {
    let Some(ffmpeg_dir) = ffmpeg_or_skip("repair --check") else { return };
    let mut rig = Rig::new("repair_check");
    rig.extra_path = ffmpeg_dir.clone();
    let dir = ffmpeg_dir.as_deref();
    let english = [("en", "English")];
    let whole = "x__vidwhole001.mkv";
    let bare = "x__vidbare0002.mkv";
    lay_video(dir, &rig.into.join(whole), Laid { cover: Cover::Attached, subtitles: &english, tags: true, chapters: true });
    lay_video(dir, &rig.into.join(bare), BARE);
    let facts = [facts_line("vidwhole001", "en", &english, &[]), facts_line("vidbare0002", "en", &english, &[])];
    fs::write(rig.stub.join("facts.txt"), facts.join("\n")).unwrap();
    fs::write(rig.stub.join("subs_served.txt"), "en\n").unwrap();
    draw_picture(dir, &rig.stub.join("thumb.jpg"), "green");
    let before = (fs::read(rig.into.join(whole)).unwrap(), fs::read(rig.into.join(bare)).unwrap());

    let out = rig.repair(&["--check"]);
    let all = text(&out);
    assert!(!out.status.success(), "a video lacking something fails the check: {all}");
    assert!(
        under(&all, "vidbare0002").contains(
            "missing: thumbnail, subtitles (en), metadata (title, date, description, synopsis, purl, comment, artist, genre), chapters (2)"
        ),
        "{all}"
    );
    assert!(!all.contains("embedded") && !all.contains("added") && !all.contains("restored"), "nothing is added: {all}");
    assert!(all.contains("repair --check: 1 already complete, 1 lacking something — nothing was changed"), "{all}");
    assert_eq!((fs::read(rig.into.join(whole)).unwrap(), fs::read(rig.into.join(bare)).unwrap()), before);
    assert!(rig.curl_calls().is_empty(), "no picture fetched: {}", rig.curl_calls());
    assert!(!rig.calls().contains("--skip-download"), "no subtitles asked for: {}", rig.calls());

    fs::remove_file(rig.into.join(bare)).unwrap();
    let clean = rig.repair(&["--check"]);
    assert!(clean.status.success(), "{}", text(&clean));
    assert!(text(&clean).contains("repair --check: 1 already complete — nothing was changed"), "{}", text(&clean));
}

/// An audio extraction gets the subtitles it lacks as the tags they ride in there, its metadata —
/// what its container keeps of it: Opus has no room for the description beside the comment — and
/// its chapters; and is then found whole.
#[test]
fn an_audio_extraction_gets_the_subtitle_tags_metadata_and_chapters_it_lacks() {
    let Some(ffmpeg_dir) = ffmpeg_or_skip("repair audio") else { return };
    let mut rig = Rig::new("repair_audio");
    rig.extra_path = ffmpeg_dir.clone();
    let song = rig.into.join("Song__vidaudi0001.opus");
    lay_audio(ffmpeg_dir.as_deref(), &song, false);
    fs::write(rig.stub.join("facts.txt"), facts_line("vidaudi0001", "en", &[("en", "English")], &[])).unwrap();
    fs::write(rig.stub.join("subs_served.txt"), "en\n").unwrap();

    let out = rig.repair(&[]);
    let all = text(&out);
    assert!(out.status.success(), "{all}");
    let said = under(&all, "vidaudi0001");
    for line in [
        "missing: subtitles (en), metadata (title, date, synopsis, purl, comment, artist, genre), chapters (2)",
        "subtitles embedded: en",
        "added metadata (title, date, synopsis, purl, comment, artist, genre) and 2 chapter(s)",
    ] {
        assert!(said.contains(line), "{line:?} in:\n{said}");
    }
    assert!(!said.contains("thumbnail"), "an audio file gets no cover: {said}");
    let held_song = held(&song);
    assert_eq!((held_song.covers, held_song.chapters), (0, 2), "{held_song:?}");
    for key in ["subtitles_en", "title", "date", "synopsis", "purl", "comment", "artist", "genre"] {
        assert!(held_song.tags.iter().any(|held| held == key), "{key} in {held_song:?}");
    }
    let again = rig.repair(&[]);
    assert!(text(&again).contains("repair: 1 already complete") && !text(&again).contains("missing:"), "{}", text(&again));
}

/// Every YouTube download under the folder is found by its id, a collection's folder included, and
/// each copy of a video — here a video and its audio — gets what fits it, the video asked about
/// once. A video in a container nothing is added to is named and passed over without failing the
/// run, and a file named for no YouTube video is no concern of the repair.
#[test]
fn every_youtube_download_below_is_found_by_its_id_and_each_copy_completed() {
    let Some(ffmpeg_dir) = ffmpeg_or_skip("repair discovery") else { return };
    let mut rig = Rig::new("repair_found");
    rig.extra_path = ffmpeg_dir.clone();
    let dir = ffmpeg_dir.as_deref();
    let english = [("en", "English")];
    let playlist = rig.into.join("Mix [PLmix]");
    fs::create_dir_all(&playlist).unwrap();
    fs::write(playlist.join(".dl_video_archive.txt"), "youtube vidnest0001\n").unwrap();
    let nested = playlist.join("001__20200101__Nested__vidnest0001.mkv");
    lay_video(dir, &nested, Laid { cover: Cover::None, subtitles: &english, tags: true, chapters: true });
    lay_audio(dir, &rig.into.join("Nested__vidnest0001.opus"), true);
    fs::write(rig.into.join("x__vidwebm0002.webm"), b"a webm").unwrap();
    fs::write(rig.into.join("Some clip [abc123].mp4"), b"another site's").unwrap();
    fs::write(rig.stub.join("facts.txt"), facts_line("vidnest0001", "en", &english, &[])).unwrap();
    fs::write(rig.stub.join("subs_served.txt"), "en\n").unwrap();
    draw_picture(dir, &rig.stub.join("thumb.jpg"), "green");

    let out = rig.repair(&[]);
    let all = text(&out);
    assert!(out.status.success(), "a container passed over fails nothing: {all}");
    let video = under(&all, "001__20200101__Nested__vidnest0001.mkv");
    assert!(video.contains("missing: thumbnail") && video.contains("thumbnail embedded"), "{video}");
    let audio = under(&all, "Nested__vidnest0001.opus [vidnest0001]");
    assert!(audio.contains("missing: subtitles (en), chapters (2)") && audio.contains("subtitles embedded: en"), "{audio}");
    assert!(under(&all, "vidwebm0002").contains("not an mkv"), "{all}");
    assert!(!all.contains("Some clip"), "{all}");
    assert!(all.contains("repair: 2 completed, 1 passed over (no mkv)"), "{all}");
    let asked = rig.calls();
    let asked = asked.lines().find(|line| line.contains("%(chapters)")).expect("YouTube was asked");
    assert_eq!(asked.matches("vidnest0001").count(), 1, "once for both copies: {asked}");
    assert!(!asked.contains("vidwebm0002"), "nothing asked about what nothing can be added to: {asked}");
}

/// A large library is asked about in parts, each counted off as YouTube answers it — a run's
/// answers come all at once, and a paced one takes a second a video, so one silence of half an hour
/// would read as a hang. (Files that are no media at all: what is counted here is the asking.)
#[test]
fn a_large_library_is_asked_about_in_parts_each_counted_off() {
    let rig = Rig::new("repair_parts");
    let ids: Vec<String> = (0..51).map(|n| format!("vidpart{n:04}")).collect();
    for id in &ids {
        fs::write(rig.into.join(format!("x__{id}.mkv")), b"not media").unwrap();
    }
    let out = rig.repair(&["--check"]);
    let all = text(&out);
    assert!(all.contains("repair: asking YouTube about 51 video(s)…"), "{all}");
    assert!(all.contains("  asked about 50 of 51\n") && all.contains("  asked about 51 of 51\n"), "{all}");
    let calls = rig.calls();
    let parts: Vec<&str> = calls.lines().filter(|line| line.contains("%(chapters)")).collect();
    assert_eq!(parts.len(), 2, "{calls}");
    assert!(parts.iter().all(|part| part.contains("--sleep-requests 1")), "a library this size is paced: {calls}");
    assert_eq!(parts[1].matches("watch?v=").count(), 1, "the second part asks about the one left: {}", parts[1]);
}
