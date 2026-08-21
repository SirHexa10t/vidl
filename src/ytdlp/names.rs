//! What a downloaded file is called, and what is recorded when its name had to be cut short.
//!
//! Two of the output templates interpolate a free-text title ([`YT_NAME`], [`GENERIC_NAME`]), and
//! a title has no length a filesystem would respect. A Facebook or TikTok post puts its whole
//! caption in that field, so a "title" of several hundred characters is ordinary rather than
//! exotic — and a path component over [`NAME_MAX`] bytes cannot be created at all. yt-dlp reports
//! that as `unable to open for writing: [Errno 36] File name too long` and the download is lost,
//! which is why the title is bounded *inside the template*, in bytes, at [`TITLE_BYTES`].
//!
//! Bounding it throws information away, so it isn't done silently. Every download names itself
//! into [`NAMES_SCRATCH`] as it finishes; [`note_shortened`] reads that back, announces each title
//! that had to be cut, and appends it in full to [`MODIFIED_LEDGER`] beside the media. The file
//! name stops being the whole title; the record keeps it.
//!
//! # Why not `--trim-filenames`
//!
//! yt-dlp's own flag cannot do this job, in three separate ways. It splits the name on the last
//! two `.` and truncates only what precedes them, so **any period inside the title leaves most of
//! it in place** — a 648-byte name came back 542 bytes under `--trim-filenames 150`, still far
//! over the limit. It counts characters where the limit is bytes. And its budget includes the
//! directory prefix, so the same flag bounds less and less as `--into` grows longer. A bounded
//! field in the template has none of those properties.

use std::ffi::OsString;
use std::path::Path;

use crate::util::{stamp, style};

/// The longest single path component the filesystems this runs on accept, in bytes (`NAME_MAX`).
/// A component over this is refused outright — it is not truncated for us.
pub(crate) const NAME_MAX: usize = 255;

/// The byte budget for a title inside an output template.
///
/// The remaining [`NAME_MAX`] − 150 = 105 bytes cover everything the template puts *around* the
/// title and everything yt-dlp adds while working: the ` [`…`].` or `__`…`__` framing, the video
/// id (16 digits on Facebook, longer elsewhere), the extension, the per-format and in-progress
/// suffixes (`.f616`, `.part`, `.part-FragNNN`), a subtitle sidecar's language suffix, and the
/// widening the filename sanitizer does when it substitutes a wide character for a reserved one
/// (`?` → `？` is one byte becoming three).
pub(crate) const TITLE_BYTES: usize = 150;

/// The title, bounded, as an output template spells it.
///
/// `B` is yt-dlp's **byte** truncation — internally `b'%.150s' % title.encode()` followed by a
/// lossy decode, so a multi-byte character sliced down the middle is dropped rather than left
/// mangled. Character truncation (`.150s`) would not do: the caption that prompted this measured
/// 604 characters but 652 bytes, and it is bytes the filesystem counts.
pub(crate) const TITLE_FIELD: &str = "%(title).150B";

/// The file name every YouTube mode shares: sortable upload date, title, and the video id that
/// keeps any file traceable back to its source (ideas kept from the old dl_youtube.py). The title
/// is bounded per [`TITLE_FIELD`] — YouTube caps titles at 100 characters, which is safe in ASCII
/// and is not safe at all in CJK or emoji, where 100 characters run to 300–400 bytes.
pub(crate) const YT_NAME: &str = "%(upload_date)s__%(title).150B__%(id)s.%(ext)s";

/// Output template for a generic (non-YouTube) single download: title + id, flat under the
/// destination. Simpler than [`YT_NAME`] — a random site rarely carries a reliable upload date,
/// and there's no collection to sort into. The `[id]` also gives `scrub_ledger` a key to match.
/// The title is bounded per [`TITLE_FIELD`]; this is the template that first needed it.
pub(crate) const GENERIC_NAME: &str = "%(title).150B [%(id)s].%(ext)s";

/// Where a cut title is kept in full, dropped inside whatever folder owns the collection —
/// alongside the download archive and the failure ledger, since all three are this crate's
/// bookkeeping about the same set of files.
pub(crate) const MODIFIED_LEDGER: &str = ".dl_video_modified";

/// The scratch file each download names itself into, so the run can tell what the templates
/// actually got handed. Transient: [`note_shortened`] consumes and removes it. It lives beside
/// the archive rather than in the temp dir because that directory is this crate's bookkeeping
/// home, is known-writable, and a copy left behind by a killed run is picked up by the next one
/// instead of leaking into a shared location under a name two concurrent runs could collide on.
const NAMES_SCRATCH: &str = ".dl_video_names";

/// What each download writes into [`NAMES_SCRATCH`]: the id that keys the record, then the title
/// exactly as the extractor gave it — deliberately *not* bounded, since the whole point is to keep
/// what the file name could not.
const RECORD_FORMAT: &str = "%(id)s\t%(title)s";

/// Whether the output templates will cut `title` — the same comparison yt-dlp's `B` conversion
/// makes, on bytes, so this answers "was it cut" without having to ask yt-dlp what it did.
pub(crate) fn is_shortened(title: &str) -> bool {
    title.len() > TITLE_BYTES
}

/// Have each download record `<id>\t<title>` as it finishes, for [`note_shortened`] to read.
///
/// `--print-to-file` and not `--print`: `--print` implies `--quiet` and (at this stage)
/// `--simulate`, which would cost the live progress output and the download itself. This one
/// implies neither, so it rides along on the download invocation and adds no request of its own —
/// which matters, because the generic path is deliberately one invocation end to end.
pub(crate) fn record_flags(dir: &Path) -> Vec<OsString> {
    vec![
        "--print-to-file".into(),
        RECORD_FORMAT.into(),
        dir.join(NAMES_SCRATCH).into_os_string(),
    ]
}

/// Announce every title the templates had to cut, and append it in full to [`MODIFIED_LEDGER`].
///
/// Reads and removes the scratch left by [`record_flags`], so calling it twice is harmless and a
/// name recorded by a diagnostic retry is picked up by the next call rather than lost. Titles
/// that fit are the overwhelming majority and leave no trace: this file exists to answer "what
/// was this called before you shortened it", and an entry per download would bury the answer.
pub(crate) fn note_shortened(dir: &Path) {
    let scratch = dir.join(NAMES_SCRATCH);
    let Ok(text) = std::fs::read_to_string(&scratch) else { return };
    let _ = std::fs::remove_file(&scratch);
    let cut: Vec<(String, String)> =
        parse_names(&text).into_iter().filter(|(_, title)| is_shortened(title)).collect();
    if cut.is_empty() {
        return;
    }
    // Red, and on stderr, as each is found: the file that just landed is not called what its
    // source calls it, and that is worth noticing during the run rather than in a file afterwards.
    for (key, title) in &cut {
        eprintln!(
            "{}",
            style::problematic(&format!(
                "vidl: {key}'s title is {} bytes — the file name keeps the first {TITLE_BYTES}",
                title.len()
            ))
        );
    }
    write_modified(dir, &cut);
}

/// Append `cut` to [`MODIFIED_LEDGER`] and say where it went. Same shape as the failure ledger:
/// a dated block per run, appended, so a re-run adds to the history instead of replacing it.
fn write_modified(dir: &Path, cut: &[(String, String)]) {
    let path = dir.join(MODIFIED_LEDGER);
    let mut block = format!("── {} ──\n", stamp::datehour_stamp());
    for (key, title) in cut {
        block += &record_line(key, title);
        block.push('\n');
    }
    use std::io::Write;
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| file.write_all(block.as_bytes()));
    match written {
        Ok(()) => println!(
            "{} file name(s) shortened — the full title(s) are in {}",
            cut.len(),
            path.display()
        ),
        Err(err) => eprintln!("vidl: could not write {}: {err}", path.display()),
    }
}

/// One record entry: the key a reader searches by, then the title in full, tab-separated.
///
/// Control characters and tabs in the title become spaces. A caption with hard line breaks in it
/// would otherwise split one entry across several lines, where its tail reads like another key.
fn record_line(key: &str, title: &str) -> String {
    let flat: String =
        title.chars().map(|c| if c.is_control() || c == '\t' { ' ' } else { c }).collect();
    format!("{key}\t{flat}")
}

/// Parse the scratch into `(id, title)` pairs.
///
/// One `<id>\t<title>` line per download — except that yt-dlp writes the title verbatim, so a
/// title containing hard line breaks arrives as several lines. A line with no tab is therefore
/// the continuation of the title above it and not a new entry; treating it as one would split a
/// long title into fragments that each look short enough to leave unrecorded.
fn parse_names(text: &str) -> Vec<(String, String)> {
    let mut named: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        match line.split_once('\t') {
            Some((id, title)) => named.push((id.trim().to_string(), title.to_string())),
            None => {
                if let Some((_, title)) = named.last_mut() {
                    title.push(' ');
                    title.push_str(line);
                }
            }
        }
    }
    named
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::scratch_dir;

    #[test]
    fn the_templates_bound_the_title_and_agree_on_the_budget() {
        // TITLE_BYTES is the documented budget; the templates spell the number themselves,
        // because a `const` template cannot interpolate one. This is what stops the two drifting.
        assert_eq!(TITLE_FIELD, format!("%(title).{TITLE_BYTES}B"));
        for template in [YT_NAME, GENERIC_NAME] {
            assert!(template.contains(TITLE_FIELD), "{template} must bound the title");
            assert!(
                !template.contains("%(title)s"),
                "{template} still interpolates the title unbounded"
            );
        }
    }

    #[test]
    fn a_bounded_name_leaves_room_for_the_framing_and_ytdlps_working_suffixes() {
        // The budget is only correct if what the template puts around the title, plus what yt-dlp
        // appends while working, still fits inside one path component. Worst case measured
        // against the real thing: a 20-byte id, a 4-byte extension, a per-format suffix, and the
        // longest in-progress suffix a fragmented download uses.
        let framing = |template: &str| template.len() - TITLE_FIELD.len();
        let fields = 8 /* upload_date */ + 20 /* id */ + 4 /* ext */;
        let working = ".f616".len() + ".part-Frag1234".len();
        for template in [YT_NAME, GENERIC_NAME] {
            let worst = TITLE_BYTES + framing(template) + fields + working;
            assert!(
                worst <= NAME_MAX,
                "{template} can still reach {worst} bytes, over NAME_MAX ({NAME_MAX})"
            );
        }
    }

    #[test]
    fn a_title_is_measured_in_bytes_not_characters() {
        // The whole bug in one assertion: this title is well under TITLE_BYTES *characters* and
        // well over TITLE_BYTES *bytes*, which is the number the filesystem counts.
        let cjk = "日".repeat(60); // 60 characters, 180 bytes
        assert_eq!(cjk.chars().count(), 60);
        assert_eq!(cjk.len(), 180);
        assert!(is_shortened(&cjk), "a byte-length check is what catches this");

        // And the boundary is exact, on either side of it.
        assert!(!is_shortened(&"a".repeat(TITLE_BYTES)), "exactly the budget still fits");
        assert!(is_shortened(&"a".repeat(TITLE_BYTES + 1)), "one byte over does not");
        assert!(!is_shortened(""), "no title, nothing to cut");
    }

    #[test]
    fn a_record_entry_stays_one_greppable_line_per_name() {
        // A caption with hard line breaks must not become several entries whose tails read like
        // keys — the record is searched by the key in column one.
        let line = record_line("vid1", "first\nsecond\twith a tab\r\nthird");
        assert_eq!(line, "vid1\tfirst second with a tab  third");
        assert_eq!(line.lines().count(), 1, "one entry, one line");
        assert_eq!(line.matches('\t').count(), 1, "only the key/title separator is a tab");
    }

    #[test]
    fn a_multi_line_title_is_read_back_as_one_title_not_several_entries() {
        // The mirror of the above, on the way in: yt-dlp writes the title verbatim, so the
        // continuation lines belong to the entry above them.
        let parsed = parse_names("vid1\tone\ntwo\nthree\nvid2\tplain\n");
        assert_eq!(parsed.len(), 2, "two downloads, not four: {parsed:?}");
        assert_eq!(parsed[0], ("vid1".to_string(), "one two three".to_string()));
        assert_eq!(parsed[1], ("vid2".to_string(), "plain".to_string()));
        // A continuation with nothing above it has no entry to join, and is not one itself.
        assert!(parse_names("orphan\n").is_empty());
        assert!(parse_names("").is_empty());
    }

    #[test]
    fn only_the_titles_that_were_cut_are_recorded_and_the_scratch_is_consumed() {
        let dir = scratch_dir("names_record");
        let scratch = dir.join(NAMES_SCRATCH);
        let ledger = dir.join(MODIFIED_LEDGER);

        // A run whose titles all fit leaves no record at all — the file answers "what was this
        // called before you shortened it", so an entry per download would bury the answer.
        std::fs::write(&scratch, "short1\tA fine title\nshort2\tAnother\n").unwrap();
        note_shortened(&dir);
        assert!(!ledger.exists(), "nothing was cut, so nothing is recorded");
        assert!(!scratch.exists(), "the scratch is consumed either way");

        // A cut title is recorded in full, keyed by the id, and the scratch is gone.
        let long = "x".repeat(TITLE_BYTES + 40);
        std::fs::write(&scratch, format!("vid1\t{long}\nvid2\tfits fine\n")).unwrap();
        note_shortened(&dir);
        let text = std::fs::read_to_string(&ledger).expect("the record exists now");
        assert!(text.contains(&format!("vid1\t{long}")), "the full title, keyed by id: {text}");
        assert!(!text.contains("vid2"), "a title that fit must not be recorded: {text}");
        assert!(!scratch.exists(), "the scratch is consumed");

        // A second run appends rather than replacing: the history of a re-run stays readable.
        let other = "y".repeat(TITLE_BYTES + 1);
        std::fs::write(&scratch, format!("vid3\t{other}\n")).unwrap();
        note_shortened(&dir);
        let text = std::fs::read_to_string(&ledger).unwrap();
        assert!(text.contains("vid1\t") && text.contains("vid3\t"), "both runs are kept: {text}");
        assert_eq!(text.matches("── ").count(), 2, "one dated block per run: {text}");

        // No scratch is the normal case (a run in which yt-dlp wrote nothing) and must be silent.
        note_shortened(&dir);
        assert_eq!(std::fs::read_to_string(&ledger).unwrap(), text, "no scratch, no change");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_record_flags_ride_the_download_and_never_imply_quiet_or_simulate() {
        let flags: Vec<String> =
            record_flags(Path::new("/dl")).iter().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(flags[0], "--print-to-file", "`--print` would imply --quiet and --simulate");
        assert_eq!(flags[1], RECORD_FORMAT, "the id keys the record; the title is unbounded");
        assert!(
            !RECORD_FORMAT.contains(TITLE_FIELD),
            "the record must keep the title the file name could not"
        );
        assert_eq!(flags[2], format!("/dl/{NAMES_SCRATCH}"));
    }
}
