//! What a downloaded file is called, and what is recorded when its name had to be cut short.
//!
//! A file name carries a free-text title, and a title has no length a filesystem would respect. A
//! Facebook or TikTok post puts its whole caption in that field, so a "title" of several hundred
//! characters is ordinary rather than exotic — and a path component over the limit in force cannot
//! be created at all. yt-dlp reports that as `unable to open for writing: [Errno 36] File name too
//! long` and the download is simply lost.
//!
//! There are two ways out of that, and this module uses both, because the two halves of the crate
//! know different things at the moment the name has to be decided.
//!
//! # On YouTube: bound the title inside the template
//!
//! Here the fields are *predictable*. A YouTube id is always 11 bytes and its format ids are three
//! characters, so a title's budget can be worked out up front and handed to yt-dlp as
//! `%(title).<n>B`, leaving it to do the naming — which matters, because `find_by_id` locates a
//! downloaded file by the literal `__<id>.` marker [`yt_name`] ends with, and the thumbnail and
//! subtitle patch passes are built on that.
//!
//! The budget is derived rather than picked: whatever the limit has left once the separators, the
//! id, the extension and yt-dlp's working suffixes are reserved ([`yt_title_budget`]). Deriving
//! matters more than the bytes it wins. The first version of this module hand-picked 150 and
//! reserved five bytes for the `.f<format_id>` tag — the width of YouTube's `616`. Facebook issues
//! eighteen-character format ids, so the real worst case was nine bytes over the limit while a test
//! asserted it fit. A literal hid that; named reserves do not.
//!
//! # Off YouTube: download to a name that cannot overrun, then rename
//!
//! Here nothing is predictable — the title is a caption, and yt-dlp's fallback extractor builds the
//! id out of the URL, so both are free text. A template would have to be handed over before either
//! length is known, and every reserve against that ignorance costs title.
//!
//! So [`temp_name`] downloads to the id alone and [`rename_downloaded`] puts the real name on
//! afterwards, when the title, the id and the extension are all known exactly and yt-dlp's working
//! suffixes are gone. Nothing is reserved for a guess, and [`sanitize`] substitutes byte-for-byte
//! rather than widening, so a measured title stays measured: **232 bytes of title, where a bounded
//! template on this path had to settle for 151** (measured, before that template was removed as the
//! rename made it redundant). The failure mode goes with it — a name that cannot be written is now
//! impossible rather than merely recoverable, and a rename that fails leaves a file called by its
//! id, which is a poor name and not a lost download.
//!
//! This is also why the generic path is not asked for the title up front. Measured against the URL
//! that prompted all this, a metadata-only query cost 23.4s against 24.0s for the download itself:
//! nearly all of a download *is* that extraction, so querying first doesn't replace the work, it
//! repeats it.
//!
//! # Either way, nothing is thrown away silently
//!
//! Every download names itself into [`NAMES_SCRATCH`] as it finishes. Whichever strategy ran reads
//! that back, announces each name that had to be cut, and appends the title in full to
//! [`MODIFIED_LEDGER`] beside the media — keyed by the id, also in full. The file name stops being
//! the whole truth; the record keeps it.
//!
//! # Why not `--trim-filenames`
//!
//! yt-dlp's own flag cannot do this job, in three separate ways. It splits the name on the last
//! two `.` and truncates only what precedes them, so **any period inside the title leaves most of
//! it in place** — a 648-byte name came back 542 bytes under `--trim-filenames 150`, still far
//! over the limit. It counts characters where the limit is bytes. And its budget includes the
//! directory prefix, so the same flag bounds less and less as `--into` grows longer. A bounded
//! field in the template has none of those properties.
//!
//! # What the YouTube budgets do not cover
//!
//! The rename path has no residual — it measures what it writes. The template path does, because it
//! is still guessing: **yt-dlp's sanitizer widens after the budget has run**, substituting a
//! three-byte fullwidth character for each of `<>:"/\|?*`, so a finished name can grow two bytes
//! per reserved character. [`SANITIZER_RESERVE`] covers a realistic number of them; a title made
//! largely of those characters would overrun anyway, as would a format id wider than reserved.
//!
//! Rather than shrink every ordinary title to make that impossible, the budget stays generous and a
//! miss is made survivable: a refused name earns one retry at [`halve_title_bound`], which turns a
//! lost download into a file with an uglier name. That is why being slightly wrong here is cheap,
//! and why the budget can afford to be optimistic.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::Path;
use std::sync::OnceLock;

use crate::util::{stamp, style};

/// The path-component limit in force, measured or supplied once per process.
static LIMIT: OnceLock<usize> = OnceLock::new();

/// Declare the limit every template will be built against. The first call wins.
///
/// A `OnceLock` for the same reason [`crate::tools`] uses one: this is a property of *where the run
/// writes*, settled before the first download and constant for the rest of it, and threading it
/// through every argv builder would distort each signature to serve a case that doesn't arise — a
/// run has one destination. The stated cost is that a library caller driving two destinations on
/// filesystems with different limits gets the first one's.
pub(crate) fn install_limit(bytes: usize) {
    let _ = LIMIT.set(bytes.max(MIN_TITLE_BYTES));
}

/// The limit in force: whatever [`install_limit`] was given, else [`DEFAULT_NAME_MAX`].
pub(crate) fn limit() -> usize {
    *LIMIT.get_or_init(|| DEFAULT_NAME_MAX)
}

/// Measure the longest file name `dir` actually accepts, by trying to create one.
///
/// Measured rather than reported: the number is available from `statfs`, but `std` doesn't expose
/// it, so reading it would mean adding a C-bindings dependency for one integer — and a reported
/// limit describes the underlying device, while a created file has to satisfy every layer above it
/// too. This asks the only question that matters, of the directory that will hold the files.
///
/// One probe in the common case: [`DEFAULT_NAME_MAX`] is tried first and almost always fits. A
/// destination that refuses it costs a bounded search of about eight more.
pub(crate) fn probe_limit(dir: &Path) -> usize {
    if accepts(dir, DEFAULT_NAME_MAX) {
        return DEFAULT_NAME_MAX;
    }
    // Something refused it. If even a short name won't write, this is not a length problem —
    // an unwritable destination is the download's error to report, with its own errno, not ours
    // to reinterpret as a tiny filesystem.
    if !accepts(dir, MIN_TITLE_BYTES) {
        return DEFAULT_NAME_MAX;
    }
    let (mut low, mut high) = (MIN_TITLE_BYTES, DEFAULT_NAME_MAX);
    while low < high {
        let mid = (low + high + 1) / 2;
        if accepts(dir, mid) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low
}

/// Whether `dir` accepts a file name of exactly `bytes` bytes. The probe file is created and
/// removed immediately; its name says what it is, so one left behind by a killed run is obvious.
fn accepts(dir: &Path, bytes: usize) -> bool {
    let name: String = ".dl_video_probe".chars().chain(std::iter::repeat('x')).take(bytes).collect();
    let path = dir.join(name);
    match std::fs::File::create(&path) {
        Ok(_) => {
            let _ = std::fs::remove_file(&path);
            true
        }
        Err(_) => false,
    }
}

/// The path-component limit assumed when nothing better is known: 255 bytes, which holds for
/// ext4, xfs, btrfs, APFS and NTFS. [`probe_limit`] measures the real one; `--name-max` overrides
/// both. A component over the limit in force is refused outright — it is not truncated for us.
pub(crate) const DEFAULT_NAME_MAX: usize = 255;

/// Bytes held back for the filename sanitizer, which runs *after* the byte budgets and substitutes
/// a three-byte fullwidth character for each of `<>:"/\|?*` — two bytes of growth apiece. This
/// covers eight of them, which is generous for real titles; see the module header for the residual.
const SANITIZER_RESERVE: usize = 16;

/// The shortest title worth putting in a name. If the limit in force cannot afford even this, the
/// destination cannot hold this crate's names at all and saying so beats truncating to nothing.
pub(crate) const MIN_TITLE_BYTES: usize = 24;

/// The byte budget for a *generic* site's id.
///
/// A named extractor hands out a short id — 11 bytes on YouTube, 16 on Facebook, 19 on TikTok —
/// but yt-dlp's fallback `generic` extractor derives one from the URL instead: `_generic_id` is
/// the last path segment, URL-unquoted. A direct media link whose filename is long therefore
/// produces an id just as long, and that is the *same* overrun the title bound exists to prevent,
/// wearing a different field. Measured against real yt-dlp: a 120-byte id took an
/// already-bounded-title name to 277 bytes.
///
/// 40 clears every named extractor's id with room to spare, and cutting a URL-derived one costs
/// nothing — on this path the id is a fragment of a filename, not a key. Nothing reads it back
/// off disk: `scrub_ledger` matches its ledger label against the download archive, and
/// `find_by_id` only ever looks at YouTube-shaped names.
pub(crate) const ID_BYTES: usize = 40;

/// A YouTube video id: always 11 characters, which is why [`yt_name`] can both leave it whole —
/// `find_by_id` matches it — and still afford a generous title.
const YT_ID_BYTES: usize = 11;

/// Bytes reserved for yt-dlp's `.f<format_id>` per-format tag.
///
/// Only the YouTube templates need one, and only because they let yt-dlp do the naming: its format
/// ids are three characters (`616`, `251`), so eight is generous. Getting this wrong is how the
/// first budget came out nine bytes optimistic — it reserved five, and Facebook turned out to issue
/// eighteen-character format ids. Off YouTube nothing is reserved at all now, because
/// [`rename_downloaded`] runs after those suffixes are gone.
const YT_FORMAT_ID: usize = 8;

/// What yt-dlp appends to a finished name while it is still working, given a format-id and
/// extension reserve: the per-format tag, the extension, the in-progress marker, and a fragment
/// index. Confirmed against yt-dlp's own `prepend_extension`, which turns
/// `Title [id].mp4` into `Title [id].f1016219494584300v.mp4` before `.part-Frag1234` is added.
const fn working_suffix(format_id: usize, ext: usize) -> usize {
    // ".f" + format_id + "." + ext + ".part" + "-Frag" + up to four digits
    2 + format_id + 1 + ext + 5 + 5 + 4
}

/// The longest extension in play. `webm` rather than the three-letter default because
/// `-- --merge-output-format webm/mkv` is a documented override.
const EXT_BYTES: usize = 4;

/// The byte budget a title gets, once everything else in the component is reserved.
///
/// `literals` is the template's own separators (excluding the dot before the extension, which
/// [`working_suffix`] counts). Saturating, so an absurdly small `--name-max` yields zero rather
/// than wrapping — callers compare against [`MIN_TITLE_BYTES`] and refuse.
const fn title_budget(limit: usize, literals: usize, id: usize, format_id: usize) -> usize {
    limit.saturating_sub(literals + id + working_suffix(format_id, EXT_BYTES) + SANITIZER_RESERVE)
}

/// The title budget inside [`yt_name`]: what the limit has left once the date, separators, id,
/// extension and yt-dlp's working suffixes are reserved. 187 bytes at the default limit, which
/// covers any of YouTube's 100-character titles up to about 1.9 bytes per character.
///
/// It can be this generous because YouTube's fields are *predictable* — an 11-byte id, three-
/// character format ids. That is the whole reason the YouTube paths can hand yt-dlp a template at
/// all, where off YouTube the same unknowns forced the rename in [`rename_downloaded`].
pub(crate) const fn yt_title_budget(limit: usize) -> usize {
    // "20240101" + "__" + <title> + "__" + <id>
    title_budget(limit, 8 + 2 + 2, YT_ID_BYTES, YT_FORMAT_ID)
}

/// A title field bounded to `bytes`.
///
/// `B` is yt-dlp's **byte** truncation — internally `b'%.150s' % title.encode()` followed by a
/// lossy decode, so a multi-byte character sliced down the middle is dropped rather than left
/// mangled. Character truncation (`.150s`) would not do: the caption that prompted this measured
/// 604 characters but 652 bytes, and it is bytes the filesystem counts.
pub(crate) fn title_field(bytes: usize) -> String {
    format!("%(title).{bytes}B")
}

/// The file name every YouTube mode shares: sortable upload date, title, and the video id that
/// keeps any file traceable back to its source (ideas kept from the old dl_youtube.py).
///
/// The id is deliberately left whole. YouTube's is always 11 characters, so there is nothing to
/// gain, and `find_by_id` locates a downloaded file by the literal `__<id>.` marker this template
/// ends with — a truncated id would break the thumbnail and subtitle patch passes for no benefit.
pub(crate) fn yt_name(limit: usize) -> String {
    format!("%(upload_date)s__{}__%(id)s.%(ext)s", title_field(yt_title_budget(limit)))
}

/// What a playlist entry's template adds ahead of [`yt_name`]: `%(playlist_index)03d__`, which
/// expands to three digits and two underscores. It shares the path component, so it comes out of
/// the title's budget rather than being free.
const COLLECTION_PREFIX: usize = 3 + 2;

/// The title budget inside [`yt_collection_name`] — [`yt_title_budget`] less the index prefix.
pub(crate) const fn yt_collection_title_budget(limit: usize) -> usize {
    yt_title_budget(limit).saturating_sub(COLLECTION_PREFIX)
}

/// [`yt_name`] with a playlist entry's ordering prefix, so a collection's folder sorts by position.
pub(crate) fn yt_collection_name(limit: usize) -> String {
    format!(
        "%(playlist_index)03d__%(upload_date)s__{}__%(id)s.%(ext)s",
        title_field(yt_collection_title_budget(limit))
    )
}

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

/// What each download writes into [`NAMES_SCRATCH`]: the id that keys the record, the path the
/// file actually landed at, and the title exactly as the extractor gave it — the title
/// deliberately *not* bounded, since the whole point is to keep what a file name could not.
///
/// `after_move:` rather than the default stage, because the path is only true once yt-dlp has
/// finished moving the file to it. That is also what makes [`rename_downloaded`] possible: the
/// generic path downloads to a name it chose, and this is how it learns which file to rename.
const RECORD_FORMAT: &str = "after_move:%(id)s\t%(filepath)s\t%(title)s";

/// The name the generic path downloads *to*: the id alone, bounded, so it cannot overrun any
/// limit whatever the site calls the video. The real name is put on afterwards by
/// [`rename_downloaded`], which by then knows the title, the id and the extension exactly and so
/// needs none of the reserves a template handed to yt-dlp does.
pub(crate) fn temp_name() -> String {
    format!("%(id).{ID_BYTES}B.%(ext)s")
}

/// The characters no mainstream filesystem takes in a name, and what they become.
///
/// A single-byte substitute on purpose. yt-dlp swaps in fullwidth lookalikes, which are three
/// bytes each and grow a name *after* it was measured — the residual this module used to have to
/// reserve against. Replacing byte-for-byte means a cut title's length is exactly what was
/// measured, so [`final_name`] needs no sanitizer reserve at all.
const RESERVED: [(char, char); 9] = [
    ('/', '-'),
    ('\\', '-'),
    (':', '-'),
    ('*', '-'),
    ('?', '-'),
    ('"', '\''),
    ('<', '('),
    ('>', ')'),
    ('|', '-'),
];

/// `text` as one safe path component: reserved characters substituted, control characters (a
/// caption's hard line breaks) turned into spaces, and the leading/trailing dots and spaces that
/// make a name awkward or hidden trimmed off.
fn sanitize(text: &str) -> String {
    let swapped: String = text
        .chars()
        .map(|c| {
            if c.is_control() {
                ' '
            } else {
                RESERVED.iter().find(|(from, _)| *from == c).map_or(c, |(_, to)| *to)
            }
        })
        .collect();
    swapped.trim().trim_matches('.').trim().to_string()
}

/// `text` cut to at most `bytes` bytes, at a character boundary — never leaving half a character
/// behind, the same guarantee yt-dlp's `B` conversion gives inside a template.
fn cut_to(text: &str, bytes: usize) -> &str {
    if text.len() <= bytes {
        return text;
    }
    let mut end = bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// What a downloaded generic file should be called: `<title> [<id>].<ext>`, with the title cut to
/// whatever `limit` leaves once the id and extension — both known exactly by now — are seated.
///
/// Nothing is reserved for yt-dlp's working suffixes, because they are gone: the download has
/// finished, so `.f<format_id>` and `.part-Frag<n>` no longer exist. Nor for the sanitizer, since
/// [`sanitize`] substitutes byte-for-byte. That is where the extra ~80 bytes of title come from
/// against a template handed to yt-dlp before any of those lengths were known.
pub(crate) fn final_name(prefix: &str, title: &str, id: &str, ext: &str, limit: usize) -> String {
    let id = cut_to(&sanitize(id), ID_BYTES).to_string();
    let ext = sanitize(ext);
    let kept =
        cut_to(&sanitize(title), title_room(prefix, &id, &ext, limit)).trim_end().to_string();
    // A title that sanitized away to nothing would leave a name opening with " [" — the id alone
    // reads better and is still unique.
    if kept.is_empty() {
        format!("{prefix}{id}.{ext}")
    } else {
        format!("{prefix}{kept} [{id}].{ext}")
    }
}

/// The ordering prefix a collected video's name carries, from a label the caller worked out — a
/// position, an episode number, or an `S6E01`-style index. Empty for a lone download.
///
/// The label is the caller's business because only it knows what the ordering *means*; this only
/// fixes the separator, which matches the `NNN__` shape a YouTube playlist entry gets.
pub(crate) fn position_prefix(label: Option<&str>) -> String {
    label.map(|label| format!("{label}__")).unwrap_or_default()
}

/// `text` as a single path component, made safe and cut to `bytes`.
///
/// For a folder this crate names *itself* — the `--multi` collection folder. A folder is a path
/// component and overruns exactly as a file name does. The YouTube collection folders are yt-dlp's
/// to name from a template and are not bounded (see the README's limitations); this one is ours,
/// so it is.
pub(crate) fn component(text: &str, bytes: usize) -> String {
    cut_to(&sanitize(text), bytes).trim_end().to_string()
}

/// Whether the caller named their own output template after `--`.
///
/// Theirs lands after ours and wins, so the file is exactly where they asked for it — renaming it
/// afterwards would quietly override the request. The generic path checks this before renaming.
pub(crate) fn caller_named_output(extra: &[String]) -> bool {
    extra.iter().any(|arg| arg == "-o" || arg == "--output" || arg.starts_with("--output="))
}

/// The bytes a title gets inside a finished generic name: whatever `limit` has left once ` [`, the
/// id, `].` and the extension are seated. Split out because the caller reports it — a notice that
/// says how much of a title survived has to be told by whoever cut it.
pub(crate) fn title_room(prefix: &str, id: &str, ext: &str, limit: usize) -> usize {
    limit.saturating_sub(
        prefix.len() + 2 + cut_to(&sanitize(id), ID_BYTES).len() + 2 + sanitize(ext).len(),
    )
}

/// The same argv with its `--output` title bound halved, for one retry after the filesystem
/// refused a name.
///
/// A refused name is the OS talking, so a plain retry is pointless — but a *shorter* name is a
/// different request, and a name only a little too long comes through on it. Rewriting the argv
/// we already have beats threading a template builder down to the diagnosis: the argv is the
/// thing that needs to change, and it carries the template it needs.
///
/// `None` when there is no bound to shrink — an `-o` the caller passed after `--`, or a budget
/// already at [`MIN_TITLE_BYTES`] — because then the retry would be the identical request.
pub(crate) fn halve_title_bound(argv: &[OsString]) -> Option<Vec<OsString>> {
    let at = argv.iter().position(|arg| arg == "--output")?;
    let template = argv.get(at + 1)?.to_str()?;
    let (bound, start, end) = title_bound(template)?;
    if bound <= MIN_TITLE_BYTES {
        return None;
    }
    let halved = (bound / 2).max(MIN_TITLE_BYTES);
    let rebuilt = format!("{}{}{}", &template[..start], title_field(halved), &template[end..]);
    let mut tightened = argv.to_vec();
    tightened[at + 1] = rebuilt.into();
    Some(tightened)
}

/// The title bound `template` carries, and the byte range of the field spelling it: the inverse of
/// [`title_field`]. `None` for a template with no such field, so a caller-supplied `-o` is left
/// alone rather than rewritten by pattern-matching luck.
fn title_bound(template: &str) -> Option<(usize, usize, usize)> {
    const OPEN: &str = "%(title).";
    let start = template.find(OPEN)?;
    let digits_at = start + OPEN.len();
    let digits: String =
        template[digits_at..].chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || !template[digits_at + digits.len()..].starts_with('B') {
        return None;
    }
    Some((digits.parse().ok()?, start, digits_at + digits.len() + 1))
}

/// Whether the output templates will cut either field of this download's name — the same
/// comparison yt-dlp's `B` conversion makes, on bytes, so this answers "was it cut" without having
/// to ask yt-dlp what it did.
///
/// `title_bytes` is the budget of whichever template ran — they differ, so the caller says which.
///
/// The title is all this asks about, because every template that reaches here leaves the id whole:
/// [`yt_name`] interpolates a bare `%(id)s` so `find_by_id` can match it. Bounding an id is the
/// rename path's business, and [`rename_downloaded`] checks that for itself.
pub(crate) fn is_shortened(title: &str, title_bytes: usize) -> bool {
    title.len() > title_bytes
}

/// What to say about a name that had to be cut: which field overran, and by how much.
///
/// Both fields are named when both overran, because they are cut for unrelated reasons — a caption
/// used as a title, and a URL-derived id — and someone chasing a file on disk needs to know which
/// part of its name stopped being the whole truth.
fn shortened_notice(id: &str, title: &str, title_bytes: usize) -> String {
    let mut cut = Vec::new();
    if title.len() > title_bytes {
        cut.push(format!(
            "its title is {} bytes, so the name keeps the first {title_bytes}",
            title.len()
        ));
    }
    if id.len() > ID_BYTES {
        cut.push(format!("its id is {} bytes, so the name keeps the first {ID_BYTES}", id.len()));
    }
    format!("{id}: file name shortened — {}", cut.join("; "))
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
pub(crate) fn note_shortened(dir: &Path, title_bytes: usize) {
    let Some(records) = take_scratch(dir) else { return };
    let cut = records
        .into_iter()
        .filter(|record| is_shortened(&record.title, title_bytes))
        .map(|record| (record.id, record.title, title_bytes))
        .collect();
    announce_and_record(dir, cut);
}

/// Consume the scratch and say nothing — for a caller who named their own `-o` after `--`.
///
/// Their template is theirs, and this crate has no idea what it kept. Reporting against a budget
/// that was never in force would announce shortenings that never happened; leaving the scratch
/// behind would strand a bookkeeping file beside their media. So: read it, drop it, be quiet.
pub(crate) fn discard_names(dir: &Path) {
    let _ = take_scratch(dir);
}

/// Announce every name that had to be cut — red, on stderr, as the run goes — and append the
/// titles in full to [`MODIFIED_LEDGER`].
///
/// Shared by both naming strategies: the bounded templates the YouTube paths hand yt-dlp, and the
/// rename the generic path does for itself. Each entry carries how many bytes of its title
/// survived, since the two strategies arrive at that number differently.
fn announce_and_record(dir: &Path, cut: Vec<(String, String, usize)>) {
    // One entry per video. A video normally names itself once, but a diagnostic retry can reach
    // the naming stage a second time, and the same title twice over reads as two shortened files.
    let mut seen = HashSet::new();
    let cut: Vec<(String, String, usize)> =
        cut.into_iter().filter(|(id, _, _)| seen.insert(id.clone())).collect();
    if cut.is_empty() {
        return;
    }
    for (id, title, kept) in &cut {
        eprintln!(
            "{}",
            style::problematic(&format!("vidl: {}", shortened_notice(id, title, *kept)))
        );
    }
    write_modified(dir, &cut);
}

/// Append `cut` to [`MODIFIED_LEDGER`] and say where it went. Same shape as the failure ledger:
/// a dated block per run, appended, so a re-run adds to the history instead of replacing it.
fn write_modified(dir: &Path, cut: &[(String, String, usize)]) {
    let path = dir.join(MODIFIED_LEDGER);
    let mut block = format!("── {} ──\n", stamp::datehour_stamp());
    for (key, title, _) in cut {
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

/// One download's own account of itself, as [`RECORD_FORMAT`] wrote it.
#[derive(Debug, PartialEq)]
struct Recorded {
    id: String,
    /// Where the file actually landed. Empty when yt-dlp reported no path.
    path: String,
    /// The title in full, before any bound touched it.
    title: String,
}

/// Parse the scratch into one [`Recorded`] per download.
///
/// The title is printed last and takes the rest of the line, then the rest of the *lines*: yt-dlp
/// writes it verbatim, so a caption with hard line breaks arrives as several. A line with no tab
/// is therefore the continuation of the title above it and not a new entry — treating it as one
/// would split a long title into fragments that each look short enough to leave alone.
fn parse_names(text: &str) -> Vec<Recorded> {
    let mut records: Vec<Recorded> = Vec::new();
    for line in text.lines() {
        let mut fields = line.splitn(3, '\t');
        match (fields.next(), fields.next(), fields.next()) {
            (Some(id), Some(path), Some(title)) => records.push(Recorded {
                id: id.trim().to_string(),
                path: path.trim().to_string(),
                title: title.to_string(),
            }),
            _ => {
                if let Some(last) = records.last_mut() {
                    last.title.push(' ');
                    last.title.push_str(line);
                }
            }
        }
    }
    records
}

/// Read the scratch and remove it, so a second call is a no-op and a name recorded by a diagnostic
/// retry is picked up by the next pass rather than counted twice.
fn take_scratch(dir: &Path) -> Option<Vec<Recorded>> {
    let scratch = dir.join(NAMES_SCRATCH);
    let text = std::fs::read_to_string(&scratch).ok()?;
    let _ = std::fs::remove_file(&scratch);
    Some(parse_names(&text))
}

/// Put the real name on each file the generic path downloaded under a temporary one, and record
/// any title that still didn't fit.
///
/// This is the whole reason that path downloads to `<id>.<ext>` first: by now the id, the title and
/// the extension are all known exactly and yt-dlp's working suffixes are gone, so the title gets
/// everything `limit` has left instead of what a template had to guess it might need.
///
/// A file that can't be renamed keeps its temporary name. That is a poor name, not a lost
/// download — the graceful degradation the bounded-template approach could not offer.
pub(crate) fn rename_downloaded(dir: &Path, limit: usize, label: Option<&str>) {
    let Some(records) = take_scratch(dir) else { return };
    let prefix = position_prefix(label);
    let mut cut = Vec::new();
    for record in records {
        let from = Path::new(&record.path);
        let Some(ext) = from.extension().and_then(|ext| ext.to_str()) else { continue };
        let wanted = final_name(&prefix, &record.title, &record.id, ext, limit);
        if from.file_name().is_some_and(|name| name == wanted.as_str()) {
            continue; // already right — nothing was temporary about it
        }
        if !rename_beside(from, &wanted) {
            eprintln!(
                "{}",
                style::problematic(&format!(
                    "vidl: could not rename {} — it keeps its id as its name",
                    from.display()
                ))
            );
            continue;
        }
        // Whether the name kept everything. Comparing against the *sanitized* title is the honest
        // test: a raw byte length would count characters the name never wanted and report a
        // shortening that only ever removed a slash. The id is checked too — this path is the one
        // that bounds it, and a URL-derived id cut down to fit would otherwise vanish without the
        // record that keys itself by the whole one.
        let whole = sanitize(&record.title);
        if !wanted.contains(whole.trim_end()) || record.id.len() > ID_BYTES {
            let room = title_room(&prefix, &record.id, ext, limit);
            cut.push((record.id, record.title, room));
        }
    }
    announce_and_record(dir, cut);
}

/// Rename `file` to `name` in its own directory, taking everything that shares its stem with it.
///
/// The set matters: on the audio path a `.vtt` sidecar is deliberately kept beside the media for
/// the tag pass and is found *by stem*, so moving the media file alone would orphan it. The `.`
/// check is what keeps a stem of `123` from dragging an unrelated `1234.mp4` along.
fn rename_beside(file: &Path, name: &str) -> bool {
    let (Some(dir), Some(old)) = (file.parent(), file.file_stem().and_then(|s| s.to_str())) else {
        return false;
    };
    let new = Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    // Collected before any move: renaming while still reading the directory races with itself.
    let moves: Vec<(std::path::PathBuf, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let rest = path.file_name()?.to_str()?.strip_prefix(old)?.to_string();
            rest.starts_with('.').then(|| (path, dir.join(format!("{new}{rest}"))))
        })
        .collect();
    !moves.is_empty() && moves.iter().all(|(from, to)| std::fs::rename(from, to).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{scratch_dir, ytdlp_or_skip};

    /// The budget the template strategy runs at, at the default limit — what the record tests
    /// measure against, since that strategy is the one that reports against a budget at all.
    const BUDGET: usize = yt_title_budget(DEFAULT_NAME_MAX);

    /// A template whose title bound is exactly `bound` — for exercising the halving directly,
    /// without tying the test to whatever the current limit happens to derive.
    fn bounded_template(bound: usize) -> String {
        format!("%(upload_date)s__{}__%(id)s.%(ext)s", title_field(bound))
    }

    /// An info dict for real yt-dlp to expand a template against, written under `dir`.
    ///
    /// Hand-rolled JSON: a handful of fields, and every title this suite passes is deliberately
    /// free of characters JSON would need escaped, so a serializer would be a dependency for
    /// nothing. `id` is mandatory — yt-dlp refuses an info dict without one outright, which is
    /// also why nothing downstream has to handle an absent id.
    fn info_json(dir: &Path, id: &str, title: &str) -> std::path::PathBuf {
        assert!(!title.contains(['"', '\\']), "this fixture writer does not escape JSON");
        let path = dir.join("info.json");
        std::fs::write(
            &path,
            format!(
                "{{\"_type\":\"video\",\"id\":\"{id}\",\"title\":\"{title}\",\"ext\":\"mp4\",\
                 \"upload_date\":\"20240101\",\
                 \"extractor\":\"generic\",\"extractor_key\":\"Generic\",\
                 \"webpage_url\":\"https://example.test/v/1\",\
                 \"formats\":[{{\"url\":\"https://example.test/a.mp4\",\"ext\":\"mp4\"}}]}}"
            ),
        )
        .unwrap();
        path
    }

    /// The file name **real yt-dlp** produces for `template` — expanded, truncated and sanitized
    /// exactly as a download would do it, with no download and no network.
    ///
    /// Raw bytes on purpose: what these tests measure is a byte length, and whether the result is
    /// still valid UTF-8 once truncation has cut into a multi-byte character. A lossy conversion
    /// would quietly repair the very thing under test.
    fn expanded(info: &Path, template: &str) -> Vec<u8> {
        let (program, args) = crate::ytdlp::ytdlp_invocation(vec![
            "--load-info-json".into(),
            info.as_os_str().to_owned(),
            "--print".into(),
            "filename".into(),
            "-o".into(),
            template.into(),
        ]);
        let out = std::process::Command::new(&program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run yt-dlp");
        assert!(
            out.status.success(),
            "yt-dlp could not expand {template}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // The template is relative, so the line yt-dlp prints IS the single path component.
        let mut name = out.stdout;
        while name.last().is_some_and(|byte| *byte == b'\n' || *byte == b'\r') {
            name.pop();
        }
        name
    }

    /// Every template built at `limit`, paired with the title budget it was built from — the whole
    /// set, so a test can assert a property of all of them and a new one cannot slip past.
    fn templates(limit: usize) -> Vec<(String, usize)> {
        vec![
            (yt_name(limit), yt_title_budget(limit)),
            (yt_collection_name(limit), yt_collection_title_budget(limit)),
        ]
    }

    #[test]
    fn each_template_spells_the_budget_it_was_built_from() {
        // The budgets are arithmetic and the templates are strings, so this is the seam where the
        // two could disagree. Reading the number back out of each template is what closes it.
        for (template, budget) in templates(DEFAULT_NAME_MAX) {
            let (spelled, _, _) =
                title_bound(&template).unwrap_or_else(|| panic!("{template} bounds no title"));
            assert_eq!(spelled, budget, "{template} disagrees with its own budget");
            assert!(!template.contains("%(title)s"), "{template} unbounds the title");
        }
        // The id is deliberately left whole here, because `find_by_id` matches the literal
        // `__<id>.` marker these names end with. Bounding an id is the rename path's business.
        assert!(yt_name(DEFAULT_NAME_MAX).contains("%(id)s"));
    }

    #[test]
    fn sanitizing_substitutes_byte_for_byte_so_a_measured_name_stays_measured() {
        // The reason this crate does its own sanitizing on the rename path: yt-dlp swaps in
        // fullwidth lookalikes, three bytes each, *after* a name was measured. Substituting
        // single-byte characters means a cut title's length is exactly what was counted, which is
        // what lets `final_name` reserve nothing at all for sanitizing.
        let dirty = "a/b\\c:d*e?f\"g<h>i|j";
        let clean = sanitize(dirty);
        assert_eq!(clean.len(), dirty.len(), "no widening: {clean}");
        for reserved in ['/', '\\', ':', '*', '?', '"', '<', '>', '|'] {
            assert!(!clean.contains(reserved), "{reserved} survived sanitizing: {clean}");
        }
        // A caption's hard line breaks become spaces — a name cannot hold them, and the record
        // keeps the original anyway.
        assert_eq!(sanitize("one\ntwo\r\tthree"), "one two  three");
        // Leading and trailing dots and spaces make a name hidden or awkward, so they go.
        assert_eq!(sanitize("  .hidden.  "), "hidden");
        assert!(sanitize("   ").is_empty(), "a title of whitespace sanitizes to nothing");
    }

    #[test]
    fn cutting_never_leaves_half_a_character_behind() {
        // The same guarantee yt-dlp's `B` conversion gives inside a template, since the rename
        // path has to make it for itself.
        let cjk = "日本語";
        assert_eq!(cut_to(cjk, 9), "日本語", "an exact fit is untouched");
        assert_eq!(cut_to(cjk, 8), "日本", "a byte short drops the whole character");
        assert_eq!(cut_to(cjk, 3), "日");
        assert_eq!(cut_to(cjk, 2), "", "no room for even one character");
        assert_eq!(cut_to("abc", 99), "abc", "a budget larger than the text is fine");
    }

    #[test]
    fn a_renamed_file_seats_far_more_title_than_a_template_could_have() {
        // The whole point of downloading to a temporary name. By rename time the id and extension
        // are known and yt-dlp's working suffixes are gone, so nothing is reserved against a guess
        // — the title gets everything the limit has left.
        let title = "T".repeat(400);
        let name = final_name("", &title, "1023581733834265", "mp4", DEFAULT_NAME_MAX);
        assert!(name.len() <= DEFAULT_NAME_MAX, "{} bytes overruns", name.len());
        assert!(name.ends_with(" [1023581733834265].mp4"), "{name}");
        let kept = name.len() - " [1023581733834265].mp4".len();
        assert!(
            kept > yt_title_budget(DEFAULT_NAME_MAX),
            "renaming must beat even the roomiest template ({}), kept {kept}",
            yt_title_budget(DEFAULT_NAME_MAX)
        );
        // And it fills the component exactly: the title took every byte that was left.
        assert_eq!(kept, title_room("", "1023581733834265", "mp4", DEFAULT_NAME_MAX));

        // A title that fits arrives whole, with no cutting and no trailing space before the id.
        let short = final_name("", "A fine title", "vid1", "mkv", DEFAULT_NAME_MAX);
        assert_eq!(short, "A fine title [vid1].mkv");
        // A title that sanitizes away to nothing would leave a name opening with " [".
        assert_eq!(final_name("", "   ", "vid1", "mkv", DEFAULT_NAME_MAX), "vid1.mkv");
        // An id long enough to matter is bounded here too — the URL-derived kind.
        let long_id = final_name("", "t", &"u".repeat(120), "mp4", DEFAULT_NAME_MAX);
        assert!(long_id.contains(&"u".repeat(ID_BYTES)), "{long_id}");
        assert!(!long_id.contains(&"u".repeat(ID_BYTES + 1)), "the id is bounded: {long_id}");

        // A collected video leads with its position on the page, and pays for it out of the title
        // — the prefix shares the component, so it cannot be free.
        let numbered =
            final_name(&position_prefix(Some("007")), &title, "vid1", "mkv", DEFAULT_NAME_MAX);
        assert!(numbered.starts_with("007__"), "zero-padded, sortable: {numbered}");
        assert!(numbered.len() <= DEFAULT_NAME_MAX);
        let plain = final_name("", &title, "vid1", "mkv", DEFAULT_NAME_MAX);
        assert_eq!(plain.len(), numbered.len(), "the prefix comes out of the title, not the limit");
        assert_eq!(position_prefix(None), "", "a lone download is not numbered");
        assert_eq!(position_prefix(Some("S6E01")), "S6E01__", "a label need not be a number");
    }

    #[test]
    fn renaming_records_a_cut_id_even_when_the_title_fitted() {
        // The rename path is the only one that bounds an id, so it is the only one that can lose
        // one. A URL-derived id cut down to fit would otherwise vanish with nothing written —
        // and the record is keyed by the WHOLE id, so writing an entry is what preserves it.
        let dir = scratch_dir("names_cut_id");
        let long_id = "u".repeat(ID_BYTES + 30);
        std::fs::write(dir.join(format!("{long_id}.mp4")), b"x").unwrap();
        std::fs::write(
            dir.join(NAMES_SCRATCH),
            format!("{long_id}\t{}\tA short title\n", dir.join(format!("{long_id}.mp4")).display()),
        )
        .unwrap();

        rename_downloaded(&dir, DEFAULT_NAME_MAX, None);
        let record = std::fs::read_to_string(dir.join(MODIFIED_LEDGER))
            .expect("a cut id is worth recording even when the title fitted");
        assert!(record.contains(&long_id), "keyed by the id in full: {record}");
        assert!(record.contains("A short title"), "and the title, which did fit: {record}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_caller_supplied_output_template_is_consumed_quietly() {
        // Their `-o` kept whatever it kept, and this crate has no idea what. Reporting against a
        // budget that was never in force would announce shortenings that never happened; leaving
        // the scratch would strand a bookkeeping file beside their media.
        let dir = scratch_dir("names_discard");
        std::fs::write(dir.join(NAMES_SCRATCH), format!("vid1\t/dl/x.mkv\t{}\n", "t".repeat(900)))
            .unwrap();
        discard_names(&dir);
        assert!(!dir.join(NAMES_SCRATCH).exists(), "the scratch is consumed");
        assert!(!dir.join(MODIFIED_LEDGER).exists(), "and nothing is claimed about their naming");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn renaming_takes_the_sidecars_with_it_and_leaves_strangers_alone() {
        let dir = scratch_dir("names_rename");
        // What a finished audio download leaves: the media file plus the `.vtt` sidecars kept for
        // the tag pass, which `media::subtitle_sidecars` finds BY STEM — so moving the media file
        // alone would orphan them.
        for name in ["vid1.opus", "vid1.en.vtt", "vid1.he-orig.vtt"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        // A stranger whose name merely starts with the same characters must not be dragged along.
        std::fs::write(dir.join("vid12345.opus"), b"x").unwrap();

        assert!(rename_beside(&dir.join("vid1.opus"), "A Title [vid1].opus"));
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "A Title [vid1].en.vtt",
                "A Title [vid1].he-orig.vtt",
                "A Title [vid1].opus",
                "vid12345.opus",
            ],
            "the whole stem moved, and only that stem"
        );
        // Nothing to move is a failure, not a silent success — the caller keeps the old name.
        assert!(!rename_beside(&dir.join("absent.mkv"), "Whatever [absent].mkv"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_caller_who_named_their_own_output_is_not_renamed_over() {
        // Their `-o` lands after ours and wins, so the file is where they asked for it. Renaming
        // it afterwards would quietly override the request.
        assert!(caller_named_output(&["-o".to_string(), "%(title)s.%(ext)s".to_string()]));
        assert!(caller_named_output(&["--output".to_string(), "x".to_string()]));
        assert!(caller_named_output(&["--output=x".to_string()]));
        assert!(!caller_named_output(&["--yes-playlist".to_string()]));
        assert!(!caller_named_output(&[]));
    }

    #[test]
    fn the_probe_returns_a_length_the_destination_actually_accepts() {
        // The property that holds on any filesystem, which is the point of measuring rather than
        // reporting: whatever comes back, a name of that length can really be created there.
        let dir = scratch_dir("names_probe");
        let limit = probe_limit(&dir);
        assert!(accepts(&dir, limit), "probed {limit} but a name that long won't write");
        assert!(limit >= MIN_TITLE_BYTES && limit <= DEFAULT_NAME_MAX, "{limit} out of range");
        // And it leaves nothing behind — the destination is the user's download directory.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "the probe must clean up");

        // A destination that cannot be written at all is not a length problem, so the probe
        // declines to reinterpret it as a tiny filesystem and leaves the default in place.
        let missing = dir.join("no/such/dir");
        assert_eq!(probe_limit(&missing), DEFAULT_NAME_MAX);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refused_name_is_retried_at_half_the_title_and_only_while_there_is_room() {
        let template = |bytes| format!("/dl/{}", bounded_template(bytes));
        let argv = |template: String| {
            vec![OsString::from("--output"), template.into(), "https://u".into()]
        };
        let bound_of = |argv: &[OsString]| {
            title_bound(argv[1].to_str().unwrap()).expect("still bounded").0
        };

        let first = argv(template(DEFAULT_NAME_MAX));
        let start = bound_of(&first);
        let retry = halve_title_bound(&first).expect("a generous bound can be halved");
        assert_eq!(bound_of(&retry), start / 2, "one halving, not a token trim");
        assert_eq!(retry[2], OsString::from("https://u"), "nothing but the template changes");
        assert_eq!(retry.len(), first.len(), "and no argument is added or lost");

        // Halving bottoms out: at the floor there is nothing left to give, so the retry is the
        // identical request and is declined rather than burning an invocation on it.
        let floored = argv(template(MIN_TITLE_BYTES * 2 + 1));
        let once = halve_title_bound(&floored).expect("one step still fits");
        assert_eq!(bound_of(&once), MIN_TITLE_BYTES, "clamped to the floor, not below it");
        assert!(halve_title_bound(&once).is_none(), "and no further halving is offered");

        // A template we did not write is left alone — an `-o` passed after `--` is the caller's,
        // and rewriting it by pattern-matching luck would be worse than not retrying.
        let theirs = argv("/dl/%(title)s-%(id)s.%(ext)s".to_string());
        assert!(halve_title_bound(&theirs).is_none(), "an unbounded caller template is untouched");
        assert!(halve_title_bound(&[OsString::from("--no-output-here")]).is_none());
    }

    #[test]
    fn a_collection_entry_pays_for_its_ordering_prefix_out_of_the_title() {
        // The index prefix shares the path component, so it cannot be free. A playlist entry's
        // budget must be exactly the lone-video budget less that prefix.
        let limit = DEFAULT_NAME_MAX;
        assert_eq!(yt_collection_title_budget(limit), yt_title_budget(limit) - COLLECTION_PREFIX);
        assert!(yt_collection_name(limit).starts_with("%(playlist_index)03d__"));
    }

    #[test]
    fn the_budget_is_derived_from_the_fields_youtube_actually_pins_down() {
        // A template can only be handed to yt-dlp because YouTube's fields are predictable — an
        // 11-byte id, three-character format ids. These are the numbers that derivation produces,
        // pinned so a change to any reserve reads as a deliberate edit and not a silent drift.
        assert_eq!(yt_title_budget(255), 187);
        assert_eq!(yt_collection_title_budget(255), 182, "less the ordering prefix");
        // A smaller limit comes straight off the title, byte for byte: that is what makes
        // `--name-max` mean something rather than merely be accepted.
        assert_eq!(yt_title_budget(255) - yt_title_budget(143), 255 - 143);
    }

    #[test]
    fn a_budget_plus_its_reserves_exactly_fills_the_limit_it_came_from() {
        // The point of deriving rather than hand-picking: whatever limit is in force, the title
        // budget plus every reserve must come to exactly that limit — no slack left unused, and
        // nothing spilling past it. Checked across a spread including `--name-max 143` and limits
        // too small to seat the reserves at all.
        for limit in [DEFAULT_NAME_MAX, 200, 143, 100, 40, 10] {
            for (shape, literals, id, format_id) in
                [("youtube", 8 + 2 + 2, YT_ID_BYTES, YT_FORMAT_ID)]
            {
                let budget = title_budget(limit, literals, id, format_id);
                let worst =
                    budget + literals + id + working_suffix(format_id, EXT_BYTES) + SANITIZER_RESERVE;
                if budget == 0 {
                    // A limit too small to seat the reserves floors at zero rather than wrapping.
                    // `run` refuses such a destination instead of naming files nothing.
                    assert!(limit < worst, "{shape} at {limit}: floored, so the reserves must not fit");
                } else {
                    assert_eq!(worst, limit, "{shape} at {limit}: {budget} + reserves must fill it");
                }
            }
        }
    }

    #[test]
    fn a_name_is_measured_in_bytes_not_characters() {
        // The whole bug in one assertion: at three bytes a character, half a budget's worth of CJK
        // is comfortably under it counted as characters and half again over it counted as bytes —
        // and bytes are what the filesystem counts. Derived from BUDGET rather than written out, so
        // it stays the boundary case whatever the budget derives to.
        let cjk = "日".repeat(BUDGET / 2);
        assert!(cjk.chars().count() < BUDGET, "under the budget in characters");
        assert!(cjk.len() > BUDGET, "over it in bytes: {} > {BUDGET}", cjk.len());
        assert!(is_shortened(&cjk, BUDGET), "a byte-length check is what catches this");

        // And the boundary is exact, on either side of it.
        assert!(!is_shortened(&"a".repeat(BUDGET), BUDGET), "exactly the budget still fits");
        assert!(is_shortened(&"a".repeat(BUDGET + 1), BUDGET), "one byte over does not");
        assert!(!is_shortened("", BUDGET), "no title, nothing to cut");
        // The id is not this function's business: every template that reaches it leaves the id
        // whole, so that `find_by_id` can match the `__<id>.` marker. Bounding one is the rename
        // path's job, and `rename_downloaded` checks it there.
        assert!(!yt_name(DEFAULT_NAME_MAX).contains("%(id)."), "the YouTube id stays whole");
    }

    #[test]
    fn a_name_cut_only_by_its_id_is_still_recorded_and_says_which_field_overran() {
        // A URL-derived id can overrun on its own — a CDN link with a long filename and a short
        // title. The name is no longer the whole truth either way, so it must still be recorded,
        // and the notice has to name the field that was cut rather than assume it was the title.
        let long_id = "u".repeat(ID_BYTES + 30);
        let id_only = shortened_notice(&long_id, "a short title", BUDGET);
        assert!(id_only.contains("its id is"), "names the id: {id_only}");
        assert!(!id_only.contains("its title is"), "and not the title, which fit: {id_only}");

        let title_only = shortened_notice("vid1", &"t".repeat(BUDGET + 1), BUDGET);
        assert!(title_only.contains("its title is"), "names the title: {title_only}");
        assert!(!title_only.contains("its id is"), "and not the id, which fit: {title_only}");

        // Both at once — the generic extractor usually derives both from the same URL, so this is
        // the common shape rather than the exotic one.
        let both = shortened_notice(&long_id, &"t".repeat(BUDGET + 1), BUDGET);
        assert!(both.contains("its title is") && both.contains("its id is"), "{both}");
        for notice in [&id_only, &title_only, &both] {
            assert!(!notice.contains('\n'), "one line per name: {notice}");
        }
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
        // continuation lines belong to the entry above them. The title is printed last precisely
        // so this works — it takes the rest of the line, then the rest of the lines.
        let parsed = parse_names("vid1\t/dl/vid1.mp4\tone\ntwo\nthree\nvid2\t/dl/vid2.mp4\tplain\n");
        assert_eq!(parsed.len(), 2, "two downloads, not four: {parsed:?}");
        assert_eq!(parsed[0].id, "vid1");
        assert_eq!(parsed[0].path, "/dl/vid1.mp4", "the path yt-dlp actually wrote to");
        assert_eq!(parsed[0].title, "one two three", "the continuations joined back on");
        assert_eq!(parsed[1].title, "plain");
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
        std::fs::write(&scratch, "short1\t/dl/a.mkv\tA fine title\nshort2\t/dl/b.mkv\tAnother\n")
            .unwrap();
        note_shortened(&dir, BUDGET);
        assert!(!ledger.exists(), "nothing was cut, so nothing is recorded");
        assert!(!scratch.exists(), "the scratch is consumed either way");

        // A cut title is recorded in full, keyed by the id, and the scratch is gone.
        let long = "x".repeat(BUDGET + 40);
        std::fs::write(&scratch, format!("vid1\t/dl/1.mkv\t{long}\nvid2\t/dl/2.mkv\tfits fine\n"))
            .unwrap();
        note_shortened(&dir, BUDGET);
        let text = std::fs::read_to_string(&ledger).expect("the record exists now");
        assert!(text.contains(&format!("vid1\t{long}")), "the full title, keyed by id: {text}");
        assert!(!text.contains("vid2"), "a title that fit must not be recorded: {text}");
        assert!(!scratch.exists(), "the scratch is consumed");

        // A second run appends rather than replacing: the history of a re-run stays readable.
        let other = "y".repeat(BUDGET + 1);
        std::fs::write(&scratch, format!("vid3\t/dl/3.mkv\t{other}\n")).unwrap();
        note_shortened(&dir, BUDGET);
        let text = std::fs::read_to_string(&ledger).unwrap();
        assert!(text.contains("vid1\t") && text.contains("vid3\t"), "both runs are kept: {text}");
        assert_eq!(text.matches("── ").count(), 2, "one dated block per run: {text}");

        // No scratch is the normal case (a run in which yt-dlp wrote nothing) and must be silent.
        note_shortened(&dir, BUDGET);
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
            title_bound(RECORD_FORMAT).is_none(),
            "the record must keep the title the file name could not"
        );
        assert_eq!(flags[2], format!("/dl/{NAMES_SCRATCH}"));
    }

    // --- the templates, expanded by the real yt-dlp (offline; skip-with-notice without one) -----
    //
    // The stubbed flows prove the wiring — that a bounded template reaches yt-dlp, and that a
    // shortened name is announced and recorded. What they cannot prove is the half that lives
    // *inside* yt-dlp: that `B` truncates by bytes, at a character boundary, and that what comes
    // out fits. `--load-info-json` asks the real thing, with no download and no network.

    #[test]
    fn real_ytdlp_leaves_room_for_its_own_working_suffixes() {
        let test = "real_ytdlp_leaves_room_for_its_own_working_suffixes";
        if !ytdlp_or_skip(test) {
            return;
        }
        let dir = scratch_dir("names_worst_tmpl");
        // The budget's whole job: a title far past it must come back inside the limit with enough
        // left over for the names yt-dlp uses *while* downloading — the per-format tag and the
        // fragment index, which no finished name shows but every download passes through.
        let info = info_json(&dir, "abcdefghijk", &"T".repeat(400));
        let name = expanded(&info, &yt_name(DEFAULT_NAME_MAX));
        let working = ".f616".len() + ".part-Frag1234".len();
        assert!(
            name.len() + working <= DEFAULT_NAME_MAX,
            "{} bytes, plus {working} of yt-dlp's working suffixes, overruns the limit",
            name.len()
        );
        // And the bound actually bit, at exactly the budget it was built from.
        let text = String::from_utf8(name).expect("a bounded name is still valid UTF-8");
        assert!(text.contains(&"T".repeat(BUDGET)), "the title should fill its budget: {text}");
        assert!(!text.contains(&"T".repeat(BUDGET + 1)), "and not exceed it: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_ytdlps_byte_bound_cuts_a_multibyte_title_at_a_character_boundary() {
        let test = "real_ytdlps_byte_bound_cuts_a_multibyte_title_at_a_character_boundary";
        if !ytdlp_or_skip(test) {
            return;
        }
        let dir = scratch_dir("names_multibyte_tmpl");
        // 100 CJK characters: comfortably inside any character limit, 150 bytes past a byte one.
        // This is the case a `.150s` (character) truncation would sail straight past, and the
        // whole reason the templates use `B`.
        let info = info_json(&dir, "abcdefghijk", &"日".repeat(100));
        let name = expanded(&info, &yt_name(DEFAULT_NAME_MAX));
        let bytes = name.len();
        let text = String::from_utf8(name).expect("truncation must not leave half a character");
        let kept = text.chars().filter(|c| *c == '日').count();
        assert!(kept < 100, "the title was not cut at all — {kept} characters survived");
        assert!(kept * 3 <= BUDGET, "{kept} characters is {} bytes, over budget", kept * 3);
        assert!(bytes <= DEFAULT_NAME_MAX, "{bytes} bytes overruns the limit ({DEFAULT_NAME_MAX})");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_title_leaves_the_youtube_names_id_marker_intact() {
        let test = "a_truncated_title_leaves_the_youtube_names_id_marker_intact";
        if !ytdlp_or_skip(test) {
            return;
        }
        let dir = scratch_dir("names_yt_tmpl");
        // `find_by_id` locates a downloaded file by the literal `__<id>.` this template ends with,
        // and the thumbnail and subtitle patch passes are built on it. The title bound sits
        // upstream of that marker — but "upstream" is a claim about yt-dlp's expansion order, so
        // it gets checked rather than assumed.
        let info = info_json(&dir, "abcdefghijk", &"T".repeat(400));
        let name = expanded(&info, &yt_name(DEFAULT_NAME_MAX));
        let bytes = name.len();
        let text = String::from_utf8(name).expect("a bounded name is still valid UTF-8");
        assert!(text.contains("__abcdefghijk."), "find_by_id's marker must survive: {text}");
        assert!(text.starts_with("20240101__"), "the sortable date still leads: {text}");
        assert!(bytes <= DEFAULT_NAME_MAX, "{bytes} bytes overruns the limit ({DEFAULT_NAME_MAX})");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
