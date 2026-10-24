//! Everything that touches the files on disk once yt-dlp has written them: cover art
//! (thumbnails), embedded subtitle tracks, and audio subtitle tags — idempotent ffmpeg/lofty
//! passes, safe to re-run across a whole collection.
//!
//! [`subtitles`] is the planner for that last part: which caption tracks a given video *should*
//! have, chosen from YouTube's ~157-language matrix, before anything is fetched or muxed.

pub(crate) mod facts;
pub(crate) mod repair;
pub(crate) mod subtitles;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::util::exec::{capture_output, capture_stdout};
use crate::Env;
use crate::ytdlp::{seeded, ytdlp_invocation};
use crate::ytdlp::scan::{ScanEntry};
use self::subtitles::{Pick, batch_probe};
use crate::util::size;
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
    let Some(file) = find_by_id(root, id, Prefer::Video) else { return };
    let Some(listing) = subtitle_title_listing(&file) else {
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
    // Checked here, not beside the lookup above, so the count in the notice is true: by this point
    // the file really does hold tracks that would have been renamed.
    //
    // Without this the pass ran on anything the lookup found, and the remux below is hardcoded to
    // write Matroska (`stamping.mkv`) before renaming its result back over the original — so a
    // `.webm` or `.mp4` either failed outright, when its subtitle codec would not `-c copy` into
    // Matroska, or survived as a file whose contents no longer matched its extension.
    if !is_mkv(&file) {
        eprintln!(
            "{}",
            style::problematic(&format!(
                "vidl: subtitle track names embed into mkv only — {} auto-generated name(s) in {} \
                 left as YouTube wrote them",
                retitle.len(),
                file.display()
            ))
        );
        return;
    }
    if apply_title_stamps(&file, &retitle) {
        for (_, title) in &retitle {
            println!("stamped subtitle track: {title}");
        }
    } else {
        eprintln!("vidl: could not stamp auto-subtitle titles in {}", file.display());
    }
}

/// `file`'s subtitle titles, one line per subtitle stream in stream order — an untitled one an
/// empty line — as [`auto_title_stamps`] reads them; `None` if ffprobe couldn't run.
fn subtitle_title_listing(file: &Path) -> Option<String> {
    let ffprobe = crate::tools::ffprobe();
    capture_stdout(
        &ffprobe,
        [
            OsString::from("-v"), "error".into(),
            "-select_streams".into(), "s".into(),
            "-show_entries".into(), "stream_tags=title".into(),
            "-of".into(), "csv=p=0".into(),
            file.as_os_str().to_owned(),
        ],
    )
}

/// Retitle the mkv `file`'s subtitle tracks as `stamps` says (subtitle-stream order, new title), in
/// one local stream-copy remux renamed over it. Returns success; on failure the file is as it was.
fn apply_title_stamps(file: &Path, stamps: &[(usize, String)]) -> bool {
    let ffmpeg = crate::tools::ffmpeg();
    let stamped = file.with_extension("stamping.mkv");
    let mut argv: Vec<OsString> = ["-nostdin", "-v", "error", "-y", "-i"].map(OsString::from).to_vec();
    argv.push(file.as_os_str().to_owned());
    // A bare `-map 0 -c copy` remux DEMOTES an attached cover, so the covers are carried through
    // ([`CoverCarry`]). This used to rest on call order alone — straight after a fresh download,
    // before any thumbnail pass — which a file already on disk but missing from the archive
    // breaks: yt-dlp calls it "already downloaded", and the finishing pass stamps it, cover and all.
    let carry = embedded(file).map(|embedded| CoverCarry::take(file, &embedded)).unwrap_or_default();
    argv.extend(["-map", "0", "-c", "copy"].map(OsString::from));
    argv.extend(carry.args());
    for (order, title) in stamps {
        argv.push(format!("-metadata:s:s:{order}").into());
        argv.push(format!("title={title}").into());
    }
    argv.push(stamped.as_os_str().to_owned());
    let renamed = crate::util::exec::run_ok(&ffmpeg, &argv) && std::fs::rename(&stamped, file).is_ok();
    carry.clean();
    if !renamed {
        let _ = std::fs::remove_file(&stamped);
    }
    renamed
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
    let Some(file) = find_by_id(root, id, Prefer::Audio) else { return };
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

/// A cover a file embeds: its absolute stream index, its image codec (`mjpeg`, `png`), and how it
/// sits there — attached, as a Matroska cover should be (ffprobe's `attached_pic`), or demoted by
/// an earlier remux to a plain video track: still the picture, still tagged with the image type
/// it was attached as, but no longer marked a cover, so thumbnailers pass it over and players
/// list it as one more video.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EmbeddedCover {
    index: u32,
    codec: String,
    demoted: bool,
}

/// What a file embeds that a remux has to mind: its covers, and how many attachments `-map 0`
/// carries on its own (a font, an info json) — a cover attached again lands after those.
#[derive(Debug, Default, PartialEq, Eq)]
struct Embedded {
    covers: Vec<EmbeddedCover>,
    attachments: usize,
}

/// [`Embedded`] as read off an ffprobe `stream=index,codec_name,codec_type:stream_disposition=
/// attached_pic:stream_tags=mimetype -of default=nw=1` listing (`index=N` opening each stream).
/// A demoted cover is a video stream with an `image/…` mimetype — only an attachment is given
/// one — and its tag arrives uppercase (`TAG:MIMETYPE=`), as Matroska stores a track's tags.
/// Pure, so the idempotency gate and the remux cover-carry are unit-tested without ffprobe.
fn embedded_in(listing: &str) -> Embedded {
    #[derive(Default)]
    struct Seen {
        index: Option<u32>,
        codec: String,
        kind: String,
        attached: bool,
        image: bool,
    }
    let mut embedded = Embedded::default();
    let mut tally = |seen: &Seen| match (seen.index, seen.kind.as_str()) {
        (Some(index), "video") if seen.attached || seen.image => embedded.covers.push(EmbeddedCover {
            index,
            codec: seen.codec.clone(),
            demoted: !seen.attached,
        }),
        (Some(_), "attachment") => embedded.attachments += 1,
        _ => {}
    };
    let mut seen = Seen::default();
    for line in listing.lines().map(str::trim) {
        if let Some(index) = line.strip_prefix("index=") {
            tally(&seen);
            seen = Seen { index: index.parse().ok(), ..Seen::default() };
        } else if let Some(codec) = line.strip_prefix("codec_name=") {
            seen.codec = codec.to_string();
        } else if let Some(kind) = line.strip_prefix("codec_type=") {
            seen.kind = kind.to_string();
        } else if line == "DISPOSITION:attached_pic=1" {
            seen.attached = true;
        } else if let Some((key, value)) = line.strip_prefix("TAG:").and_then(|tag| tag.split_once('=')) {
            seen.image |= key.eq_ignore_ascii_case("mimetype") && value.starts_with("image/");
        }
    }
    tally(&seen);
    embedded
}

/// What `file` embeds ([`Embedded`]); `None` when ffprobe couldn't run — the caller then leaves the
/// file untouched rather than guess.
fn embedded(file: &Path) -> Option<Embedded> {
    let ffprobe = crate::tools::ffprobe();
    let listing = capture_stdout(
        &ffprobe,
        [
            OsString::from("-v"), "error".into(),
            "-show_entries".into(),
            "stream=index,codec_name,codec_type:stream_disposition=attached_pic:stream_tags=mimetype".into(),
            "-of".into(), "default=nw=1".into(),
            file.as_os_str().to_owned(),
        ],
    )?;
    Some(embedded_in(&listing))
}

/// The extension and mimetype a cover in `codec` is extracted and attached as — `None` for an image
/// type Matroska keeps no cover in. vidl's own are always JPEG: both `dl`'s pass and yt-dlp's legacy
/// inline embeds convert to jpg; PNG is here for covers from elsewhere.
fn cover_type(codec: &str) -> Option<(&'static str, &'static str)> {
    match codec {
        "mjpeg" => Some(("jpg", "image/jpeg")),
        "png" => Some(("png", "image/png")),
        _ => None,
    }
}

/// Pull `cover` out of `file` — a one-packet stream copy into a temp file beside it — so a remux can
/// re-`-attach` it. `None` (temp cleaned) on failure.
fn extract_cover(file: &Path, cover: &EmbeddedCover) -> Option<PathBuf> {
    let (ext, _) = cover_type(&cover.codec)?;
    let ffmpeg = crate::tools::ffmpeg();
    let tmp = file.with_extension(format!("cover-keep-{}.{ext}", cover.index));
    let mut argv: Vec<OsString> = ["-nostdin", "-v", "error", "-y", "-i"].map(OsString::from).to_vec();
    argv.push(file.as_os_str().to_owned());
    argv.push("-map".into());
    argv.push(format!("0:{}", cover.index).into());
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

/// The covers a remux of a file carries through. A cover can't just ride `-map 0` — the remux
/// demotes it to a plain video track (the `attached_pic` disposition doesn't survive an mkv
/// round-trip) — so each is extracted first, left out of the map, and re-`-attach`ed in the same
/// pass; a demoted one is restored with them. A picture embedded twice (the old, demoted copy and
/// a fresh one beside it) is carried once; a different second picture is kept, named so the first
/// stays the one thumbnailers show. A cover that won't come out stays mapped — demoted, as before
/// this carry existed, but kept.
#[derive(Debug, Default)]
struct CoverCarry {
    /// Cover streams left out of the map: each extracted, or a copy of one that was.
    excluded: Vec<u32>,
    /// The extracted pictures, each with the file name and mimetype it is attached under.
    attached: Vec<(PathBuf, String, &'static str)>,
    /// The attachments `-map 0` carries itself, numbered ahead of the re-attached covers.
    mapped_attachments: usize,
}

impl CoverCarry {
    /// Extracts the covers `embedded` lists out of `file`, attached ones first.
    fn take(file: &Path, embedded: &Embedded) -> CoverCarry {
        let mut carry = CoverCarry { mapped_attachments: embedded.attachments, ..CoverCarry::default() };
        let mut covers: Vec<&EmbeddedCover> = embedded.covers.iter().collect();
        covers.sort_by_key(|cover| cover.demoted);
        let mut taken: Vec<Vec<u8>> = Vec::new();
        for cover in covers {
            let Some(tmp) = extract_cover(file, cover) else { continue };
            let bytes = std::fs::read(&tmp).unwrap_or_default();
            carry.excluded.push(cover.index);
            if taken.contains(&bytes) {
                let _ = std::fs::remove_file(&tmp);
                continue;
            }
            let (ext, mimetype) = cover_type(&cover.codec).expect("extracted, so a known type");
            let name = match carry.attached.len() {
                0 => format!("cover.{ext}"),
                n => format!("cover-{}.{ext}", n + 1),
            };
            taken.push(bytes);
            carry.attached.push((tmp, name, mimetype));
        }
        carry
    }

    /// The options carrying the covers, for after a remux's `-map 0`: their streams left out of it,
    /// and each attached again — named and typed, as attachment number `mapped_attachments + n`.
    fn args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        for index in &self.excluded {
            args.push("-map".into());
            args.push(format!("-0:{index}").into());
        }
        for (number, (tmp, name, mimetype)) in self.attached.iter().enumerate() {
            let tag = format!("-metadata:s:t:{}", self.mapped_attachments + number);
            args.push("-attach".into());
            args.push(tmp.as_os_str().to_owned());
            args.extend([tag.clone(), format!("mimetype={mimetype}"), tag, format!("filename={name}")].map(OsString::from));
        }
        args
    }

    /// Remove the extracted pictures — they were only for the re-attach.
    fn clean(self) {
        for (tmp, ..) in self.attached {
            let _ = std::fs::remove_file(tmp);
        }
    }
}

/// Restore `file`'s demoted covers in place: one stream-copy remux carrying every cover through
/// ([`CoverCarry`]) and changing nothing else. Renames over the original. Returns success.
fn repair_covers(file: &Path, embedded: &Embedded) -> bool {
    let ffmpeg = crate::tools::ffmpeg();
    let carry = CoverCarry::take(file, embedded);
    let out = file.with_extension("covering.mkv");
    let mut argv: Vec<OsString> = ["-nostdin", "-v", "error", "-y", "-i"].map(OsString::from).to_vec();
    argv.push(file.as_os_str().to_owned());
    argv.extend(["-map", "0", "-c", "copy"].map(OsString::from));
    argv.extend(carry.args());
    argv.push(out.as_os_str().to_owned());
    let ok = !carry.excluded.is_empty() && crate::util::exec::run_ok(&ffmpeg, &argv) && std::fs::rename(&out, file).is_ok();
    carry.clean();
    if !ok {
        let _ = std::fs::remove_file(&out);
    }
    ok
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
/// it), keeping every existing stream (`-map 0 -c copy`) — for a file with no cover yet, so there is
/// none to carry. The new attachment is numbered after those the file already has, which `-map 0`
/// puts first. Writes a sibling temp file and renames over the original. Returns whether it
/// succeeded.
fn attach_thumbnail(file: &Path, thumb: &Path) -> bool {
    let ffmpeg = crate::tools::ffmpeg();
    let out = file.with_extension("thumbing.mkv");
    let tag = format!("-metadata:s:t:{}", embedded(file).map_or(0, |embedded| embedded.attachments));
    let mut argv: Vec<OsString> = ["-nostdin", "-v", "error", "-y", "-i"].map(OsString::from).to_vec();
    argv.push(file.as_os_str().to_owned());
    argv.extend(["-map", "0", "-c", "copy", "-attach"].map(OsString::from));
    argv.push(thumb.as_os_str().to_owned());
    argv.extend([tag.clone(), "mimetype=image/jpeg".to_string(), tag, "filename=cover.jpg".to_string()].map(OsString::from));
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

/// The late cover-art pass, which a run makes unless `--no-thumbnail`: for each video `id` under
/// `dir`, scan whether it already has an embedded thumbnail — naming each one that's missing up
/// front, so the user sees what's coming rather than waiting on unbounded work — then fetch +
/// embed only those. A video it leaves alone is counted, not named ([`Tally`]): every run makes
/// this pass, and a re-run over a whole channel would otherwise print a line per video it did
/// nothing to.
/// Deliberately independent of the download archive, so a re-run *patches* previously-downloaded
/// videos; the embedded-thumbnail check keeps it idempotent (a video that already has one is never
/// re-fetched or re-embedded). A thumbnail an earlier remux demoted to a plain video track is no
/// missing one: it is restored in place ([`repair_covers`]) — no fetch, and no second cover beside
/// it.
pub(crate) fn embed_thumbnails(dir: &Path, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    println!("thumbnails: scanning {} video(s)…", ids.len());
    let mut tally = Tally::default();
    let mut missing: Vec<(String, PathBuf)> = Vec::new();
    for id in ids {
        match find_by_id(dir, id, Prefer::Video) {
            None => {
                tally.skipped += 1;
                println!("  [{id}]: {}", style::problematic("no file found — skipping"));
            }
            // A cover embeds as an mkv attachment ([`attach_thumbnail`] muxes into the mkv
            // container) — renaming that over an audio/webm file would swap its container out
            // from under it, corrupting it. Skip anything that isn't an mkv, and say so.
            Some(file) if !is_mkv(&file) => {
                tally.skipped += 1;
                println!("  [{id}]: {}", style::problematic("not an mkv — cover art embeds into video (mkv) only; skipping"));
            }
            Some(file) => match embedded(&file) {
                Some(embedded) if embedded.covers.iter().any(|cover| cover.demoted) => {
                    if repair_covers(&file, &embedded) {
                        tally.restored += 1;
                        println!("  [{id}]: {}", style::approved("thumbnail restored (a remux had made a plain video track of it)"));
                    } else {
                        tally.failed += 1;
                        eprintln!("  [{id}]: {}", style::problematic("could not restore its thumbnail"));
                    }
                }
                Some(embedded) if !embedded.covers.is_empty() => tally.present += 1,
                Some(_) => {
                    println!("  [{id}]: {}", style::problematic("missing a thumbnail"));
                    missing.push((id.clone(), file));
                }
                None => {
                    tally.skipped += 1;
                    println!("  [{id}]: {}", style::problematic("could not read (ffprobe) — skipping"));
                }
            },
        }
    }
    if !missing.is_empty() {
        println!("thumbnails: fetching + embedding {} …", missing.len());
    }
    for (id, file) in &missing {
        if embed_one_thumbnail(file, id) {
            tally.embedded += 1;
            println!("  [{id}]: {}", style::approved("embedded"));
        } else {
            tally.failed += 1;
            eprintln!("  [{id}]: {}", style::problematic("could not embed a thumbnail"));
        }
    }
    println!("thumbnails: {}", tally.summary());
}

/// What one thumbnail pass found and did, for its closing line — the only mention a video it left
/// alone gets.
#[derive(Default)]
struct Tally {
    /// Already had a cover.
    present: usize,
    /// Had a cover a remux demoted, now put back.
    restored: usize,
    /// Fetched and embedded.
    embedded: usize,
    /// Could not be restored or embedded.
    failed: usize,
    /// No file, not an mkv, or unreadable.
    skipped: usize,
}

impl Tally {
    /// `298 present, 1 restored, 1 embedded, 1 failed, 1 skipped` — every count that isn't zero.
    fn summary(&self) -> String {
        // Destructured, not read field by field, so a new count cannot be left out of the line.
        let Tally { present, restored, embedded, failed, skipped } = *self;
        let counts = [
            (present, "present"),
            (restored, "restored"),
            (embedded, "embedded"),
            (failed, "failed"),
            (skipped, "skipped"),
        ];
        let said: Vec<String> = counts
            .iter()
            .filter(|(count, _)| *count > 0)
            .map(|(count, what)| format!("{count} {what}"))
            .collect();
        said.join(", ")
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
/// count, so the new tracks land at the right output indices. Its covers are carried through
/// ([`CoverCarry`]): a remux would otherwise demote them. Renames over the original. Returns
/// success.
fn mux_subtitles(file: &Path, arrived: &[(&Pick, PathBuf)], existing: usize) -> bool {
    let ffmpeg = crate::tools::ffmpeg();
    let carry = embedded(file).map(|embedded| CoverCarry::take(file, &embedded)).unwrap_or_default();
    let out = file.with_extension("subbing.mkv");
    let mut argv: Vec<OsString> = ["-nostdin", "-v", "error", "-y", "-i"].map(OsString::from).to_vec();
    argv.push(file.as_os_str().to_owned());
    for (_, sidecar) in arrived {
        argv.push("-i".into());
        argv.push(sidecar.as_os_str().to_owned());
    }
    argv.extend(["-map", "0"].map(OsString::from));
    for input in 1..=arrived.len() {
        argv.push("-map".into());
        argv.push(input.to_string().into());
    }
    argv.extend(["-c", "copy", "-c:s", "srt"].map(OsString::from));
    argv.extend(carry.args());
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
    carry.clean();
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
    let Some(file) = find_by_id(into, id, Prefer::Video) else {
        println!("  [{id}]: {}", style::problematic("no file found — skipping"));
        return;
    };
    if !is_mkv(&file) {
        // Two different situations, and calling both "audio" told half the users a video had
        // nothing to patch when in fact it could not be patched at all.
        let why = if is_audio(&file) {
            style::approved("audio — subtitles kept as tags; nothing to patch")
        } else {
            style::problematic("not an mkv — subtitle tracks embed into video (mkv) only; skipping")
        };
        println!("  [{id}]: {why}");
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
        embed_subtitles(home, &plan.id, &facts::watch_url(&plan.id), &plan.picks, env);
    }
}

/// The video containers a download can land in.
///
/// `.mkv` is the default merge target and `.webm` a documented override. `.mp4` earns its place
/// off YouTube: a generic single-format download very often ends in it, and while it was missing
/// from here every pass that locates a file by its id quietly skipped those.
///
/// Kept apart from [`AUDIO_EXTS`] because the difference is load-bearing — a pass that cannot use
/// a container has to say whether that is because the file is audio (nothing was lost) or because
/// it is a video this crate cannot mux into (something was).
const VIDEO_EXTS: [&str; 3] = [".mp4", ".mkv", ".webm"];

/// What an `--audio` run leaves instead of a video.
const AUDIO_EXTS: [&str; 6] = [".opus", ".m4a", ".mp3", ".ogg", ".flac", ".wav"];

/// Whether `name` ends in an extension one of this crate's downloads produces.
fn is_media_name(name: &str) -> bool {
    VIDEO_EXTS.iter().chain(&AUDIO_EXTS).any(|ext| name.ends_with(ext))
}

/// Whether `file` is an audio extraction rather than a video.
fn is_audio(file: &Path) -> bool {
    let name = file.file_name().unwrap_or_default().to_string_lossy();
    AUDIO_EXTS.iter().any(|ext| name.ends_with(ext))
}

/// Whether `file` is the one container the passes here can mux into.
///
/// mkv takes attachments (cover art) and subtitle streams of any codec; the alternatives a
/// download can land in take neither, or not in a way `-c copy` can carry. All three muxing passes
/// ask this and all three *decline* rather than convert: re-encoding a finished download to add a
/// track name or a cover would cost far more than either is worth.
fn is_mkv(file: &Path) -> bool {
    file.extension().and_then(|ext| ext.to_str()) == Some("mkv")
}

/// Delete the `.part` files no run can ever finish, and say which ones went.
///
/// # Exactly what gets deleted
///
/// A file is removed only if **all three** hold:
///
/// 1. its name ends in `.part` — yt-dlp's in-progress suffix;
/// 2. stripped of that suffix, it ends in an extension a download writes — so it is something we
///    wrote, not another program's unrelated partial (a browser downloading `report.pdf` leaves
///    `report.pdf.part`, and that must survive);
/// 3. its name contains no `__`.
///
/// Nothing else in the tree is touched: no finished videos, no subtitle sidecars, no thumbnails,
/// and no `.part` file that could still be resumed.
///
/// # Why the `__` rule makes it safe
///
/// Every `--output` template this crate hands yt-dlp puts `__` in the name — the id marker on
/// YouTube, the position prefix in a collection, the id/format separator off YouTube. A part file
/// without one therefore cannot belong to any download *this* version is able to resume: it was
/// written before part files were keyed to their rendition, and the name yt-dlp would now look for
/// is a different name, so the bytes are unreachable and will sit there forever.
///
/// That invariant is not assumed. `names::tests` asserts every template contains `__`, so if a
/// future template ever drops it, a test fails rather than this sweep quietly eating live data.
///
/// One consequence is worth stating outright: a part file being written *right now*, by a
/// concurrently running `vidl`, is never a candidate — its name contains `__` like every other
/// current one. This cannot interrupt a download in progress.
///
/// # Why it also had to be a sweep
///
/// Cleaning up only what the current video needs would leave the rest untouched forever, because
/// an interrupted collection leaves a leftover per entry and the entries that already succeeded
/// are skipped on the next run. So it walks the whole destination once per run — the leftovers sit
/// wherever the interrupted download was writing, which for a collection is a folder the next run
/// may never enter.
pub(crate) fn discard_stale_parts(root: &Path) {
    let mut stack = vec![root.to_path_buf()];
    let mut removed: Vec<(String, u64)> = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if !is_stale_part(&path) {
                continue;
            }
            let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            match std::fs::remove_file(&path) {
                // Reported by full path: these sit in nested collection folders, and a bare file
                // name would not say which.
                Ok(()) => removed.push((path.display().to_string(), size)),
                Err(why) => eprintln!(
                    "{}",
                    style::problematic(&format!(
                        "vidl: could not remove the unresumable {} ({why})",
                        path.display()
                    ))
                ),
            }
        }
    }
    if removed.is_empty() {
        return;
    }
    let freed: u64 = removed.iter().map(|(_, size)| size).sum();
    println!(
        "removed {} unfinishable partial download(s), freeing {} — left by a version whose \
         partial files could not be told apart by rendition:",
        removed.len(),
        size::roughly(freed)
    );
    for (path, size) in removed {
        println!("  {path} ({})", size::roughly(size));
    }
}

/// Whether `path` is a partial download this crate can no longer finish — the three conditions
/// listed on [`discard_stale_parts`], which is the only caller and where the reasoning lives.
fn is_stale_part(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else { return false };
    let Some(stem) = name.strip_suffix(".part") else { return false };
    !name.contains("__") && is_media_name(stem)
}

/// Which of a video's downloads a pass works on, when [`find_by_id`] finds more than one.
#[derive(Clone, Copy)]
pub(crate) enum Prefer {
    /// The video — the mkv the passes mux into before any other container.
    Video,
    /// The audio extraction, which subtitle tags go into.
    Audio,
}

impl Prefer {
    /// How well `file` suits: 0 is the download wanted, and a higher rank is taken only when
    /// nothing better is there — so a pass finding just the other kind still has it to report
    /// on, as it did when every folder held one download per video.
    fn rank(self, file: &Path) -> u8 {
        match self {
            Prefer::Video if is_mkv(file) => 0,
            Prefer::Video if is_audio(file) => 2,
            Prefer::Video => 1,
            Prefer::Audio if is_audio(file) => 0,
            Prefer::Audio => 1,
        }
    }
}

/// The downloaded file whose name carries `__<id>.` — the id rides in every output template
/// precisely so files stay findable. A shallow recursive walk under `root`.
///
/// A folder can hold more than one download of a video — an audio extraction beside the video,
/// or a copy in a collection folder further down — so each is ranked for the pass asking
/// ([`Prefer`]), and the walk stops at the first one wanted most. Taking the first one met left
/// it to the order the filesystem lists them in: the thumbnail pass could meet the audio, skip it
/// as no mkv, and never reach the video beside it. A folder that cannot be read hides only what
/// is in it.
pub(crate) fn find_by_id(root: &Path, id: &str, prefer: Prefer) -> Option<PathBuf> {
    let marker = format!("__{id}.");
    let mut best: Option<(u8, PathBuf)> = None;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.contains(&marker) && is_media_name(&name)
            }) {
                let rank = prefer.rank(&path);
                if rank == 0 {
                    return Some(path);
                }
                if best.as_ref().is_none_or(|(held, _)| rank < *held) {
                    best = Some((rank, path));
                }
            }
        }
    }
    best.map(|(_, path)| path)
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
    fn covers_are_read_from_ffprobes_stream_listing_attached_or_demoted() {
        // `index=N` opening each stream's lines (default=nw=1 layout): a cover is attached by its
        // disposition, or demoted but still typed as an image — the tag then a track's, uppercase.
        let video = "index=0\ncodec_name=h264\ncodec_type=video\nDISPOSITION:attached_pic=0\n";
        let audio = "index=1\ncodec_name=aac\ncodec_type=audio\nDISPOSITION:attached_pic=0\n";
        let attached = "index=2\ncodec_name=mjpeg\ncodec_type=video\nDISPOSITION:attached_pic=1\nTAG:mimetype=image/jpeg\n";
        let demoted = "index=3\ncodec_name=mjpeg\ncodec_type=video\nDISPOSITION:attached_pic=0\nTAG:MIMETYPE=image/jpeg\n";
        let font = "index=4\ncodec_type=attachment\nDISPOSITION:attached_pic=0\nTAG:mimetype=application/x-truetype-font\n";
        let listing = [video, audio, attached, demoted, font].concat();
        let cover = |index, demoted| EmbeddedCover { index, codec: "mjpeg".to_string(), demoted };
        assert_eq!(embedded_in(&listing), Embedded { covers: vec![cover(2, false), cover(3, true)], attachments: 1 });
        assert_eq!(embedded_in(&[video, audio].concat()), Embedded::default(), "video+audio, no cover");
        assert_eq!(embedded_in(""), Embedded::default(), "no streams");
    }

    #[test]
    fn the_thumbnail_pass_closes_on_every_count_that_is_not_zero() {
        let all = Tally { present: 298, restored: 1, embedded: 2, failed: 3, skipped: 4 };
        assert_eq!(all.summary(), "298 present, 1 restored, 2 embedded, 3 failed, 4 skipped");
        // A re-run that changed nothing says only that, and a fresh download only what it did.
        assert_eq!(Tally { present: 300, ..Tally::default() }.summary(), "300 present");
        assert_eq!(Tally { embedded: 1, ..Tally::default() }.summary(), "1 embedded");
        assert_eq!(Tally { failed: 1, skipped: 2, ..Tally::default() }.summary(), "1 failed, 2 skipped");
    }

    #[test]
    fn a_download_is_classified_as_mkv_other_video_or_audio() {
        // The three-way split the muxing passes report on: an mkv they can use, another video
        // container they cannot (so something IS lost and it should read as a problem), and an
        // audio extraction (where nothing was lost and it should not).
        assert!(is_mkv(Path::new("Title__abcdefghijk.mkv")));
        for video in ["Title__abcdefghijk.mp4", "Title__abcdefghijk.webm"] {
            let path = Path::new(video);
            assert!(!is_mkv(path), "{video}");
            assert!(!is_audio(path), "{video} is a video — calling it audio was the bug here");
        }
        for audio in ["s__id.opus", "s__id.m4a", "s__id.mp3", "s__id.ogg", "s__id.flac", "s__id.wav"] {
            let path = Path::new(audio);
            assert!(!is_mkv(path), "{audio}");
            assert!(is_audio(path), "{audio}");
        }
        // Every extension the lookup accepts has to land in exactly one bucket, or a pass could
        // find a file it has no wording for.
        for ext in VIDEO_EXTS.iter().chain(&AUDIO_EXTS) {
            let name = format!("Title__abcdefghijk{ext}");
            assert!(is_media_name(&name), "{name} must be findable");
            assert_ne!(
                is_audio(Path::new(&name)),
                VIDEO_EXTS.contains(ext),
                "{name} must be video or audio, not both and not neither"
            );
        }
        assert!(!is_media_name("notes__abcdefghijk.txt"), "not something we download");
    }

    #[test]
    fn discarding_stale_parts_takes_only_the_unfinishable_ones() {
        let dir = scratch_dir("stale_parts");
        std::fs::create_dir_all(dir.join("Season 3[9001234]")).unwrap();
        let write = |rel: &str| {
            std::fs::write(dir.join(rel), b"xxxx").unwrap();
            dir.join(rel)
        };
        // Must GO: the old scheme, at the root and nested in a collection folder.
        let old_root = write("1_m3n4o5p6.mp4.part");
        let old_nested = write("Season 3[9001234]/1_a1b2c3d4.mkv.part");
        // Must STAY, each for its own reason.
        let current = write("Season 3[9001234]/S3E01__1_aaaaaaaa.mp4.part"); // resumable
        let youtube = write("20240101__Title__abcdefghijk.mkv.part"); // resumable
        let browser = write("quarterly report.pdf.part"); // not ours at all
        let finished = write("1_m3n4o5p6.mp4"); // a finished download
        let sidecar = write("1_m3n4o5p6.en.vtt"); // not a partial

        discard_stale_parts(&dir);

        assert!(!old_root.exists(), "an old-scheme leftover must go");
        assert!(!old_nested.exists(), "including inside a collection folder");
        assert!(current.exists(), "a part the current scheme can resume must stay");
        assert!(youtube.exists(), "the YouTube naming carries `__` too, and is resumable");
        assert!(browser.exists(), "another program's partial is none of our business");
        assert!(finished.exists(), "a finished download is not a partial");
        assert!(sidecar.exists(), "a sidecar is not a partial");
        // Idempotent: a second sweep finds nothing and touches nothing.
        discard_stale_parts(&dir);
        assert!(current.exists() && finished.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_part_is_judged_by_all_three_conditions() {
        // The predicate on its own, so each condition is pinned independently of the walk.
        let stale = |name: &str| is_stale_part(Path::new(name));
        assert!(stale("1_m3n4o5p6.mp4.part"), "the case this exists for");
        assert!(stale("abc.mkv.part") && stale("abc.opus.part"), "any extension we write");
        assert!(!stale("1_m3n4o5p6.mp4"), "not a `.part` at all");
        assert!(!stale("notes.txt.part"), "not an extension we write");
        assert!(!stale("report.pdf.part"), "a browser's partial download");
        assert!(!stale("S3E01__abc.mp4.part"), "a resumable current-scheme part");
        assert!(!stale("a__b.mp4.part"), "`__` anywhere is enough to spare it");
        assert!(!stale(".part"), "a bare suffix has no extension to match");
    }

    #[test]
    fn find_by_id_walks_nested_dirs_and_matches_media_extensions_only() {
        let dir = scratch_dir("findbyid");
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/20200101__Title__abcdefghijk.mkv"), b"x").unwrap();
        std::fs::write(dir.join("notes__abcdefghijk.txt"), b"x").unwrap(); // not a media ext
        std::fs::write(dir.join("song__qrstuvwxyz1.opus"), b"x").unwrap();
        let found = find_by_id(&dir, "abcdefghijk", Prefer::Video).expect("nested mkv found");
        assert!(found.ends_with("a/b/20200101__Title__abcdefghijk.mkv"), "{found:?}");
        assert!(find_by_id(&dir, "qrstuvwxyz1", Prefer::Audio).is_some(), "audio extensions match too");
        // `.mp4` is what a generic single-format download lands as; while it was missing from the
        // list, every pass that locates a file by its id silently skipped those.
        std::fs::write(dir.join("clip__mp4vid00001.mp4"), b"x").unwrap();
        assert!(find_by_id(&dir, "mp4vid00001", Prefer::Video).is_some(), "generic downloads land as .mp4");
        assert!(find_by_id(&dir, "zzzzzzzzzzz", Prefer::Video).is_none(), "an unknown id finds nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two downloads of one video in reach — an audio extraction beside the video, another
    /// container, a copy in a collection folder further down — and each pass gets the one it works
    /// on, whichever the filesystem lists first. A pass finding only the other kind still gets it,
    /// to say why it cannot use it.
    #[test]
    fn each_pass_finds_the_download_it_works_on_among_several() {
        let dir = scratch_dir("findbykind");
        // Written in both orders, so the answer cannot come from the order they are listed in.
        for (id, first, second) in [("orderedvid1", "opus", "mkv"), ("orderedvid2", "mkv", "opus")] {
            std::fs::write(dir.join(format!("a__{id}.{first}")), b"x").unwrap();
            std::fs::write(dir.join(format!("a__{id}.{second}")), b"x").unwrap();
            let video = find_by_id(&dir, id, Prefer::Video).unwrap();
            assert!(video.ends_with(format!("a__{id}.mkv")), "{first} then {second}: {video:?}");
            let audio = find_by_id(&dir, id, Prefer::Audio).unwrap();
            assert!(audio.ends_with(format!("a__{id}.opus")), "{first} then {second}: {audio:?}");
        }
        // The mkv the passes mux into, before another video container — deeper down or not.
        std::fs::create_dir_all(dir.join("Mix [PLx]")).unwrap();
        std::fs::write(dir.join("b__webmormkv1.webm"), b"x").unwrap();
        std::fs::write(dir.join("Mix [PLx]/b__webmormkv1.mkv"), b"x").unwrap();
        let video = find_by_id(&dir, "webmormkv1", Prefer::Video).unwrap();
        assert!(video.ends_with("Mix [PLx]/b__webmormkv1.mkv"), "{video:?}");
        // No mkv: another video container still comes before the audio, in either order.
        for (id, first, second) in [("webmoropus1", "webm", "opus"), ("webmoropus2", "opus", "webm")] {
            std::fs::write(dir.join(format!("e__{id}.{first}")), b"x").unwrap();
            std::fs::write(dir.join(format!("e__{id}.{second}")), b"x").unwrap();
            let video = find_by_id(&dir, id, Prefer::Video).unwrap();
            assert!(video.ends_with(format!("e__{id}.webm")), "{first} then {second}: {video:?}");
        }
        // Only the other kind there: still found, so the pass can say what it found.
        std::fs::write(dir.join("c__onlyaudio01.opus"), b"x").unwrap();
        std::fs::write(dir.join("d__onlyvideo01.mkv"), b"x").unwrap();
        assert!(find_by_id(&dir, "onlyaudio01", Prefer::Video).unwrap().ends_with("c__onlyaudio01.opus"));
        assert!(find_by_id(&dir, "onlyvideo01", Prefer::Audio).unwrap().ends_with("d__onlyvideo01.mkv"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A folder the walk cannot read hides what is in it, and nothing else. Reading one used to
    /// end the whole lookup, with no file at all — and a walk ranking its matches goes on past
    /// one it does not want, into more folders, as it must here: the audio found first is no mkv.
    #[test]
    fn an_unreadable_folder_hides_only_what_is_in_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir("findlocked");
        let locked = dir.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(dir.join("s__lockedwalk1.opus"), b"x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let found = find_by_id(&dir, "lockedwalk1", Prefer::Video);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(found.is_some_and(|file| file.ends_with("s__lockedwalk1.opus")), "the walk must go on past it");
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

        assert_eq!(embedded(&file).expect("probe works").covers, [], "starts bare");
        assert!(attach_thumbnail(&file, &cover), "attach succeeds");
        let covers = embedded(&file).expect("probe works").covers;
        assert!(matches!(covers.as_slice(), [only] if !only.demoted), "probe sees the cover, attached: {covers:?}");
        let out = extract_cover(&file, &covers[0]).expect("extracts back out");
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
        let covers = embedded(&file).expect("probe works").covers;
        assert!(matches!(covers.as_slice(), [only] if !only.demoted), "the cover survived the remux, attached: {covers:?}");
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
        // The same video's audio extraction beside it: the titles to stamp are still the video's.
        std::fs::write(dir.join("v__abcdefghijk.opus"), b"OPUSDATA").unwrap();

        mark_auto_titles(&dir, "abcdefghijk", std::slice::from_ref(&auto), Env::default());
        let (_, titles) = subtitle_streams(&file).expect("probe works");
        assert_eq!(titles, ["English (auto-generated)"], "the machine origin is stamped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A small cover image at `path`, in `color` — distinct colors make distinct pictures.
    fn build_cover(path: &Path, color: &str) {
        let ok = std::process::Command::new(ff_bin("ffmpeg")).stdin(std::process::Stdio::null())
            .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", &format!("color=c={color}:s=32x32:d=1"), "-frames:v", "1"])
            .arg(path)
            .status()
            .is_ok_and(|status| status.success());
        assert!(ok, "could not build the cover");
    }

    /// What every other remux did to a cover before the carry: a bare `-map 0 -c copy`, which leaves
    /// it a plain video track.
    fn demote_covers(file: &Path) {
        let tmp = file.with_extension("demoting.mkv");
        let ok = std::process::Command::new(ff_bin("ffmpeg")).stdin(std::process::Stdio::null())
            .args(["-nostdin", "-v", "error", "-y", "-i"])
            .arg(file)
            .args(["-map", "0", "-c", "copy"])
            .arg(&tmp)
            .status()
            .is_ok_and(|status| status.success());
        assert!(ok && std::fs::rename(&tmp, file).is_ok(), "could not demote the cover");
    }

    /// Each stream of `file` as `type,attached_pic[,filename,mimetype]`.
    fn stream_rows(file: &Path) -> Vec<String> {
        let listing = capture_stdout(
            ff_bin("ffprobe"),
            [
                OsString::from("-v"), "error".into(),
                "-show_entries".into(), "stream=codec_type:stream_disposition=attached_pic:stream_tags=filename,mimetype".into(),
                "-of".into(), "csv=p=0".into(),
                file.as_os_str().to_owned(),
            ],
        )
        .expect("ffprobe runs");
        listing.lines().map(str::to_string).collect()
    }

    #[test]
    fn stamping_a_file_that_has_a_cover_keeps_it_attached() {
        // The case that used to demote it: the finishing pass reaching a file already on disk —
        // missing from the archive, so yt-dlp called it "already downloaded" — with its cover.
        if !ffmpeg_or_skip("stamping a covered file") {
            return;
        }
        let dir = scratch_dir("stampcover");
        let file = dir.join("v__abcdefghijk.mkv");
        build_test_mkv(&file);
        let cover = dir.join("cover.jpg");
        build_cover(&cover, "red");
        assert!(attach_thumbnail(&file, &cover));
        let sidecar = dir.join("v.en.vtt");
        std::fs::write(&sidecar, "WEBVTT\n\n00:00.000 --> 00:01.000\nhi\n").unwrap();
        let plain = Pick { key: "en".into(), name: "English".into(), auto: false };
        assert!(mux_subtitles(&file, &[(&plain, sidecar)], 0));

        let auto = Pick { key: "en".into(), name: "English".into(), auto: true };
        mark_auto_titles(&dir, "abcdefghijk", std::slice::from_ref(&auto), Env::default());
        let (_, titles) = subtitle_streams(&file).expect("probe works");
        assert_eq!(titles, ["English (auto-generated)"], "stamped");
        let covers = embedded(&file).expect("probe works").covers;
        assert!(matches!(covers.as_slice(), [only] if !only.demoted), "the cover stayed a cover: {covers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_thumbnail_pass_restores_a_demoted_cover_without_fetching_another() {
        // A demoted cover is no missing one: restored in place, the same picture — a fetch would
        // bring YouTube's (or fail, offline), and leave the old one beside it as a second track.
        if !ffmpeg_or_skip("restoring a demoted cover") {
            return;
        }
        let dir = scratch_dir("restorecover");
        let file = dir.join("v__abcdefghijk.mkv");
        build_test_mkv(&file);
        let cover = dir.join("cover.jpg");
        build_cover(&cover, "red");
        assert!(attach_thumbnail(&file, &cover));
        demote_covers(&file);
        let demoted = embedded(&file).expect("probe works").covers;
        assert!(matches!(demoted.as_slice(), [only] if only.demoted), "set-up: demoted: {demoted:?}");

        embed_thumbnails(&dir, &["abcdefghijk".to_string()]);
        let covers = embedded(&file).expect("probe works").covers;
        assert!(matches!(covers.as_slice(), [only] if !only.demoted), "one cover, attached again: {covers:?}");
        let out = extract_cover(&file, &covers[0]).expect("extracts");
        assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&cover).unwrap(), "the very picture it had");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".cover-keep-") || name.ends_with(".covering.mkv"))
            .collect();
        assert_eq!(leftovers, [out.file_name().unwrap().to_string_lossy().into_owned()], "no temp file but the test's own extract");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_doubled_cover_is_carried_once_and_a_different_one_kept_second() {
        // The double a fetch beside a demoted cover left: the same picture twice comes out as one
        // cover; a different second picture is kept, named so the first stays the one shown.
        if !ffmpeg_or_skip("doubled covers") {
            return;
        }
        let dir = scratch_dir("doublecover");
        let red = dir.join("red.jpg");
        build_cover(&red, "red");
        let green = dir.join("green.jpg");
        build_cover(&green, "green");
        for (second, kept) in [(&red, vec!["cover.jpg"]), (&green, vec!["cover.jpg", "cover-2.jpg"])] {
            let file = dir.join("v__abcdefghijk.mkv");
            build_test_mkv(&file);
            assert!(attach_thumbnail(&file, &red));
            demote_covers(&file);
            assert!(attach_thumbnail(&file, second), "the fetch's new cover beside the demoted one");

            embed_thumbnails(&dir, &["abcdefghijk".to_string()]);
            let covers = embedded(&file).expect("probe works").covers;
            assert!(covers.iter().all(|cover| !cover.demoted), "{covers:?}");
            let names: Vec<String> = stream_rows(&file)
                .iter()
                .filter(|row| row.starts_with("video,1,"))
                .map(|row| row.split(',').nth(2).unwrap_or_default().to_string())
                .collect();
            assert_eq!(names, kept, "second cover {}", second.display());
            // `cover.jpg`, the name thumbnailers prefer, is the cover that was attached already.
            let shown = extract_cover(&file, &covers[0]).expect("extracts");
            assert_eq!(std::fs::read(&shown).unwrap(), std::fs::read(second).unwrap(), "second cover {}", second.display());
            let _ = std::fs::remove_file(&shown);
            let _ = std::fs::remove_file(&file);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_demoted_cover_that_does_not_come_out_is_left_as_it_is() {
        // A cover in an image type vidl keeps no covers in (here GIF: they are JPEG, PNG for ones
        // from elsewhere) is not extracted to attach again: the file is left untouched, not
        // rewritten for nothing and called restored.
        if !ffmpeg_or_skip("an unextractable cover") {
            return;
        }
        let dir = scratch_dir("gifcover");
        let plain = dir.join("plain.mkv");
        build_test_mkv(&plain);
        let file = dir.join("v__abcdefghijk.mkv");
        let ok = std::process::Command::new(ff_bin("ffmpeg")).stdin(std::process::Stdio::null())
            .args(["-nostdin", "-v", "error", "-y", "-i"])
            .arg(&plain)
            .args(["-f", "lavfi", "-i", "color=c=red:s=32x32:d=1"])
            .args(["-map", "0", "-map", "1", "-c", "copy", "-c:v:1", "gif", "-frames:v:1", "1", "-metadata:s:v:1", "mimetype=image/gif"])
            .arg(&file)
            .status()
            .is_ok_and(|status| status.success());
        assert!(ok, "could not build the file");
        let covers = embedded(&file).expect("probe works").covers;
        assert!(matches!(covers.as_slice(), [only] if only.demoted && only.codec == "gif"), "set-up: {covers:?}");
        let before = std::fs::read(&file).unwrap();

        embed_thumbnails(&dir, &["abcdefghijk".to_string()]);
        assert_eq!(std::fs::read(&file).unwrap(), before, "the file is left as it was");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_carried_cover_is_attached_after_the_files_own_attachments() {
        // `-map 0` carries a font itself, numbered first: the re-attached cover's name and type go
        // to the attachment after it — not over the font's.
        if !ffmpeg_or_skip("cover beside a font") {
            return;
        }
        let dir = scratch_dir("fontcover");
        let file = dir.join("v__abcdefghijk.mkv");
        build_test_mkv(&file);
        let font = dir.join("font.ttf");
        std::fs::write(&font, "not really a font").unwrap();
        let fonted = dir.join("fonted.mkv");
        let ok = std::process::Command::new(ff_bin("ffmpeg")).stdin(std::process::Stdio::null())
            .args(["-nostdin", "-v", "error", "-y", "-i"])
            .arg(&file)
            .args(["-map", "0", "-c", "copy", "-attach"])
            .arg(&font)
            .args(["-metadata:s:t:0", "mimetype=application/x-truetype-font", "-metadata:s:t:0", "filename=font.ttf"])
            .arg(&fonted)
            .status()
            .is_ok_and(|status| status.success());
        assert!(ok && std::fs::rename(&fonted, &file).is_ok(), "could not attach the font");
        let cover = dir.join("cover.jpg");
        build_cover(&cover, "red");
        assert!(attach_thumbnail(&file, &cover), "a cover numbered after the font");

        let sidecar = dir.join("v.en.vtt");
        std::fs::write(&sidecar, "WEBVTT\n\n00:00.000 --> 00:01.000\nhi\n").unwrap();
        let pick = Pick { key: "en".into(), name: "English".into(), auto: false };
        assert!(mux_subtitles(&file, &[(&pick, sidecar)], 0), "the mux carries the cover past the font");
        let rows = stream_rows(&file);
        assert!(rows.contains(&"attachment,0,font.ttf,application/x-truetype-font".to_string()), "the font as it was: {rows:?}");
        assert!(rows.contains(&"video,1,cover.jpg,image/jpeg".to_string()), "the cover, attached: {rows:?}");
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
        // The same video's video beside each: the tags still go into the audio.
        std::fs::write(dir.join("song__abcdefghijk.mkv"), b"not audio").unwrap();
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
