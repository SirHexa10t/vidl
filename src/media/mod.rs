//! Everything that touches the files on disk once yt-dlp has written them: cover art
//! (thumbnails), embedded subtitle tracks, and audio subtitle tags — idempotent ffmpeg/lofty
//! passes, safe to re-run across a whole collection.
//!
//! [`subtitles`] is the planner for that last part: which caption tracks a given video *should*
//! have, chosen from YouTube's ~157-language matrix, before anything is fetched or muxed.

pub(crate) mod subtitles;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::util::exec::{capture_output, capture_stdout};
use crate::Env;
use crate::ytdlp::{seeded, ytdlp_invocation};
use crate::ytdlp::scan::{ScanEntry};
use self::subtitles::{Pick, batch_probe};
use crate::util::style;

/// Rename the embedded titles of auto-generated subtitle tracks. YouTube's own names read like
/// authentic tracks — "Spanish (Original)" sounds MORE official than the uploader's, and a
/// translated track is often titled plain "English" — so every auto pick's track becomes
/// "<name> (auto-generated)", with the misleading " (Original)" dropped. A local stream-copy
/// remux (no re-encode, no network); any failure leaves the downloaded file as-is.
pub(crate) fn mark_auto_titles(root: &Path, id: &str, picks: &[Pick], env: Env) {
    let autos: Vec<&Pick> = picks.iter().filter(|pick| pick.auto).collect();
    if autos.is_empty() || env.audio {
        return;
    }
    let Some(file) = find_by_id(root, id) else { return };
    let ffprobe = crate::tools::ffprobe();
    let Some(listing) = capture_stdout(
        &ffprobe,
        [
            OsString::from("-v"), "error".into(),
            "-select_streams".into(), "s".into(),
            "-show_entries".into(), "stream_tags=title".into(),
            "-of".into(), "csv=p=0".into(),
            file.as_os_str().to_owned(),
        ],
    ) else {
        eprintln!(
            "vidl: could not read {}'s subtitle titles (ffprobe) — auto-generated stamps skipped",
            file.display()
        );
        return;
    };
    let retitle = auto_title_stamps(&listing, picks);
    if retitle.is_empty() {
        return;
    }
    let ffmpeg = crate::tools::ffmpeg();
    let stamped = file.with_extension("stamping.mkv");
    let mut argv: Vec<OsString> = ["-nostdin", "-v", "error", "-y", "-i"].map(OsString::from).to_vec();
    argv.push(file.as_os_str().to_owned());
    // NOTE: a bare `-map 0 -c copy` remux DEMOTES an attached cover (see [`mux_subtitles`],
    // which extracts and re-attaches for exactly that reason). Safe here only by call order:
    // this runs straight after a fresh download, before any `--thumbnail` pass could have
    // attached one — keep it that way, or adopt the mux's cover-carry.
    argv.extend(["-map", "0", "-c", "copy"].map(OsString::from));
    for (order, title) in &retitle {
        argv.push(format!("-metadata:s:s:{order}").into());
        argv.push(format!("title={title}").into());
    }
    argv.push(stamped.as_os_str().to_owned());
    let renamed = crate::util::exec::run_ok(&ffmpeg, &argv)
        && std::fs::rename(&stamped, &file).is_ok();
    if renamed {
        for (_, title) in &retitle {
            println!("stamped subtitle track: {title}");
        }
    } else {
        let _ = std::fs::remove_file(&stamped);
        eprintln!("vidl: could not stamp auto-subtitle titles in {}", file.display());
    }
}

/// The (subtitle-stream order, stamped title) rewrites for `listing` — ffprobe's one-title-per-
/// line output, in stream order. A title matching an auto pick's probed name is stamped
/// directly; but yt-dlp names tracks per invocation, and the probe and the download are TWO
/// invocations — the same `ja-orig` track has arrived titled "Japanese" in one session and
/// "Japanese (Original)" in another, so name-matching alone silently misses. When every pick
/// is machine-made anyway (nothing manual was requested), every stream is therefore stamped
/// regardless of its name, keeping whatever title it carries. Mixed manual+auto picks stay
/// name-matched — with unrecognizable names there is no safe way to tell the tracks apart.
/// Already-marked titles are left alone, so re-stamping is idempotent.
fn auto_title_stamps(listing: &str, picks: &[Pick]) -> Vec<(usize, String)> {
    let autos: Vec<&Pick> = picks.iter().filter(|pick| pick.auto).collect();
    let all_auto = autos.len() == picks.len();
    let mut stamps = Vec::new();
    for (order, title) in listing.lines().enumerate() {
        let title = title.trim();
        if title.ends_with("(auto-generated)") {
            continue;
        }
        if autos.iter().any(|pick| pick.name == title) || all_auto {
            let base = if title.is_empty() { "subtitles" } else { title };
            stamps.push((order, stamped_title(base)));
        }
    }
    stamps
}

/// Fold the kept `.vtt` sidecars into an audio file's metadata: each becomes a tag named
/// [`subtitle_tag_name`]-style (`subtitles_en`, `subtitles_iw_autogenerated`, …) holding the
/// full VTT text, and the sidecar is deleted. Audio containers can't carry subtitle streams,
/// but their tags hold text fine — the words travel with the sound. Tags are written IN PLACE
/// by lofty, in-process (a remux would drag the embedded cover art through container rules —
/// ogg refuses picture streams); missing sidecars (a 429'd track) are simply skipped: picks
/// are must-try.
pub(crate) fn embed_subtitle_tags(root: &Path, id: &str, picks: &[Pick]) {
    if picks.is_empty() {
        return;
    }
    let Some(file) = find_by_id(root, id) else { return };
    let pairs = subtitle_sidecars(&file, picks);
    if pairs.is_empty() {
        return;
    }
    match write_subtitle_tags(&file, &pairs) {
        Ok(()) => {
            let names: Vec<&str> = pairs.iter().map(|(name, _)| name.as_str()).collect();
            for (_, sidecar) in &pairs {
                let _ = std::fs::remove_file(sidecar);
            }
            println!("embedded subtitle tags: {}", names.join(", "));
        }
        Err(why) => eprintln!(
            "vidl: could not embed subtitle tags into {} ({why}) — sidecar .vtt files kept",
            file.display()
        ),
    }
}

/// The in-place tag writer (lofty, per-container): a plain key in Vorbis comments (opus/ogg/
/// flac), a TXXX frame on ID3 (mp3/wav), and a `----:vidl:<name>` freeform atom on MP4 (m4a).
///
/// MP4 has no user-defined text frame, so a custom tag must be namespaced by a vendor string —
/// that is what `vidl` is doing in the atom name, and why it is a written-once constant rather
/// than anything configurable. Nothing reads it back (the idempotency checks go through track
/// titles and ffprobe), so it is a label for whoever opens the file, not a protocol.
///
/// Existing tags (yt-dlp's embedded metadata) are read first and written back with the additions.
fn write_subtitle_tags(file: &Path, pairs: &[(String, PathBuf)]) -> Result<(), String> {
    use lofty::config::{ParseOptions, WriteOptions};
    use lofty::file::{AudioFile, FileType};
    use lofty::tag::TagExt;

    let mut texts: Vec<(String, String)> = Vec::new();
    for (name, sidecar) in pairs {
        let text = std::fs::read_to_string(sidecar).map_err(|err| err.to_string())?;
        texts.push((name.clone(), text));
    }
    let file_type = lofty::probe::Probe::open(file)
        .map_err(|err| err.to_string())?
        .guess_file_type()
        .map_err(|err| err.to_string())?
        .file_type()
        .ok_or("unrecognized audio container")?;
    let mut reader = std::fs::File::open(file).map_err(|err| err.to_string())?;
    let parse = ParseOptions::new();
    let write = WriteOptions::default();
    let vorbis = |tag: &mut lofty::ogg::VorbisComments, texts: Vec<(String, String)>| {
        for (name, text) in texts {
            tag.push(name, text);
        }
    };
    match file_type {
        FileType::Opus => {
            let mut audio = lofty::ogg::OpusFile::read_from(&mut reader, parse)
                .map_err(|err| err.to_string())?;
            vorbis(audio.vorbis_comments_mut(), texts);
            audio.vorbis_comments().save_to_path(file, write).map_err(|err| err.to_string())
        }
        FileType::Vorbis => {
            let mut audio = lofty::ogg::VorbisFile::read_from(&mut reader, parse)
                .map_err(|err| err.to_string())?;
            vorbis(audio.vorbis_comments_mut(), texts);
            audio.vorbis_comments().save_to_path(file, write).map_err(|err| err.to_string())
        }
        FileType::Flac => {
            let mut audio = lofty::flac::FlacFile::read_from(&mut reader, parse)
                .map_err(|err| err.to_string())?;
            if audio.vorbis_comments().is_none() {
                audio.set_vorbis_comments(lofty::ogg::VorbisComments::default());
            }
            let tag = audio.vorbis_comments_mut().expect("tag ensured above");
            vorbis(tag, texts);
            tag.save_to_path(file, write).map_err(|err| err.to_string())
        }
        FileType::Mpeg | FileType::Wav => {
            // Both carry ID3v2; read via their own parsers so the tag offset is right.
            let mut id3 = if file_type == FileType::Mpeg {
                lofty::mpeg::MpegFile::read_from(&mut reader, parse)
                    .map_err(|err| err.to_string())?
                    .id3v2()
                    .cloned()
            } else {
                lofty::iff::wav::WavFile::read_from(&mut reader, parse)
                    .map_err(|err| err.to_string())?
                    .id3v2()
                    .cloned()
            }
            .unwrap_or_default();
            for (name, text) in texts {
                id3.insert_user_text(name, text);
            }
            id3.save_to_path(file, write).map_err(|err| err.to_string())
        }
        FileType::Mp4 => {
            use lofty::mp4::{Atom, AtomData, AtomIdent};
            let mut audio = lofty::mp4::Mp4File::read_from(&mut reader, parse)
                .map_err(|err| err.to_string())?;
            if audio.ilst().is_none() {
                audio.set_ilst(lofty::mp4::Ilst::new());
            }
            let ilst = audio.ilst_mut().expect("tag ensured above");
            for (name, text) in texts {
                let ident = AtomIdent::Freeform { mean: "vidl".into(), name: name.into() };
                ilst.insert(Atom::new(ident, AtomData::UTF8(text)));
            }
            ilst.save_to_path(file, write).map_err(|err| err.to_string())
        }
        other => Err(format!("unsupported audio container: {other:?}")),
    }
}

/// The per-file finishing pass, by mode: video files get their auto-subtitle track titles
/// stamped; audio files get the kept `.vtt` sidecars folded into metadata tags.
pub(crate) fn finish_media(root: &Path, id: &str, picks: &[Pick], env: Env) {
    if env.audio {
        embed_subtitle_tags(root, id, picks);
    } else {
        mark_auto_titles(root, id, picks, env);
    }
}

/// The sidecar `.vtt` files that actually arrived for `file`'s picks, paired with their tag
/// names — yt-dlp names each `<output-stem>.<lang-key>.vtt`. A missing sidecar (a refused
/// track) simply isn't listed: picks are must-try.
fn subtitle_sidecars(file: &Path, picks: &[Pick]) -> Vec<(String, PathBuf)> {
    let stem = file.with_extension("");
    picks
        .iter()
        .filter_map(|pick| {
            let sidecar = PathBuf::from(format!("{}.{}.vtt", stem.display(), pick.key));
            sidecar.is_file().then(|| (subtitle_tag_name(pick), sidecar))
        })
        .collect()
}

/// The metadata tag carrying one subtitle track: `subtitles_<key>` with the key lowercased and
/// `_`-joined, the redundant `_orig` marker folded into the `_autogenerated` suffix that every
/// auto track gets.
fn subtitle_tag_name(pick: &Pick) -> String {
    let key = pick.key.to_lowercase().replace('-', "_");
    let key = key.strip_suffix("_orig").unwrap_or(&key);
    if pick.auto {
        format!("subtitles_{key}_autogenerated")
    } else {
        format!("subtitles_{key}")
    }
}

/// An auto track's honest title: YouTube's name with the actively-misleading " (Original)"
/// dropped and the machine origin stated.
fn stamped_title(name: &str) -> String {
    format!("{} (auto-generated)", name.replace(" (Original)", ""))
}

/// The absolute stream index of the embedded cover in an ffprobe
/// `stream=index:stream_disposition=attached_pic -of default=nw=1` listing (`index=N` lines, each
/// followed by its dispositions), or `None` when no stream carries `attached_pic=1`. Pure, so the
/// idempotency gate and the remux cover-carry are unit-tested without ffprobe.
fn attached_pic_stream_index(listing: &str) -> Option<u32> {
    let mut current = None;
    for line in listing.lines() {
        if let Some(index) = line.strip_prefix("index=") {
            current = index.trim().parse().ok();
        } else if line.trim() == "DISPOSITION:attached_pic=1" {
            return current;
        }
    }
    None
}

/// The absolute stream index of `file`'s embedded thumbnail: `Some(Some(idx))` when one exists,
/// `Some(None)` when none does, `None` when ffprobe couldn't run — the caller then leaves the
/// file untouched rather than guess.
fn embedded_thumbnail_index(file: &Path) -> Option<Option<u32>> {
    let ffprobe = crate::tools::ffprobe();
    let listing = capture_stdout(
        &ffprobe,
        [
            OsString::from("-v"), "error".into(),
            "-show_entries".into(), "stream=index:stream_disposition=attached_pic".into(),
            "-of".into(), "default=nw=1".into(),
            file.as_os_str().to_owned(),
        ],
    )?;
    Some(attached_pic_stream_index(&listing))
}

/// Whether `file` already carries an embedded thumbnail. `None` when ffprobe couldn't run.
fn has_embedded_thumbnail(file: &Path) -> Option<bool> {
    Some(embedded_thumbnail_index(file)?.is_some())
}

/// Pull the attached cover (stream `index`) out of `file` — a one-packet stream copy into a temp
/// `.jpg` beside it — so a remux can re-`-attach` it. Covers here are always JPEG: both `dl`'s
/// own pass and yt-dlp's legacy inline embeds convert to jpg. `None` (temp cleaned) on failure.
fn extract_thumbnail(file: &Path, index: u32) -> Option<PathBuf> {
    let ffmpeg = crate::tools::ffmpeg();
    let tmp = file.with_extension("cover-keep.jpg");
    let mut argv: Vec<OsString> = ["-nostdin", "-v", "error", "-y", "-i"].map(OsString::from).to_vec();
    argv.push(file.as_os_str().to_owned());
    argv.push("-map".into());
    argv.push(format!("0:{index}").into());
    argv.extend(["-frames:v", "1", "-c", "copy"].map(OsString::from));
    argv.push(tmp.as_os_str().to_owned());
    let ok = crate::util::exec::run_ok(&ffmpeg, &argv) && tmp.is_file();
    if ok {
        Some(tmp)
    } else {
        let _ = std::fs::remove_file(&tmp);
        None
    }
}

/// Fetch YouTube video `id`'s thumbnail to `dest` (a `.jpg`), best quality first: `maxresdefault`
/// (HD, often absent for low-effort uploads), then `hqdefault` (always present). Uses `curl` — the
/// same fetcher `dl` uses for pages — hitting the two known URLs directly, so no yt-dlp launch and
/// no format-probe cascade. Returns whether a file landed.
fn fetch_youtube_thumbnail(id: &str, dest: &Path) -> bool {
    for quality in ["maxresdefault", "hqdefault"] {
        let url = format!("https://i.ytimg.com/vi/{id}/{quality}.jpg");
        let landed = capture_output(
            "curl",
            [OsString::from("-fsSL"), "-o".into(), dest.as_os_str().to_owned(), url.into()],
        )
        .is_some_and(|(ok, _, _)| ok);
        if landed {
            return true;
        }
    }
    false
}

/// Attach `thumb` into the mkv `file` as cover art (an mkv attachment, exactly as yt-dlp embeds
/// it), keeping every existing stream (`-map 0 -c copy`). Writes a sibling temp file and renames
/// over the original. Returns whether it succeeded.
fn attach_thumbnail(file: &Path, thumb: &Path) -> bool {
    let ffmpeg = crate::tools::ffmpeg();
    let out = file.with_extension("thumbing.mkv");
    let mut argv: Vec<OsString> = ["-nostdin", "-v", "error", "-y", "-i"].map(OsString::from).to_vec();
    argv.push(file.as_os_str().to_owned());
    argv.extend(["-map", "0", "-c", "copy", "-attach"].map(OsString::from));
    argv.push(thumb.as_os_str().to_owned());
    argv.extend(
        ["-metadata:s:t:0", "mimetype=image/jpeg", "-metadata:s:t:0", "filename=cover.jpg"]
            .map(OsString::from),
    );
    argv.push(out.as_os_str().to_owned());
    let ok = crate::util::exec::run_ok(&ffmpeg, &argv) && std::fs::rename(&out, file).is_ok();
    if !ok {
        let _ = std::fs::remove_file(&out);
    }
    ok
}

/// Fetch and embed `id`'s thumbnail into `file`, cleaning up the temp image. Returns success.
fn embed_one_thumbnail(file: &Path, id: &str) -> bool {
    let thumb = file.with_extension("cover.jpg");
    let ok = fetch_youtube_thumbnail(id, &thumb) && attach_thumbnail(file, &thumb);
    let _ = std::fs::remove_file(&thumb);
    ok
}

/// The late, opt-in (`--thumbnail`) cover-art pass: for each video `id` under `dir`, scan and
/// report whether it already has an embedded thumbnail — so the user sees up front what's missing
/// rather than waiting on unbounded work — then fetch + embed only the ones lacking one.
/// Deliberately independent of the download archive, so a re-run *patches* previously-downloaded
/// videos; the embedded-thumbnail check keeps it idempotent (a video that already has one is never
/// re-fetched or re-embedded).
pub(crate) fn embed_thumbnails(dir: &Path, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    println!("thumbnails: scanning {} video(s)…", ids.len());
    let mut missing: Vec<(String, PathBuf)> = Vec::new();
    for id in ids {
        match find_by_id(dir, id) {
            None => println!("  [{id}]: {}", style::problematic("no file found — skipping")),
            // A cover embeds as an mkv attachment ([`attach_thumbnail`] muxes into the mkv
            // container) — renaming that over an audio/webm file would swap its container out
            // from under it, corrupting it. Skip anything that isn't an mkv, and say so.
            Some(file) if file.extension().and_then(|ext| ext.to_str()) != Some("mkv") => {
                println!("  [{id}]: {}", style::problematic("not an mkv — cover art embeds into video (mkv) only; skipping"));
            }
            Some(file) => match has_embedded_thumbnail(&file) {
                Some(true) => println!("  [{id}]: {}", style::approved("already has a thumbnail")),
                Some(false) => {
                    println!("  [{id}]: {}", style::problematic("missing a thumbnail"));
                    missing.push((id.clone(), file));
                }
                None => println!("  [{id}]: {}", style::problematic("could not read (ffprobe) — skipping")),
            },
        }
    }
    if missing.is_empty() {
        println!("thumbnails: all present — nothing to embed");
        return;
    }
    println!("thumbnails: fetching + embedding {} …", missing.len());
    for (id, file) in &missing {
        if embed_one_thumbnail(file, id) {
            println!("  [{id}]: {}", style::approved("embedded"));
        } else {
            eprintln!("  [{id}]: {}", style::problematic("could not embed a thumbnail"));
        }
    }
}

/// The titles of the subtitle streams in an ffprobe `-select_streams s -show_entries
/// stream=index:stream_tags=title -of default=nw=1` listing — one `TAG:title=…` line per titled
/// stream. Split out pure so the idempotency match is unit-tested without ffprobe.
fn subtitle_titles(listing: &str) -> Vec<String> {
    listing.lines().filter_map(|line| line.strip_prefix("TAG:title=").map(str::to_string)).collect()
}

/// `(subtitle-stream count, their titles)` for `file`, in one ffprobe pass. `None` if ffprobe
/// couldn't run — the caller then leaves the file untouched rather than guess. The count places
/// new tracks at the right output index when muxing; the titles are the idempotency check.
fn subtitle_streams(file: &Path) -> Option<(usize, Vec<String>)> {
    let ffprobe = crate::tools::ffprobe();
    let listing = capture_stdout(
        &ffprobe,
        [
            OsString::from("-v"), "error".into(),
            "-select_streams".into(), "s".into(),
            "-show_entries".into(), "stream=index:stream_tags=title".into(),
            "-of".into(), "default=nw=1".into(),
            file.as_os_str().to_owned(),
        ],
    )?;
    let count = listing.lines().filter(|line| line.starts_with("index=")).count();
    Some((count, subtitle_titles(&listing)))
}

/// The title an embedded track carries for `pick` — the auto-generated stamp for an auto track,
/// the plain name otherwise. This is what [`embed_subtitles`] matches on and writes.
fn subtitle_title(pick: &Pick) -> String {
    if pick.auto {
        stamped_title(&pick.name)
    } else {
        pick.name.clone()
    }
}

/// The fixed, id-based stem the subtitle patch fetches its sidecars to (`.dlsub-<id>.<key>.vtt`) —
/// deliberately NOT the video's own filename, so a title/date drift between the on-disk file and
/// yt-dlp's current template can't hide the freshly-fetched tracks.
fn subtitle_sidecar(into: &Path, id: &str, key: &str) -> PathBuf {
    into.join(format!(".dlsub-{id}.{key}.vtt"))
}

/// Fetch `keys`' subtitle sidecars for `url` (bundled yt-dlp, no media), written to the fixed
/// [`subtitle_sidecar`] paths. Returns whether yt-dlp ran.
fn fetch_subtitles(url: &str, id: &str, keys: &[String], into: &Path, env: Env) -> bool {
    let mut argv = seeded(env);
    argv.extend([
        OsString::from("--skip-download"),
        "--write-subs".into(),
        "--write-auto-subs".into(),
        "--sub-langs".into(),
        keys.join(",").into(),
        "--no-playlist".into(),
        "--output".into(),
        into.join(format!(".dlsub-{id}.%(ext)s")).into_os_string(),
        url.into(),
    ]);
    let (program, args) = ytdlp_invocation(argv);
    capture_stdout(program, args).is_some()
}

/// Mux the arrived subtitle sidecars into the mkv `file` in one ffmpeg pass, keeping every existing
/// stream and tagging each new track's language + title. `existing` is the file's current subtitle
/// count, so the new tracks land at the right output indices. An attached cover can't just ride
/// `-map 0` — a remux demotes it to a plain video track (the `attached_pic` disposition doesn't
/// survive an mkv round-trip) — so it's extracted first, excluded from the map, and re-`-attach`ed
/// in the same pass. Renames over the original. Returns success.
fn mux_subtitles(file: &Path, arrived: &[(&Pick, PathBuf)], existing: usize) -> bool {
    let ffmpeg = crate::tools::ffmpeg();
    let cover_index = embedded_thumbnail_index(file).flatten();
    // Extraction failing (odd, but possible) degrades to the old demote-the-cover behaviour —
    // the exclusion below is applied only when the re-attach is actually in hand.
    let cover = cover_index.and_then(|index| extract_thumbnail(file, index));
    let out = file.with_extension("subbing.mkv");
    let mut argv: Vec<OsString> = ["-nostdin", "-v", "error", "-y", "-i"].map(OsString::from).to_vec();
    argv.push(file.as_os_str().to_owned());
    for (_, sidecar) in arrived {
        argv.push("-i".into());
        argv.push(sidecar.as_os_str().to_owned());
    }
    if let Some(tmp) = &cover {
        argv.push("-attach".into());
        argv.push(tmp.as_os_str().to_owned());
    }
    argv.extend(["-map", "0"].map(OsString::from));
    if let (Some(index), Some(_)) = (cover_index, &cover) {
        argv.push("-map".into());
        argv.push(format!("-0:{index}").into());
    }
    for input in 1..=arrived.len() {
        argv.push("-map".into());
        argv.push(input.to_string().into());
    }
    argv.extend(["-c", "copy", "-c:s", "srt"].map(OsString::from));
    if cover.is_some() {
        argv.extend(
            ["-metadata:s:t:0", "mimetype=image/jpeg", "-metadata:s:t:0", "filename=cover.jpg"]
                .map(OsString::from),
        );
    }
    for (offset, (pick, _)) in arrived.iter().enumerate() {
        let idx = existing + offset;
        let lang = pick.key.split('-').next().unwrap_or(&pick.key);
        argv.push(format!("-metadata:s:s:{idx}").into());
        argv.push(format!("language={lang}").into());
        argv.push(format!("-metadata:s:s:{idx}").into());
        argv.push(format!("title={}", subtitle_title(pick)).into());
    }
    argv.push(out.as_os_str().to_owned());
    let ok = crate::util::exec::run_ok(&ffmpeg, &argv) && std::fs::rename(&out, file).is_ok();
    if let Some(tmp) = &cover {
        let _ = std::fs::remove_file(tmp); // the extracted cover was only for the re-attach
    }
    if !ok {
        let _ = std::fs::remove_file(&out);
    }
    ok
}

/// The late, opt-in (`--subtitles`) subtitle patch pass for one video: report which of its
/// `expected` tracks are already embedded, then fetch + mux only the missing ones. Idempotent (a
/// track whose title is already present is never re-fetched) and archive-independent (so a re-run
/// patches an already-downloaded video). Video-only: an mkv holds subtitle streams; audio files
/// carry their subtitles as metadata tags from the original download, so they're skipped here.
pub(crate) fn embed_subtitles(into: &Path, id: &str, url: &str, expected: &[Pick], env: Env) {
    let Some(file) = find_by_id(into, id) else {
        println!("  [{id}]: {}", style::problematic("no file found — skipping"));
        return;
    };
    if file.extension().and_then(|ext| ext.to_str()) != Some("mkv") {
        println!("  [{id}]: {}", style::approved("audio — subtitles kept as tags; nothing to patch"));
        return;
    }
    let Some((count, embedded)) = subtitle_streams(&file) else {
        println!("  [{id}]: {}", style::problematic("could not read (ffprobe) — skipping"));
        return;
    };
    let missing: Vec<&Pick> = expected
        .iter()
        .filter(|pick| {
            let title = subtitle_title(pick);
            !title.is_empty() && !embedded.contains(&title)
        })
        .collect();
    if missing.is_empty() {
        // "all 0 expected already embedded" would read absurd — a subtitle-less video gets its
        // own honest wording.
        let message = if expected.is_empty() {
            "no subtitles exist for this video — nothing to embed".to_string()
        } else {
            format!("all {} expected subtitle(s) already embedded", expected.len())
        };
        println!("  [{id}]: {}", style::approved(&message));
        return;
    }
    let want: Vec<&str> = missing.iter().map(|pick| pick.key.as_str()).collect();
    println!("  [{id}]: {}", style::problematic(&format!("missing subtitle(s): {}", want.join(", "))));
    let keys: Vec<String> = missing.iter().map(|pick| pick.key.clone()).collect();
    if !fetch_subtitles(url, id, &keys, into, env) {
        eprintln!("  [{id}]: {}", style::problematic("could not fetch subtitles"));
        return;
    }
    let arrived: Vec<(&Pick, PathBuf)> = missing
        .iter()
        .filter_map(|pick| {
            let sidecar = subtitle_sidecar(into, id, &pick.key);
            sidecar.is_file().then_some((*pick, sidecar))
        })
        .collect();
    if arrived.is_empty() {
        eprintln!("  [{id}]: {}", style::problematic("no subtitles arrived (rate-limited?)"));
        return;
    }
    let ok = mux_subtitles(&file, &arrived, count);
    for (_, sidecar) in &arrived {
        let _ = std::fs::remove_file(sidecar);
    }
    if ok {
        let done: Vec<&str> = arrived.iter().map(|(pick, _)| pick.key.as_str()).collect();
        println!("  [{id}]: {}", style::approved(&format!("embedded {}", done.join(", "))));
    } else {
        eprintln!("  [{id}]: {}", style::problematic("could not embed subtitles"));
    }
}

/// The collection `--subtitles` pass: one batch probe for every entry's expected tracks, then
/// [`embed_subtitles`] per entry — ignoring the archive, so already-downloaded entries get patched
/// too. `url` is the playlist/tab the entries belong to; `home` holds their files.
pub(crate) fn patch_collection_subtitles(url: &str, entries: &[&ScanEntry], home: &Path, env: Env) {
    if entries.is_empty() {
        return;
    }
    println!("subtitles: scanning {} video(s)…", entries.len());
    let indexes: Vec<String> = entries.iter().map(|entry| entry.index.clone()).collect();
    for plan in batch_probe(url, &indexes, env) {
        let watch = format!("https://www.youtube.com/watch?v={}", plan.id);
        embed_subtitles(home, &plan.id, &watch, &plan.picks, env);
    }
}

/// The downloaded file whose name carries `__<id>.` — the id rides in every output template
/// precisely so files stay findable. A shallow recursive walk under `root`.
pub(crate) fn find_by_id(root: &Path, id: &str) -> Option<PathBuf> {
    let marker = format!("__{id}.");
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.contains(&marker)
                    && [".mkv", ".opus", ".m4a", ".mp3", ".ogg", ".flac", ".wav", ".webm"]
                        .iter()
                        .any(|ext| name.ends_with(ext))
            }) {
                return Some(path);
            }
        }
    }
    None
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::Env;
    use crate::media::subtitles::{Pick};
    use crate::testutil::{ff_bin, ffmpeg_or_skip, scratch_dir};

    #[test]
    fn auto_stamps_survive_ytdlps_per_session_naming_drift() {
        let pick = |name: &str, auto: bool| Pick { key: "x".into(), name: name.into(), auto };
        // The regression that shipped unstamped tracks: the probe said "Japanese", the download
        // embedded "Japanese (Original)" — different invocations, different names. With every
        // pick machine-made, every stream is stamped anyway, keeping its own title.
        let all_auto = [pick("Japanese", true), pick("English", true)];
        assert_eq!(
            auto_title_stamps("Japanese (Original)\nEnglish\n", &all_auto),
            [
                // `stamped_title` drops YouTube's " (Original)" qualifier on purpose — the
                // auto-generated marker makes it redundant noise.
                (0, "Japanese (auto-generated)".to_string()),
                (1, "English (auto-generated)".to_string())
            ]
        );
        // Mixed manual+auto: only a name-matched auto track can be told apart — the manual one
        // must never be mislabelled, even at the cost of missing a renamed auto track.
        let mixed = [pick("English", false), pick("Japanese", true)];
        assert_eq!(
            auto_title_stamps("English\nJapanese\n", &mixed),
            [(1, "Japanese (auto-generated)".to_string())]
        );
        assert_eq!(
            auto_title_stamps("English\nJapanese (Original)\n", &mixed),
            [],
            "an unrecognizable auto name in a mixed set stays untouched — no safe discrimination"
        );
        // Idempotence: a already-stamped title is never double-marked.
        assert_eq!(auto_title_stamps("Japanese (auto-generated)\n", &all_auto[..1]), []);
        // An untitled stream still gets a readable mark.
        assert_eq!(
            auto_title_stamps("\n", &all_auto[..1]),
            [(0, "subtitles (auto-generated)".to_string())]
        );
    }

    #[test]
    fn auto_tracks_get_stamped_titles() {
        assert_eq!(stamped_title("Hebrew (Original)"), "Hebrew (auto-generated)");
        assert_eq!(stamped_title("English"), "English (auto-generated)");
        assert_eq!(stamped_title("English from Korean"), "English from Korean (auto-generated)");
    }

    #[test]
    fn the_attached_pic_index_is_read_from_ffprobes_stream_listing() {
        // `index=N` lines each followed by that stream's dispositions (default=nw=1 layout).
        let with_cover = "index=0\nDISPOSITION:attached_pic=0\nindex=1\nDISPOSITION:attached_pic=0\nindex=2\nDISPOSITION:attached_pic=1";
        assert_eq!(attached_pic_stream_index(with_cover), Some(2), "the cover's absolute index");
        let without = "index=0\nDISPOSITION:attached_pic=0\nindex=1\nDISPOSITION:attached_pic=0";
        assert_eq!(attached_pic_stream_index(without), None, "video+audio, no cover");
        assert_eq!(attached_pic_stream_index(""), None, "no streams");
    }

    #[test]
    fn find_by_id_walks_nested_dirs_and_matches_media_extensions_only() {
        let dir = scratch_dir("findbyid");
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/20200101__Title__abcdefghijk.mkv"), b"x").unwrap();
        std::fs::write(dir.join("notes__abcdefghijk.txt"), b"x").unwrap(); // not a media ext
        std::fs::write(dir.join("song__qrstuvwxyz1.opus"), b"x").unwrap();
        let found = find_by_id(&dir, "abcdefghijk").expect("nested mkv found");
        assert!(found.ends_with("a/b/20200101__Title__abcdefghijk.mkv"), "{found:?}");
        assert!(find_by_id(&dir, "qrstuvwxyz1").is_some(), "audio extensions match too");
        assert!(find_by_id(&dir, "zzzzzzzzzzz").is_none(), "an unknown id finds nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subtitle_titles_reads_ffprobes_titled_streams_for_the_idempotency_match() {
        let listing = "index=2\nTAG:title=English\nindex=3\nTAG:title=Hebrew (auto-generated)\nindex=4";
        assert_eq!(subtitle_titles(listing), ["English", "Hebrew (auto-generated)"]);
        assert!(subtitle_titles("index=2\nindex=3").is_empty(), "streams with no title tag → none");
        // The title a pick is matched/written by: plain name for a real track, stamped for auto.
        let real = Pick { key: "en".into(), name: "English".into(), auto: false };
        let auto = Pick { key: "iw-orig".into(), name: "Hebrew (Original)".into(), auto: true };
        assert_eq!(subtitle_title(&real), "English");
        assert_eq!(subtitle_title(&auto), "Hebrew (auto-generated)");
    }

    // --- round-trip checks against real media (skip-with-notice when ffmpeg is absent) ------

    /// A tiny real mkv (blue frame + a beep) at `path` — the fixture every round-trip starts from.
    fn build_test_mkv(path: &Path) {
        let ok = std::process::Command::new(ff_bin("ffmpeg")).stdin(std::process::Stdio::null())
            .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", "color=c=blue:s=64x64:d=1"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=1"])
            .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac"])
            .arg(path)
            .status()
            .is_ok_and(|status| status.success());
        assert!(ok, "could not build the test mkv");
    }

    #[test]
    fn a_thumbnail_attaches_probes_and_extracts_back_out() {
        if !ffmpeg_or_skip("thumbnail round-trip") {
            return;
        }
        let dir = scratch_dir("thumbtrip");
        let file = dir.join("v__abcdefghijk.mkv");
        build_test_mkv(&file);
        let cover = dir.join("cover.jpg");
        let ok = std::process::Command::new(ff_bin("ffmpeg")).stdin(std::process::Stdio::null())
            .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", "color=c=red:s=32x32:d=1", "-frames:v", "1"])
            .arg(&cover)
            .status()
            .is_ok_and(|status| status.success());
        assert!(ok, "could not build the cover");

        assert_eq!(has_embedded_thumbnail(&file), Some(false), "starts bare");
        assert!(attach_thumbnail(&file, &cover), "attach succeeds");
        assert_eq!(has_embedded_thumbnail(&file), Some(true), "probe sees the cover");
        let index = embedded_thumbnail_index(&file).flatten().expect("cover index");
        let out = extract_thumbnail(&file, index).expect("extracts back out");
        assert!(std::fs::metadata(&out).unwrap().len() > 0, "extracted cover has bytes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_subtitle_mux_adds_the_track_and_carries_the_cover_through() {
        // Unit-pins the remux bug the live tests caught: an attached cover must survive
        // mux_subtitles (it can't ride `-map 0` — the pass extracts and re-attaches it).
        if !ffmpeg_or_skip("subtitle mux round-trip") {
            return;
        }
        let dir = scratch_dir("muxtrip");
        let file = dir.join("v__abcdefghijk.mkv");
        build_test_mkv(&file);
        let cover = dir.join("cover.jpg");
        let _ = std::process::Command::new(ff_bin("ffmpeg")).stdin(std::process::Stdio::null())
            .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", "color=c=red:s=32x32:d=1", "-frames:v", "1"])
            .arg(&cover)
            .status();
        assert!(attach_thumbnail(&file, &cover));

        let sidecar = dir.join("v.en.vtt");
        std::fs::write(&sidecar, "WEBVTT\n\n00:00.000 --> 00:01.000\nhi\n").unwrap();
        let pick = Pick { key: "en".into(), name: "English".into(), auto: false };
        assert!(mux_subtitles(&file, &[(&pick, sidecar)], 0), "mux succeeds");

        let (count, titles) = subtitle_streams(&file).expect("probe works");
        assert_eq!((count, titles), (1, vec!["English".to_string()]), "the track landed, titled");
        assert_eq!(has_embedded_thumbnail(&file), Some(true), "the cover survived the remux");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auto_subtitle_tracks_get_their_titles_stamped_in_place() {
        if !ffmpeg_or_skip("auto-title stamping") {
            return;
        }
        let dir = scratch_dir("stamptrip");
        let file = dir.join("v__abcdefghijk.mkv");
        build_test_mkv(&file);
        // Embed a track carrying YouTube's own name for it, as a download would.
        let sidecar = dir.join("v.en.vtt");
        std::fs::write(&sidecar, "WEBVTT\n\n00:00.000 --> 00:01.000\nhi\n").unwrap();
        let auto = Pick { key: "en".into(), name: "English".into(), auto: true };
        // mux_subtitles writes the stamped title for auto picks; build the pre-stamp state
        // (plain "English") with a non-auto pick, then stamp via mark_auto_titles.
        let plain = Pick { key: "en".into(), name: "English".into(), auto: false };
        assert!(mux_subtitles(&file, &[(&plain, sidecar)], 0));

        mark_auto_titles(&dir, "abcdefghijk", std::slice::from_ref(&auto), Env::default());
        let (_, titles) = subtitle_streams(&file).expect("probe works");
        assert_eq!(titles, ["English (auto-generated)"], "the machine origin is stamped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sidecars_pair_by_lang_key_and_skip_refused_tracks() {
        let dir = std::env::temp_dir().join(format!("vidl_sidecar_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("20260101__title__abcdefghijk.opus");
        std::fs::write(&file, "").unwrap();
        std::fs::write(dir.join("20260101__title__abcdefghijk.iw-orig.vtt"), "WEBVTT").unwrap();
        let picks = [
            Pick { key: "iw-orig".into(), name: String::new(), auto: true },
            Pick { key: "en".into(), name: String::new(), auto: true }, // 429'd: never arrived
        ];
        let pairs = subtitle_sidecars(&file, &picks);
        assert_eq!(pairs.len(), 1, "missing sidecars are skipped, not errors");
        assert_eq!(pairs[0].0, "subtitles_iw_autogenerated");
        assert!(pairs[0].1.ends_with("20260101__title__abcdefghijk.iw-orig.vtt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subtitle_tags_round_trip_into_each_audio_container() {
        // The lofty-backed tagger, end to end per family: Vorbis comments (opus), MP4 atoms
        // (m4a), ID3 frames (mp3). Read back through ffprobe — a second, independent tool — so
        // the test isn't lofty checking lofty. The sidecar must be folded in and removed.
        if !ffmpeg_or_skip("audio tag round-trip") {
            return;
        }
        let dir = scratch_dir("audiotags");
        for (ext, codec) in [("opus", "libopus"), ("m4a", "aac"), ("mp3", "libmp3lame")] {
            let file = dir.join(format!("song__abcdefghijk.{ext}"));
            let built = std::process::Command::new(ff_bin("ffmpeg")).stdin(std::process::Stdio::null())
                .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=1"])
                .args(["-c:a", codec])
                .arg(&file)
                .status()
                .is_ok_and(|status| status.success());
            assert!(built, "could not build the {ext} sample");
            let sidecar = dir.join("song__abcdefghijk.en.vtt");
            std::fs::write(&sidecar, "WEBVTT\n\n00:00.000 --> 00:01.000\nthe words\n").unwrap();
            let picks = vec![Pick { key: "en".into(), name: "English".into(), auto: false }];

            embed_subtitle_tags(&dir, "abcdefghijk", &picks);

            assert!(!sidecar.exists(), "{ext}: the sidecar is folded in and removed");
            // ffprobe models ogg-family tags on the stream and mp3/m4a tags on the format —
            // ask for both levels so every container's convention is covered.
            let tags = std::process::Command::new(ff_bin("ffprobe")).stdin(std::process::Stdio::null())
                .args(["-v", "error", "-show_entries", "format_tags:stream_tags", "-of", "default=nw=1"])
                .arg(&file)
                .output()
                .expect("run ffprobe");
            let tags = String::from_utf8_lossy(&tags.stdout).to_lowercase();
            assert!(
                tags.contains("subtitles_en") && tags.contains("webvtt"),
                "{ext}: the tag and its text must read back: {tags}"
            );
            std::fs::remove_file(&file).unwrap(); // one file per id at a time (find_by_id)
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subtitle_tags_name_the_language_and_the_machine_origin() {
        let pick = |key: &str, auto: bool| Pick { key: key.into(), name: String::new(), auto };
        assert_eq!(subtitle_tag_name(&pick("en", false)), "subtitles_en");
        assert_eq!(subtitle_tag_name(&pick("en-US", false)), "subtitles_en_us");
        assert_eq!(subtitle_tag_name(&pick("iw-orig", true)), "subtitles_iw_autogenerated");
        assert_eq!(subtitle_tag_name(&pick("en-he", true)), "subtitles_en_he_autogenerated");
    }
    #[test]
    fn the_thumbnail_pass_leaves_non_mkv_files_untouched() {
        // Regression: the cover attaches via an mkv remux — renaming that over an audio file
        // would swap its container. The pass must skip non-mkv files without touching them
        // (guard fires before any ffprobe/fetch, so this runs fully offline).
        let dir = scratch_dir("thumbguard");
        let file = dir.join("song__abcdefghijk.opus");
        std::fs::write(&file, b"OPUSDATA").unwrap();
        embed_thumbnails(&dir, &["abcdefghijk".to_string()]);
        assert_eq!(std::fs::read(&file).unwrap(), b"OPUSDATA", "audio bytes must be untouched");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name() != "song__abcdefghijk.opus")
            .collect();
        assert!(leftovers.is_empty(), "no temp or mkv artifacts may appear: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
