//! Enumerate a playlist or channel tab (yt-dlp's flat scan), read the download archive, and
//! report the entries nobody can play anymore (tombstoned titles).

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::Path;

use crate::util::exec::{capture_output, capture_stdout};
use super::{Env, seeded, ytdlp_invocation};
use crate::util::stamp;

/// One playlist entry as the flat scan sees it.
pub(crate) struct ScanEntry {
    pub(crate) index: String,
    pub(crate) id: String,
    pub(crate) title: String,
}

/// A scanned playlist: its title, its folder name (as yt-dlp will spell it), and every entry,
/// dead or alive.
pub(crate) struct PlaylistScan {
    pub(crate) title: String,
    pub(crate) dirname: Option<String>,
    pub(crate) entries: Vec<ScanEntry>,
}

/// Per-entry print format of the flat scan (playlist title rides along on every line).
const SCAN_FORMAT: &str = "%(playlist_index)s\t%(id)s\t%(title)s\t%(playlist_title)s";

/// A channel tab's scan outcome: tabs a channel simply doesn't have are normal, not errors.
pub(crate) enum TabScan {
    Found(PlaylistScan),
    Missing,
    Failed,
}

/// Scan a channel tab with stderr captured, so "this channel has no such tab" — yt-dlp's error,
/// but an everyday reality — turns into a calm message instead of an ERROR dump; anything else
/// on stderr is passed through as the real failure it is.
pub(crate) fn scan_tab(url: &str, dir_template: &str, env: Env) -> TabScan {
    let mut args = seeded(env);
    args.extend([
        OsString::from("--flat-playlist"),
        "--print".into(), SCAN_FORMAT.into(),
        "--print".into(), format!("playlist:{dir_template}").into(),
        url.into(),
    ]);
    let (program, args) = ytdlp_invocation(args);
    let Some((ok, stdout, stderr)) = capture_output(program, args) else {
        return TabScan::Failed;
    };
    if ok {
        if let Some(scan) = parse_scan(&stdout) {
            return TabScan::Found(scan);
        }
        return TabScan::Missing; // reachable but empty: nothing to download either way
    }
    if tab_absence(&stderr) {
        return TabScan::Missing;
    }
    eprint!("{stderr}");
    TabScan::Failed
}

/// Whether yt-dlp's stderr says the tab doesn't exist (its stable phrasing:
/// `ERROR: [youtube:tab] @handle: This channel does not have a streams tab`).
fn tab_absence(stderr: &str) -> bool {
    stderr.contains("does not have a")
}

/// One flat invocation lists the entries AND prints the collection's folder name (playlist
/// scope, sanitized by the `S` conversion in `dir_template` — a channel tab's flat entries
/// often lack `uploader`/`channel_id`, the `NA[NA]` trap, while the tab's own fields don't).
pub(crate) fn scan_playlist(url: &str, dir_template: &str, env: Env) -> Option<PlaylistScan> {
    let mut argv = seeded(env);
    argv.extend([
        OsString::from("--flat-playlist"),
        "--print".into(), SCAN_FORMAT.into(),
        "--print".into(), format!("playlist:{dir_template}").into(),
        url.into(),
    ]);
    let (program, args) = ytdlp_invocation(argv);
    let out = capture_stdout(program, args)?;
    parse_scan(&out)
}

/// Parse the flat scan's tab-separated lines (malformed ones are skipped; `None` only when
/// nothing parsed at all).
fn parse_scan(out: &str) -> Option<PlaylistScan> {
    let mut title = String::new();
    let mut dirname = None;
    let mut entries = Vec::new();
    for line in out.lines() {
        if !line.contains('\t') {
            // The playlist-scope print: a lone tabless line (it arrives after the entries;
            // the last one wins). An `NA` in it means the fields weren't there — no folder.
            let line = line.trim();
            if !line.is_empty() && !line.contains("NA") {
                dirname = Some(line.to_string());
            }
            continue;
        }
        let mut fields = line.split('\t');
        let (Some(index), Some(id), Some(entry_title)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if let Some(playlist_title) = fields.next() {
            title = playlist_title.to_string();
        }
        entries.push(ScanEntry {
            index: index.to_string(),
            id: id.to_string(),
            title: entry_title.to_string(),
        });
    }
    (!entries.is_empty()).then_some(PlaylistScan { title, dirname, entries })
}

/// The video ids a download-archive has recorded (its lines are `<extractor> <id>` — the
/// extractor prefix is yt-dlp's own on-disk format, kept so it can read the file back).
pub(crate) fn archived_ids(path: &Path) -> HashSet<String> {
    std::fs::read_to_string(path)
        .map(|content| {
            content
                .lines()
                .filter_map(|line| line.split_whitespace().nth(1).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The titles YouTube substitutes once an entry can't be played by anyone anymore.
const TOMBSTONES: &[&str] = &["[Private video]", "[Deleted video]", "[Unavailable video]"];

pub(crate) fn is_tombstone(title: &str) -> bool {
    TOMBSTONES.contains(&title)
}

/// The unplayable-entries report: everything a future search needs to trace what each entry
/// was — its id (the strongest key), its position, and lookup links for the WaybackMachine
/// and filmot (which indexes titles of deleted videos).
pub(crate) fn unplayable_report(playlist_title: &str, playlist_id: &str, dead: &[&ScanEntry]) -> String {
    let mut report = format!(
        "Unplayable entries of playlist: {playlist_title} [{playlist_id}]\n\
         recorded {} — video ids stay valid keys for archive services even after deletion\n\n",
        stamp::datehour_stamp()
    );
    for entry in dead {
        report += &format!(
            "#{index}  {title}\n  \
             was:      https://www.youtube.com/watch?v={id}\n  \
             wayback:  https://web.archive.org/web/*/https://www.youtube.com/watch?v={id}\n  \
             filmot:   https://filmot.com/video/{id}\n\n",
            index = entry.index,
            title = entry.title,
            id = entry.id,
        );
    }
    report
}


#[cfg(test)]
mod tests {
    use super::*;
    // The one cross-family constant this suite needs: the archive filename lives with argv assembly.
    use crate::ytdlp::ARCHIVE_NAME;
    

    #[test]
    fn the_flat_scan_parses_spots_tombstones_and_takes_the_folder_line() {
        let out = "1\tabc123def45\tA fine video\tMy Playlist\n\
                   2\tdead0000001\t[Private video]\tMy Playlist\n\
                   3\tdead0000002\t[Deleted video]\tMy Playlist\n\
                   My Playlist[PLxyz]\n";
        let scan = parse_scan(out).expect("parses");
        assert_eq!(scan.title, "My Playlist");
        assert_eq!(scan.entries.len(), 3);
        assert_eq!(scan.dirname.as_deref(), Some("My Playlist[PLxyz]"));
        let dead: Vec<&ScanEntry> =
            scan.entries.iter().filter(|e| is_tombstone(&e.title)).collect();
        assert_eq!(dead.len(), 2);
        // A folder line carrying NA means the fields weren't there — no folder is better
        // than an `NA[NA]` one.
        let na = parse_scan("1\tid234567890\tT\tP\nNA[NA]\n").expect("parses");
        assert_eq!(na.dirname, None);
        assert!(parse_scan("").is_none(), "an empty scan is a failed scan");
    }

    #[test]
    fn the_unplayable_report_traces_each_entry_by_id() {
        let dead = [ScanEntry {
            index: "2".into(),
            id: "dead0000001".into(),
            title: "[Private video]".into(),
        }];
        let refs: Vec<&ScanEntry> = dead.iter().collect();
        let report = unplayable_report("My Playlist", "PLxyz", &refs);
        assert!(report.contains("My Playlist [PLxyz]"));
        assert!(report.contains("#2  [Private video]"));
        assert!(report.contains("https://www.youtube.com/watch?v=dead0000001"));
        assert!(report
            .contains("https://web.archive.org/web/*/https://www.youtube.com/watch?v=dead0000001"));
        assert!(report.contains("https://filmot.com/video/dead0000001"));
    }

    #[test]
    fn a_missing_tab_is_recognized_by_ytdlps_phrasing() {
        assert!(tab_absence("ERROR: [youtube:tab] @x: This channel does not have a streams tab"));
        assert!(!tab_absence("ERROR: [youtube:tab] @x: Unable to download webpage"));
    }
    #[test]
    fn archived_ids_read_the_second_column() {
        let dir = std::env::temp_dir().join(format!("vidl_arch_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join(ARCHIVE_NAME);
        std::fs::write(&file, "youtube abc123\nyoutube def456\nmalformed\n").unwrap();
        let ids = archived_ids(&file);
        assert!(ids.contains("abc123") && ids.contains("def456"));
        assert_eq!(ids.len(), 2);
        assert!(archived_ids(Path::new("/no/such/archive")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
