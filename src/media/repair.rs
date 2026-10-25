//! `vidl repair`: add to downloaded videos the data a download of them would hold now and they
//! lack — the thumbnail, subtitle tracks and their auto-generated marks, the metadata tags and
//! chapters a download embeds — in a folder and every collection folder below it, or in one file,
//! without downloading a video again. What a file lacks is taken never to have been fetched as it
//! should have been: by an older version, or refused by YouTube at the time.
//!
//! A file is known by the YouTube id its name carries (`…__<id>.<ext>`, as every YouTube template
//! here ends it), or else by the page its metadata says it was downloaded from ([`source_url`]) — a
//! renamed download is still found — and never by its title: a video can be retitled on YouTube,
//! its id cannot. A file from another site is named and passed over, as is one nothing tells the
//! origin of. Two files
//! with one id are two copies of one video — a video beside its audio extraction, one video in two
//! playlists — and each gets what fits its kind. The download archives say what was
//! downloaded: an archived id with no file left is reported, and an entry from another site is
//! counted and passed over, since nothing can be asked about it — an archive line holds an id, and
//! off YouTube no URL can be rebuilt from one.
//!
//! What a file should hold is what a download of it would embed now, so YouTube is asked once for
//! every video ([`super::facts`]). A file only ever gains: a tag it has keeps its value, whatever
//! YouTube says now, so a retitled video does not retitle the copy kept. Each addition is a
//! stream-copy rewrite with the covers carried through, renamed over the file only once it
//! succeeded — and only the formats a download writes into can take one: a Matroska video (every
//! YouTube video download is one) or an audio extraction. Another video container is named and
//! passed over.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::Env;
use crate::util::exec::capture_stdout;
use crate::util::json::{self, Json};
use crate::util::style;
use crate::ytdlp::ARCHIVE_NAME;
use crate::ytdlp::scan::archive_entries;

use super::facts::{self, Chapter, Facts};
use super::subtitles::Pick;
use super::{
    apply_title_stamps, attach_thumbnail, auto_title_stamps, embedded, fetch_subtitles,
    fetch_youtube_thumbnail, is_audio, is_mkv, mux_subtitles, repair_covers, subtitle_sidecar,
    subtitle_tag_name, subtitle_title, subtitle_title_listing, write_subtitle_tags, CoverCarry,
    Embedded, AUDIO_EXTS, VIDEO_EXTS,
};

/// What each container keeps of what a download embeds: `(extension, tags it drops, whether it
/// keeps chapters)`. Measured by writing every metadata key and two chapters through ffmpeg n9.0
/// and reading them back with ffprobe — the way a download writes them, so a file of that kind
/// never held the rest, and expecting it would find it missing on every run and "add" it to no
/// effect each time. (Vorbis comments store a `comment` as DESCRIPTION, and a `description` reads
/// back as that comment.)
const KEEPS: &[(&str, &[&str], bool)] = &[
    (".mkv", &[], true),
    (".mp3", &[], true),
    (".opus", &["description"], true),
    (".ogg", &["description"], false),
    (".flac", &["description"], false),
    (".m4a", &["purl"], true),
    (".wav", &["description", "synopsis", "purl"], false),
];

/// Add to every downloaded YouTube video under `path` — a folder and every folder below it, or the
/// one file `path` names — the data it lacks; or, with `check`, only say what each one lacks,
/// adding nothing. Returns the exit code: 1 when a file is left lacking something it could hold
/// (checking: lacks anything) or could not be checked — the one file named included, when nothing
/// tells where it came from; 0 otherwise.
pub fn repair(path: &Path, env: Env, check: bool) -> i32 {
    if path.is_dir() {
        let library = survey(path);
        if library.files.is_empty() {
            println!("repair: no downloaded YouTube videos under {}", path.display());
            report_unreached(path, &library);
            return 0;
        }
        println!("repair: {} video file(s) under {}", library.files.len(), path.display());
        return complete(path, &library, env, check);
    }
    let shown = path.display();
    match origin(path) {
        Origin::YouTube(id) => {
            println!("repair: {shown}, YouTube video [{id}]");
            let library = Library { files: vec![Found { path: path.to_path_buf(), id }], ..Library::default() };
            complete(path.parent().unwrap_or(Path::new("")), &library, env, check)
        }
        Origin::Elsewhere(host) => {
            println!("repair: {shown} was downloaded from {host} — passed over: a repair completes YouTube videos only");
            0
        }
        Origin::Unknown => {
            let why = "no YouTube id in its name, and no source URL in its metadata";
            eprintln!("{}", style::problematic(&format!("repair: nothing says where {shown} was downloaded from — {why}")));
            1
        }
        Origin::Unreadable => {
            eprintln!("{}", style::problematic(&format!("repair: could not read {shown} (ffprobe), and its name carries no YouTube id")));
            1
        }
    }
}

/// [`repair`]'s work on the videos of `library`, found under `root`: YouTube asked about each, each
/// completed or checked and said, then what was passed over, then the run's counts.
fn complete(root: &Path, library: &Library, env: Env, check: bool) -> i32 {
    // Asked about once per video, whatever copies of it there are — and not at all for one only
    // held in a container nothing is added to.
    let mut ids: Vec<String> = Vec::new();
    for found in &library.files {
        if kind(&found.path) != Kind::OtherVideo && !ids.contains(&found.id) {
            ids.push(found.id.clone());
        }
    }
    println!("repair: asking YouTube about {} video(s)…", ids.len());
    let answers = facts::ask_youtube(&ids, env);
    let mut totals = Totals::default();
    for found in &library.files {
        let facts = answers.facts.iter().find(|facts| facts.id == found.id);
        let unanswered = answers.unanswered.iter().find(|(id, _)| *id == found.id).map(|(_, why)| why.as_str());
        let (outcome, lines) = repair_one(found, facts, unanswered, env, check);
        if !lines.is_empty() {
            println!("  {} [{}]", found.path.strip_prefix(root).unwrap_or(&found.path).display(), found.id);
            for line in lines {
                println!("    {line}");
            }
        }
        totals.count(outcome);
    }
    report_unreached(root, library);
    println!("repair{}: {}", if check { " --check" } else { "" }, totals.summary(check));
    i32::from(totals.left_wanting())
}

/// A downloaded YouTube video file, and its id.
struct Found {
    path: PathBuf,
    id: String,
}

/// What lies under the folder a repair was pointed at.
#[derive(Default)]
struct Library {
    /// Every file of a YouTube video.
    files: Vec<Found>,
    /// Every download-archive entry: `(extractor, id)`.
    archived: Vec<(String, String)>,
    /// Every media file from another site, with the site's host.
    elsewhere: Vec<(PathBuf, String)>,
    /// How many media files nothing tells the origin of ([`Origin::Unknown`]).
    unknown: usize,
    /// How many media files with no id in the name ffprobe could not read.
    unreadable: usize,
}

impl Library {
    /// Count `path` in by where it came from.
    fn take(&mut self, path: PathBuf, origin: Origin) {
        match origin {
            Origin::YouTube(id) => self.files.push(Found { path, id }),
            Origin::Elsewhere(host) => self.elsewhere.push((path, host)),
            Origin::Unknown => self.unknown += 1,
            Origin::Unreadable => self.unreadable += 1,
        }
    }
}

/// Where a media file came from, as far as a repair can tell.
#[derive(Debug, PartialEq)]
enum Origin {
    /// A YouTube video, by its id.
    YouTube(String),
    /// Another site, by the host of the page it came from.
    Elsewhere(String),
    /// Nothing says: no YouTube id in its name, no source URL in its metadata.
    Unknown,
    /// Its name carries no id, and ffprobe could not read its metadata.
    Unreadable,
}

/// Where `file` came from: the YouTube id its name carries, or else the page its metadata names.
fn origin(file: &Path) -> Origin {
    if let Some(id) = youtube_id(&file.file_name().unwrap_or_default().to_string_lossy()) {
        return Origin::YouTube(id.to_string());
    }
    match tags_and_chapters(file, is_audio(file)) {
        Some((tags, _)) => origin_in_tags(&tags),
        None => Origin::Unreadable,
    }
}

/// Where a file came from by its tags: YouTube's video when [`source_url`] is a YouTube watch page,
/// another site's when it is that site's — and nothing known when there is none, or it is a YouTube
/// page that names no video.
fn origin_in_tags(tags: &[(String, String)]) -> Origin {
    let Some(url) = source_url(tags) else { return Origin::Unknown };
    if !crate::is_youtube(url) {
        return Origin::Elsewhere(crate::host_of(url));
    }
    crate::id_from_url(url).map_or(Origin::Unknown, Origin::YouTube)
}

/// The page a file's tags say it was downloaded from: a lone URL in its `purl` tag or, failing that,
/// its `comment` — where every download here puts the page's URL, one of the two kept by each
/// container a download writes ([`KEEPS`]). A lone URL only: a comment of someone's own, links and
/// all, says where else to look, not where the file came from.
fn source_url(tags: &[(String, String)]) -> Option<&str> {
    let lone_url = |value: &&str| {
        (value.starts_with("https://") || value.starts_with("http://")) && !value.contains(char::is_whitespace)
    };
    ["purl", "comment"].into_iter().find_map(|wanted| {
        tags.iter().filter(|(key, _)| key.eq_ignore_ascii_case(wanted)).map(|(_, value)| value.trim()).find(lone_url)
    })
}

/// Walk `root` and every folder below it — one it cannot read is passed over — for its media files,
/// each counted in by where it came from ([`origin`]), and the download archives. A file a download
/// or a pass has in hand is left out ([`in_passing`]).
fn survey(root: &Path) -> Library {
    let mut library = Library::default();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ARCHIVE_NAME {
                library.archived.extend(archive_entries(&std::fs::read_to_string(&path).unwrap_or_default()));
            } else if is_media_name(&name) && !in_passing(&name) {
                let origin = origin(&path);
                library.take(path, origin);
            }
        }
    }
    library.files.sort_by(|a, b| a.path.cmp(&b.path));
    library
}

/// The YouTube id a downloaded file's name carries — the eleven characters ahead of its extension,
/// after the `__` every YouTube template here puts before them; `None` for any other name (another
/// site's file, a sidecar, a half-written download).
fn youtube_id(name: &str) -> Option<&str> {
    let stem = VIDEO_EXTS.iter().chain(&AUDIO_EXTS).find_map(|ext| name.strip_suffix(ext))?;
    let cut = stem.len().checked_sub(11)?;
    let id = stem.get(cut..)?;
    let named = stem[..cut].ends_with("__") && id.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_');
    named.then_some(id)
}

/// Whether `name` ends in an extension a download writes ([`VIDEO_EXTS`], [`AUDIO_EXTS`]).
fn is_media_name(name: &str) -> bool {
    VIDEO_EXTS.iter().chain(&AUDIO_EXTS).any(|ext| name.ends_with(ext))
}

/// Whether `name` is one a file only has in passing: it holds the `__<id>.` that ends every YouTube
/// name here, but not at its end — a pass's temporary copy (`…__<id>.tagging.mkv`), a stream yt-dlp
/// has yet to merge (`…__<id>.f137.mp4`). Its metadata is someone else's, or not yet written.
fn in_passing(name: &str) -> bool {
    let id_byte = |byte: &u8| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_');
    youtube_id(name).is_none()
        && name.as_bytes().windows(14).any(|window| window.starts_with(b"__") && window[2..13].iter().all(id_byte) && window[13] == b'.')
}

/// Say what a repair passed over: the media files from other sites, by host, and those nothing
/// tells the origin of; then what the archives hold that no repair can reach — an archived YouTube
/// id with no file left under `root`, and the entries from other sites.
fn report_unreached(root: &Path, library: &Library) {
    if !library.elsewhere.is_empty() {
        let mut hosts: Vec<&str> = library.elsewhere.iter().map(|(_, host)| host.as_str()).collect();
        hosts.sort_unstable();
        hosts.dedup();
        println!(
            "repair: {} file(s) from other sites ({}) — passed over: a repair completes YouTube videos only",
            library.elsewhere.len(),
            hosts.join(", ")
        );
    }
    if library.unknown > 0 {
        println!(
            "repair: {} media file(s) say nothing of where they were downloaded from (no YouTube id in the \
             name, no source URL in the metadata) — passed over",
            library.unknown
        );
    }
    if library.unreadable > 0 {
        println!(
            "{}",
            style::problematic(&format!(
                "repair: {} media file(s) with no YouTube id in the name could not be read (ffprobe) to tell \
                 where they came from — passed over",
                library.unreadable
            ))
        );
    }
    let found: HashSet<&str> = library.files.iter().map(|found| found.id.as_str()).collect();
    let mut gone: Vec<&str> = Vec::new();
    let mut elsewhere = 0;
    for (extractor, id) in &library.archived {
        if extractor != "youtube" {
            elsewhere += 1;
        } else if !found.contains(id.as_str()) && !gone.contains(&id.as_str()) {
            gone.push(id);
        }
    }
    if !gone.is_empty() {
        let ids: Vec<String> = gone.iter().map(|id| format!("[{id}]")).collect();
        println!(
            "{}",
            style::problematic(&format!(
                "repair: archived, but with no file under {} (moved, deleted or renamed?): {}",
                root.display(),
                ids.join(" ")
            ))
        );
    }
    if elsewhere > 0 {
        println!(
            "repair: {elsewhere} archived from other sites — passed over: off YouTube, an archived id \
             names no page to ask about the video"
        );
    }
}

/// What kind of file a repair has in front of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Mkv,
    Audio,
    /// A video in another container, which tracks and tags are not added to.
    OtherVideo,
}

fn kind(file: &Path) -> Kind {
    if is_mkv(file) {
        Kind::Mkv
    } else if is_audio(file) {
        Kind::Audio
    } else {
        Kind::OtherVideo
    }
}

/// What a file holds, as far as a repair looks — read locally, with ffprobe.
struct Holds {
    /// Its covers — an mkv's.
    covers: Embedded,
    /// Its subtitle tracks' titles, a line per track ([`subtitle_title_listing`]) — an mkv's.
    title_listing: String,
    /// Its metadata tags: the container's, and an audio file's streams' too, where Ogg keeps them.
    tags: Vec<(String, String)>,
    chapters: usize,
}

/// Read what `file` holds; `None` if ffprobe could not.
fn inspect(file: &Path, kind: Kind) -> Option<Holds> {
    let (tags, chapters) = tags_and_chapters(file, kind == Kind::Audio)?;
    if kind == Kind::Audio {
        return Some(Holds { covers: Embedded::default(), title_listing: String::new(), tags, chapters });
    }
    Some(Holds { covers: embedded(file)?, title_listing: subtitle_title_listing(file)?, tags, chapters })
}

/// `file`'s metadata tags and chapter count, in one ffprobe pass — as JSON, since a description
/// runs over several lines: the container's tags, and its streams' too when `streams`.
fn tags_and_chapters(file: &Path, streams: bool) -> Option<(Vec<(String, String)>, usize)> {
    let entries = if streams { "format_tags:stream_tags" } else { "format_tags" };
    let listing = capture_stdout(
        crate::tools::ffprobe(),
        [
            OsString::from("-v"), "error".into(),
            "-show_entries".into(), entries.into(),
            "-show_chapters".into(),
            "-of".into(), "json".into(),
            file.as_os_str().to_owned(),
        ],
    )?;
    read_tags_and_chapters(&listing)
}

/// [`tags_and_chapters`]' listing read back.
fn read_tags_and_chapters(listing: &str) -> Option<(Vec<(String, String)>, usize)> {
    let probe = json::parse(listing)?;
    let tags_of = |section: &Json| -> Vec<(String, String)> {
        let tags = section.get("tags").map(Json::members).unwrap_or_default();
        tags.iter().map(|(key, value)| (key.clone(), value.as_str().unwrap_or_default().to_string())).collect()
    };
    let mut tags = probe.get("format").map(tags_of).unwrap_or_default();
    for stream in probe.get("streams").map(Json::items).unwrap_or_default() {
        tags.extend(tags_of(stream));
    }
    Some((tags, probe.get("chapters").map_or(0, |chapters| chapters.items().len())))
}

/// What a file lacks, against what YouTube says now.
#[derive(Debug, Default, PartialEq)]
struct Needs {
    cover: Option<CoverNeed>,
    /// Auto-generated tracks still under YouTube's own names: `(subtitle order, title to give)`.
    stamps: Vec<(usize, String)>,
    /// The subtitle tracks — an audio file's, tags — a download would embed that it does not hold.
    subtitles: Vec<Pick>,
    /// The metadata tags a download embeds that it does not hold, with their values.
    tags: Vec<(&'static str, String)>,
    /// YouTube's chapters, when the file has none.
    chapters: Vec<Chapter>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoverNeed {
    Missing,
    /// A remux made a plain video track of it.
    Demoted,
}

impl Needs {
    fn is_empty(&self) -> bool {
        *self == Needs::default()
    }

    /// What is missing, as one line's worth: `thumbnail, subtitles (en, ko), metadata (title, date)`.
    fn describe(&self) -> String {
        let mut said: Vec<String> = Vec::new();
        match self.cover {
            Some(CoverNeed::Missing) => said.push("thumbnail".into()),
            Some(CoverNeed::Demoted) => said.push("thumbnail (a remux made a plain video track of it)".into()),
            None => {}
        }
        if !self.stamps.is_empty() {
            said.push(format!("auto-generated mark on {} subtitle track(s)", self.stamps.len()));
        }
        if !self.subtitles.is_empty() {
            said.push(format!("subtitles ({})", keys(&self.subtitles)));
        }
        if !self.tags.is_empty() {
            let names: Vec<&str> = self.tags.iter().map(|(key, _)| *key).collect();
            said.push(format!("metadata ({})", names.join(", ")));
        }
        if !self.chapters.is_empty() {
            said.push(format!("chapters ({})", self.chapters.len()));
        }
        said.join(", ")
    }
}

/// The language keys of `picks`, listed: `en, ko`.
fn keys(picks: &[Pick]) -> String {
    picks.iter().map(|pick| pick.key.as_str()).collect::<Vec<_>>().join(", ")
}

/// What `holds` lacks of what a download of the video embeds now (`facts`; without them, only its
/// cover can be judged). Its extension says which tags and chapters it could hold at all ([`KEEPS`]).
fn needs(file: &Path, kind: Kind, holds: &Holds, facts: Option<&Facts>, thumbnails: bool) -> Needs {
    let mut needs = Needs::default();
    if kind == Kind::Mkv && thumbnails {
        if holds.covers.covers.iter().any(|cover| cover.demoted) {
            needs.cover = Some(CoverNeed::Demoted);
        } else if holds.covers.covers.is_empty() {
            needs.cover = Some(CoverNeed::Missing);
        }
    }
    let Some(facts) = facts else { return needs };
    let has_tag = |name: &str| holds.tags.iter().any(|(key, _)| key.eq_ignore_ascii_case(name));
    if kind == Kind::Mkv {
        needs.stamps = auto_title_stamps(&holds.title_listing, &facts.picks);
        let titles: Vec<String> = holds
            .title_listing
            .lines()
            .enumerate()
            .map(|(order, title)| {
                let stamped = needs.stamps.iter().find(|(stamp, _)| *stamp == order);
                stamped.map_or_else(|| title.trim().to_string(), |(_, stamped)| stamped.clone())
            })
            .collect();
        needs.subtitles = facts
            .picks
            .iter()
            .filter(|pick| {
                let title = subtitle_title(pick);
                !title.is_empty() && !titles.contains(&title)
            })
            .cloned()
            .collect();
    } else {
        needs.subtitles = facts.picks.iter().filter(|pick| !has_tag(&subtitle_tag_name(pick))).cloned().collect();
    }
    let name = file.file_name().unwrap_or_default().to_string_lossy();
    let (dropped, keeps_chapters) = KEEPS
        .iter()
        .find(|(ext, ..)| name.ends_with(ext))
        .map_or((&[][..], false), |(_, dropped, chapters)| (*dropped, *chapters));
    needs.tags = facts
        .tags
        .iter()
        .filter(|(key, _)| !dropped.contains(key) && !has_tag(key))
        .cloned()
        .collect();
    if keeps_chapters && holds.chapters == 0 {
        needs.chapters = facts.chapters.clone();
    }
    needs
}

/// How one file came out of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// It holds everything a download would.
    Complete,
    /// It lacked something, all of it now added.
    Completed,
    /// Something it lacked could not be added.
    Incomplete,
    /// It lacks something (`--check`: nothing is changed).
    Lacking,
    /// YouTube did not answer for it, so its subtitles, metadata and chapters went unchecked.
    Unchecked,
    /// ffprobe could not read it.
    Unreadable,
    /// A video container nothing is added to.
    PassedOver,
}

/// Check one file and — unless `check` — add what it lacks; with the lines to print for it.
fn repair_one(found: &Found, facts: Option<&Facts>, unanswered: Option<&str>, env: Env, check: bool) -> (Outcome, Vec<String>) {
    let kind = kind(&found.path);
    if kind == Kind::OtherVideo {
        let line = "not an mkv — thumbnails, subtitle tracks and tags are added to mkv videos and audio files only";
        return (Outcome::PassedOver, vec![style::problematic(line)]);
    }
    let Some(holds) = inspect(&found.path, kind) else {
        return (Outcome::Unreadable, vec![style::problematic("could not read it (ffprobe) — left as it is")]);
    };
    let needs = needs(&found.path, kind, &holds, facts, env.thumbnail);
    let mut lines: Vec<String> = Vec::new();
    if let Some(why) = unanswered.filter(|_| facts.is_none()) {
        lines.push(style::problematic(&format!(
            "YouTube did not answer ({why}) — its subtitles, metadata and chapters went unchecked"
        )));
    }
    if !needs.is_empty() {
        lines.push(style::problematic(&format!("missing: {}", needs.describe())));
    }
    let failed = !check && !needs.is_empty() && !fix(found, kind, &holds, &needs, env, &mut lines);
    let outcome = if failed {
        Outcome::Incomplete
    } else if facts.is_none() {
        Outcome::Unchecked
    } else if needs.is_empty() {
        Outcome::Complete
    } else if check {
        Outcome::Lacking
    } else {
        Outcome::Completed
    };
    (outcome, lines)
}

/// Add what `found` lacks, a line apiece onto `lines`. Returns whether all of it went in.
fn fix(found: &Found, kind: Kind, holds: &Holds, needs: &Needs, env: Env, lines: &mut Vec<String>) -> bool {
    let file = &found.path;
    let mut whole = true;
    let mut said = |done: bool, line: String| {
        whole &= done;
        lines.push(if done { style::approved(&line) } else { style::problematic(&line) });
    };
    match needs.cover {
        Some(CoverNeed::Demoted) => {
            let done = repair_covers(file, &holds.covers);
            said(done, if done { "thumbnail restored".into() } else { "thumbnail: could not restore it (ffmpeg)".into() });
        }
        Some(CoverNeed::Missing) => {
            let thumb = file.with_extension("cover.jpg");
            if !fetch_youtube_thumbnail(&found.id, &thumb) {
                said(false, "thumbnail: could not fetch it from YouTube's image server".into());
            } else {
                let done = attach_thumbnail(file, &thumb);
                said(done, if done { "thumbnail embedded".into() } else { "thumbnail: could not embed it (ffmpeg)".into() });
            }
            let _ = std::fs::remove_file(&thumb);
        }
        None => {}
    }
    if !needs.stamps.is_empty() {
        let done = apply_title_stamps(file, &needs.stamps);
        let count = needs.stamps.len();
        said(
            done,
            if done {
                format!("marked {count} subtitle track(s) auto-generated")
            } else {
                format!("could not mark {count} subtitle track(s) auto-generated (ffmpeg)")
            },
        );
    }
    if !needs.subtitles.is_empty() {
        let (done, line) = put_back_subtitles(found, kind, holds, &needs.subtitles, env);
        said(done, line);
    }
    if !needs.tags.is_empty() || !needs.chapters.is_empty() {
        let done = add_tags_and_chapters(file, &needs.tags, &needs.chapters);
        let mut what: Vec<String> = Vec::new();
        if !needs.tags.is_empty() {
            what.push(format!("metadata ({})", needs.tags.iter().map(|(key, _)| *key).collect::<Vec<_>>().join(", ")));
        }
        if !needs.chapters.is_empty() {
            what.push(format!("{} chapter(s)", needs.chapters.len()));
        }
        let what = what.join(" and ");
        said(done, if done { format!("added {what}") } else { format!("could not write {what} (ffmpeg)") });
    }
    whole
}

/// Fetch the subtitle tracks `missing` names and put them into `found`: as tracks into an mkv, as
/// tags into an audio file. Returns whether every one went in, and the line saying how it went.
fn put_back_subtitles(found: &Found, kind: Kind, holds: &Holds, missing: &[Pick], env: Env) -> (bool, String) {
    let dir = found.path.parent().unwrap_or(Path::new("."));
    let wanted: Vec<String> = missing.iter().map(|pick| pick.key.clone()).collect();
    if !fetch_subtitles(&facts::watch_url(&found.id), &found.id, &wanted, dir, env) {
        return (false, format!("subtitles: could not fetch {} (yt-dlp failed)", keys(missing)));
    }
    let sidecars: Vec<(&Pick, PathBuf)> =
        missing.iter().map(|pick| (pick, subtitle_sidecar(dir, &found.id, &pick.key))).collect();
    let (arrived, lost): (Vec<_>, Vec<_>) = sidecars.into_iter().partition(|(_, sidecar)| sidecar.is_file());
    let lost: Vec<Pick> = lost.into_iter().map(|(pick, _)| pick.clone()).collect();
    if arrived.is_empty() {
        return (false, format!("subtitles: none of {} arrived (rate-limited?)", keys(&lost)));
    }
    let embedded = match kind {
        Kind::Audio => {
            let pairs: Vec<(String, PathBuf)> =
                arrived.iter().map(|(pick, sidecar)| (subtitle_tag_name(pick), sidecar.clone())).collect();
            write_subtitle_tags(&found.path, &pairs).is_ok()
        }
        _ => mux_subtitles(&found.path, &arrived, holds.title_listing.lines().count()),
    };
    for (_, sidecar) in &arrived {
        let _ = std::fs::remove_file(sidecar);
    }
    let put: Vec<Pick> = arrived.iter().map(|(pick, _)| (*pick).clone()).collect();
    if !embedded {
        return (false, format!("subtitles: could not embed {} (ffmpeg)", keys(&put)));
    }
    if lost.is_empty() {
        (true, format!("subtitles embedded: {}", keys(&put)))
    } else {
        (false, format!("subtitles embedded: {}; {} did not arrive (rate-limited?)", keys(&put), keys(&lost)))
    }
}

/// Write `tags` and, when there are any, `chapters` into `file`, in one stream-copy rewrite keeping
/// every stream, tag and attachment it has — an mkv's covers carried ([`CoverCarry`]) — renamed over
/// it once the rewrite succeeded. Returns success; on failure the file is as it was.
fn add_tags_and_chapters(file: &Path, tags: &[(&str, String)], chapters: &[Chapter]) -> bool {
    let ext = file.extension().unwrap_or_default().to_string_lossy();
    let out = file.with_extension(format!("tagging.{ext}"));
    let chapter_file = file.with_extension("chapters.ffmeta");
    if !chapters.is_empty() && std::fs::write(&chapter_file, ffmetadata(chapters)).is_err() {
        return false;
    }
    let carry = if is_mkv(file) {
        embedded(file).map(|embedded| CoverCarry::take(file, &embedded)).unwrap_or_default()
    } else {
        CoverCarry::default()
    };
    let mut argv: Vec<OsString> = ["-nostdin", "-v", "error", "-y", "-i"].map(OsString::from).to_vec();
    argv.push(file.as_os_str().to_owned());
    if !chapters.is_empty() {
        argv.extend(["-f", "ffmetadata", "-i"].map(OsString::from));
        argv.push(chapter_file.as_os_str().to_owned());
    }
    argv.extend(["-map", "0", "-c", "copy"].map(OsString::from));
    if !chapters.is_empty() {
        argv.extend(["-map_chapters", "1"].map(OsString::from));
    }
    argv.extend(carry.args());
    for (key, value) in tags {
        argv.push("-metadata".into());
        argv.push(format!("{key}={value}").into());
    }
    argv.push(out.as_os_str().to_owned());
    let done = crate::util::exec::run_ok(crate::tools::ffmpeg(), &argv) && std::fs::rename(&out, file).is_ok();
    carry.clean();
    let _ = std::fs::remove_file(&chapter_file);
    if !done {
        let _ = std::fs::remove_file(&out);
    }
    done
}

/// `chapters` as an ffmetadata file — ffmpeg's own text form, millisecond timestamps, each title's
/// `=`, `;`, `#`, `\` and line breaks escaped as that form needs them.
fn ffmetadata(chapters: &[Chapter]) -> String {
    let escape = |text: &str| {
        let mut escaped = String::new();
        for ch in text.chars() {
            if matches!(ch, '=' | ';' | '#' | '\\' | '\n') {
                escaped.push('\\');
            }
            escaped.push(ch);
        }
        escaped
    };
    let millis = |seconds: f64| (seconds * 1000.0).round() as i64;
    let mut text = String::from(";FFMETADATA1\n");
    for chapter in chapters {
        text += &format!(
            "[CHAPTER]\nTIMEBASE=1/1000\nSTART={}\nEND={}\ntitle={}\n",
            millis(chapter.start),
            millis(chapter.end),
            escape(&chapter.title)
        );
    }
    text
}

/// A run's outcomes, counted.
#[derive(Debug, Default)]
struct Totals {
    complete: usize,
    completed: usize,
    incomplete: usize,
    lacking: usize,
    unchecked: usize,
    unreadable: usize,
    passed_over: usize,
}

impl Totals {
    fn count(&mut self, outcome: Outcome) {
        let counter = match outcome {
            Outcome::Complete => &mut self.complete,
            Outcome::Completed => &mut self.completed,
            Outcome::Incomplete => &mut self.incomplete,
            Outcome::Lacking => &mut self.lacking,
            Outcome::Unchecked => &mut self.unchecked,
            Outcome::Unreadable => &mut self.unreadable,
            Outcome::PassedOver => &mut self.passed_over,
        };
        *counter += 1;
    }

    /// Whether a file was left lacking what it could hold, or unchecked — the run's exit is 1.
    fn left_wanting(&self) -> bool {
        self.incomplete + self.lacking + self.unchecked + self.unreadable > 0
    }

    /// The closing line's counts — every one that isn't zero; checking, said to have changed nothing.
    fn summary(&self, check: bool) -> String {
        // Destructured, not read field by field, so a new count cannot be left out of the line.
        let Totals { complete, completed, incomplete, lacking, unchecked, unreadable, passed_over } = *self;
        let counts = [
            (complete, "already complete"),
            (completed, "completed"),
            (lacking, "lacking something"),
            (incomplete, "left incomplete"),
            (unchecked, "not checked"),
            (unreadable, "unreadable"),
            (passed_over, "passed over (no mkv)"),
        ];
        let said: Vec<String> =
            counts.iter().filter(|(count, _)| *count > 0).map(|(count, what)| format!("{count} {what}")).collect();
        let said = said.join(", ");
        if check { format!("{said} — nothing was changed") } else { said }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_youtube_download_is_known_by_the_id_ending_its_name() {
        assert_eq!(youtube_id("20200215__Plash Speed Router (Eng Sub)__Q591qHMJgSI.mkv"), Some("Q591qHMJgSI"));
        assert_eq!(youtube_id("003__20200215__A__B__a-b_c123456.opus"), Some("a-b_c123456"), "a collection's entry, `__` in its title");
        for other in [
            "Some Video [1234567890a].mp4",          // another site's name
            "x__Q591qHMJgSI.mkv.part",               // half written
            "x__Q591qHMJgSI.en.vtt",                 // a sidecar
            "x__Q591qHMJgSI.thumbing.mkv",           // a pass's own temp file
            "_Q591qHMJgSI.mkv",                      // one underscore
            "x__Q591qHMJgS!.mkv",                    // no id's characters
            "x__short.mkv",
            "日本語日本語ab.mkv",                       // multi-byte, cut mid-character
        ] {
            assert_eq!(youtube_id(other), None, "{other}");
        }
    }

    fn tagged(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect()
    }

    #[test]
    fn a_file_comes_from_the_page_its_purl_or_else_its_comment_names() {
        let watch = "https://www.youtube.com/watch?v=Q591qHMJgSI";
        let video = Origin::YouTube("Q591qHMJgSI".into());
        assert_eq!(origin_in_tags(&tagged(&[("PURL", watch)])), video, "a key in any case");
        assert_eq!(origin_in_tags(&tagged(&[("comment", "https://youtu.be/Q591qHMJgSI")])), video, "the comment, without a purl");
        assert_eq!(origin_in_tags(&tagged(&[("comment", "https://vimeo.com/1"), ("purl", watch)])), video, "the purl first");
        assert_eq!(origin_in_tags(&tagged(&[("purl", "https://vimeo.com/76979871")])), Origin::Elsewhere("vimeo.com".into()));
        assert_eq!(
            origin_in_tags(&tagged(&[("purl", "https://example.com/watch?v=Q591qHMJgSI")])),
            Origin::Elsewhere("example.com".into()),
            "another site's `v=` is no YouTube id"
        );
        assert_eq!(origin_in_tags(&tagged(&[("purl", "https://www.youtube.com/@someone")])), Origin::Unknown, "a page naming no video");
        for words in ["my favourite — https://youtu.be/Q591qHMJgSI", "https://youtu.be/Q591qHMJgSI is my favourite"] {
            assert_eq!(origin_in_tags(&tagged(&[("comment", words)])), Origin::Unknown, "a URL among someone's words: {words}");
        }
        assert_eq!(origin_in_tags(&tagged(&[("description", watch), ("title", watch)])), Origin::Unknown, "only purl and comment say");
        assert_eq!(origin_in_tags(&[]), Origin::Unknown);
    }

    #[test]
    fn a_name_a_file_only_has_in_passing_is_never_taken_for_a_download() {
        for passing in ["x__Q591qHMJgSI.tagging.mkv", "x__Q591qHMJgSI.f137.mp4", "t___Q591qHMJgSI.temp.mkv"] {
            assert!(in_passing(passing), "{passing}");
        }
        for kept in ["x__Q591qHMJgSI.mkv", "Talk (final cut).mkv", "Some Clip [vimeo123].mp4", "a__b.mkv", "日本語__日本語語.mkv"] {
            assert!(!in_passing(kept), "{kept}");
        }
    }

    #[test]
    fn a_files_tags_and_chapters_read_from_ffprobe_json() {
        let listing = r#"{
            "streams": [{"index": 0, "tags": {"TITLE": "from the stream", "subtitles_en": "WEBVTT"}}],
            "chapters": [{"id": 0, "tags": {"title": "One"}}, {"id": 1}],
            "format": {"tags": {"title": "T", "DESCRIPTION": "a\nb"}}
        }"#;
        let (tags, chapters) = read_tags_and_chapters(listing).expect("ffprobe's JSON");
        assert_eq!(chapters, 2);
        let keys: Vec<&str> = tags.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["title", "DESCRIPTION", "TITLE", "subtitles_en"], "the container's, then each stream's");
        assert_eq!(tags[1].1, "a\nb");
        assert_eq!(read_tags_and_chapters("{}"), Some((Vec::new(), 0)), "nothing is no failure");
        assert_eq!(read_tags_and_chapters("not json"), None);
    }

    fn pick(key: &str, name: &str, auto: bool) -> Pick {
        Pick { key: key.into(), name: name.into(), auto }
    }

    fn facts_of(picks: Vec<Pick>, chapters: usize) -> Facts {
        Facts {
            id: "idA00000001".into(),
            picks,
            tags: ["title", "date", "description", "synopsis", "purl", "comment", "artist"]
                .map(|key| (key, format!("{key}-value")))
                .to_vec(),
            chapters: (0..chapters).map(|n| Chapter { start: n as f64, end: n as f64 + 1.0, title: format!("c{n}") }).collect(),
        }
    }

    fn holding(covers: Vec<bool>, titles: &str, tags: &[&str], chapters: usize) -> Holds {
        Holds {
            covers: Embedded {
                covers: covers
                    .into_iter()
                    .enumerate()
                    .map(|(index, demoted)| super::super::EmbeddedCover { index: index as u32, codec: "mjpeg".into(), demoted })
                    .collect(),
                attachments: 0,
            },
            title_listing: titles.into(),
            tags: tags.iter().map(|key| (key.to_string(), "kept".to_string())).collect(),
            chapters,
        }
    }

    #[test]
    fn a_complete_file_lacks_nothing() {
        let holds = holding(vec![false], "English\nKorean (auto-generated)\n", &["TITLE", "DATE", "DESCRIPTION", "SYNOPSIS", "PURL", "COMMENT", "ARTIST"], 3);
        let facts = facts_of(vec![pick("en", "English", false), pick("ko-orig", "Korean (Original)", true)], 3);
        assert!(needs(Path::new("v.mkv"), Kind::Mkv, &holds, Some(&facts), true).is_empty());
    }

    #[test]
    fn what_an_mkv_lacks_is_read_against_what_youtube_offers_now() {
        let facts = facts_of(vec![pick("en", "English", false), pick("ko", "Korean", true)], 2);
        // No cover, an unstamped auto track, English missing, three tags gone, no chapters.
        let holds = holding(vec![], "Korean\n", &["title", "DESCRIPTION", "SYNOPSIS", "COMMENT"], 0);
        let found = needs(Path::new("v.mkv"), Kind::Mkv, &holds, Some(&facts), true);
        assert_eq!(found.cover, Some(CoverNeed::Missing));
        assert_eq!(found.stamps, [(0, "Korean (auto-generated)".to_string())], "stamped, not fetched twice");
        assert_eq!(keys(&found.subtitles), "en");
        let tags: Vec<&str> = found.tags.iter().map(|(key, _)| *key).collect();
        assert_eq!(tags, ["date", "purl", "artist"], "present tags, in any case, are kept — whatever their value");
        assert_eq!(found.chapters.len(), 2);
        assert_eq!(
            found.describe(),
            "thumbnail, auto-generated mark on 1 subtitle track(s), subtitles (en), metadata (date, purl, artist), chapters (2)"
        );
        let demoted = needs(Path::new("v.mkv"), Kind::Mkv, &holding(vec![false, true], "", &[], 1), None, true);
        assert_eq!(demoted.cover, Some(CoverNeed::Demoted), "one demoted copy is enough to restore");
        assert!(needs(Path::new("v.mkv"), Kind::Mkv, &holding(vec![], "", &[], 0), None, false).is_empty(), "--no-thumbnail");
    }

    #[test]
    fn without_youtubes_answer_only_the_cover_is_judged() {
        let found = needs(Path::new("v.mkv"), Kind::Mkv, &holding(vec![], "", &[], 0), None, true);
        assert_eq!(found, Needs { cover: Some(CoverNeed::Missing), ..Needs::default() });
    }

    #[test]
    fn an_audio_file_is_held_to_its_tags_and_what_its_container_keeps() {
        let facts = facts_of(vec![pick("en", "English", false), pick("ko", "Korean", true)], 2);
        let holds = holding(vec![], "", &["subtitles_en", "TITLE"], 0);
        let opus = needs(Path::new("s.opus"), Kind::Audio, &holds, Some(&facts), true);
        assert_eq!(opus.cover, None, "no cover is put into audio");
        assert_eq!(keys(&opus.subtitles), "ko", "an audio file's subtitles are its tags");
        let tags: Vec<&str> = opus.tags.iter().map(|(key, _)| *key).collect();
        assert_eq!(tags, ["date", "synopsis", "purl", "comment", "artist"], "no description, which Ogg cannot hold");
        assert_eq!(opus.chapters.len(), 2, "opus keeps chapters");
        let ogg = needs(Path::new("s.ogg"), Kind::Audio, &holds, Some(&facts), true);
        assert!(ogg.chapters.is_empty(), "ogg does not");
        let m4a = needs(Path::new("s.m4a"), Kind::Audio, &holds, Some(&facts), true);
        assert!(m4a.tags.iter().all(|(key, _)| *key != "purl"), "m4a cannot hold purl");
        let wav = needs(Path::new("s.wav"), Kind::Audio, &holds, Some(&facts), true);
        let tags: Vec<&str> = wav.tags.iter().map(|(key, _)| *key).collect();
        assert_eq!(tags, ["date", "comment", "artist"]);
    }

    #[test]
    fn chapters_are_written_in_ffmpegs_own_form_escaped_as_it_needs() {
        let chapters = [
            Chapter { start: 0.0, end: 12.5, title: "Intro".into() },
            Chapter { start: 12.5, end: 171.1304, title: "a=b; #c \\ d\ne".into() },
        ];
        assert_eq!(
            ffmetadata(&chapters),
            ";FFMETADATA1\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=12500\ntitle=Intro\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=12500\nEND=171130\ntitle=a\\=b\\; \\#c \\\\ d\\\ne\n"
        );
    }

    #[test]
    fn the_closing_line_counts_what_happened_and_sets_the_exit() {
        let mut totals = Totals::default();
        for outcome in [Outcome::Complete, Outcome::Complete, Outcome::Completed, Outcome::PassedOver] {
            totals.count(outcome);
        }
        assert_eq!(totals.summary(false), "2 already complete, 1 completed, 1 passed over (no mkv)");
        assert!(!totals.left_wanting(), "an mkv-less video is said, not failed on");
        for wanting in [Outcome::Incomplete, Outcome::Lacking, Outcome::Unchecked, Outcome::Unreadable] {
            let mut totals = Totals::default();
            totals.count(wanting);
            assert!(totals.left_wanting(), "{wanting:?}");
        }
        let mut checked = Totals::default();
        checked.count(Outcome::Lacking);
        assert_eq!(checked.summary(true), "1 lacking something — nothing was changed");
    }
}
