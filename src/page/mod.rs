//! What a page lists, when it lists more than one video.
//!
//! Some pages are a collection wearing a single URL: a season of episodes, all present in the
//! served HTML, with JavaScript swapping which one the player points at and the address bar never
//! changing. yt-dlp resolves such a page to whichever video the player was initialised with, and
//! the rest are simply lost — silently, which is the part worth fixing.
//!
//! This module is reached only through `--multi`, and everything it does lives here rather than in
//! the ordinary download paths. That is the point of the flag: a page that already works cannot be
//! disturbed by code that never runs on it.
//!
//! # How a collection is recognised
//!
//! Without a table of sites, and by two tests rather than one — both derived from the video that
//! *did* resolve, never looked up per site:
//!
//! 1. **Shape.** A sibling's id looks like the resolved one's, as [`platforms`] judges it.
//! 2. **Key.** A sibling is written under the same key the resolved id is (`entryId`, `entry_id`).
//!
//! Two or more survivors, and the page is a collection. The second test is not decoration: shape
//! alone collected a widget id off a player URL — same shape exactly — and the download failed with
//! `Invalid entry id`. On a page whose entries all share one key it costs nothing.
//!
//! One more detail does as much work as either test: scanning **only the page**, not everything
//! yt-dlp fetched. The platform's own API replies repeat ids the page never lists, and reading them
//! too inflated the count from 18 to 23 (measured). [`page_dump`] is what keeps them out.
//!
//! # What it cannot derive
//!
//! Turning a bare id into a URL. See [`platforms`] — that part is a table, of embed platforms
//! rather than of sites, and it is the honest cost of the feature.
//!
//! # Off YouTube only
//!
//! YouTube publishes its collections as playlists and channels, and this crate walks those
//! properly already. `--multi` is refused there rather than being made to work twice.

pub(crate) mod naming;
pub(crate) mod platforms;

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::Env;
use crate::util::exec::capture_stdout_in;
use crate::util::style;
use crate::ytdlp::failures::{GeoRescue, diagnose_failure, write_ledger};
use crate::ytdlp::{generic_argv, launch, names, seeded, ytdlp_invocation};

/// How many sibling videos make a page a collection. Two: one is the ordinary case, and the whole
/// promise of `--multi` is that it leaves that case exactly as it was.
const COLLECTION_MINIMUM: usize = 2;

/// Bytes allowed for a collection folder's name. Bounded because this crate chooses this folder
/// itself, unlike the YouTube collection folders that yt-dlp names from a template (see the
/// README's limitations) — a folder is a path component and overruns the same way a file does.
const DIR_BYTES: usize = 120;

/// What one page turned out to be.
struct Scan {
    /// The id yt-dlp resolved the page to — the one video `--multi` would otherwise have taken.
    resolved: String,
    /// The page's own text, as yt-dlp fetched it.
    page: String,
}

/// Download every video `url`'s page lists, into a folder of its own.
///
/// `None` means "not a collection I can do anything with", and the caller carries on with the
/// ordinary single-video download — so `--multi` on an ordinary page costs one scan and changes
/// nothing else about the result.
pub(crate) fn download_collection(url: &str, into: &Path, env: Env) -> Option<i32> {
    let scan = scan_page(url, env)?;
    let Some(platform) = platforms::claiming(&scan.resolved) else {
        eprintln!(
            "vidl: --multi can find siblings on {} pages — this page's video ({}) isn't one of \
             those, so it downloads on its own",
            platforms::names().join("/"),
            scan.resolved
        );
        return None;
    };
    let siblings = siblings(&scan.page, platform, &scan.resolved);
    if siblings.len() < COLLECTION_MINIMUM {
        println!(
            "this page lists {} {} video — nothing for --multi to collect",
            siblings.len(),
            platform.name
        );
        return None;
    }
    let home =
        into.join(names::component(&collection_dir(url, &scan.page, &siblings), DIR_BYTES));
    if let Err(err) = std::fs::create_dir_all(&home) {
        eprintln!("vidl: cannot create {}: {err}", home.display());
        return Some(1);
    }
    println!(
        "this page lists {} {} videos — collecting them into {}",
        siblings.len(),
        platform.name,
        home.display()
    );
    // The collection's own numbering, when it has one. Page order is only the fallback.
    let numbering = naming::sequence(&scan.page, &siblings);
    match &numbering {
        Some(found) => println!("numbering them by {}", found.source),
        None => println!("their names carry no sequence — numbering them in page order"),
    }
    let labels = numbering.as_ref().map(|found| found.prefixes.as_slice());
    Some(download_each(&siblings, platform, &scan.page, &home, env, labels))
}

/// Download each sibling in the order the page lists them, numbered by `numbering` where the
/// collection has its own, and by page position where it doesn't.
///
/// The number has to be applied here: each sibling is its own yt-dlp invocation, so there is no
/// playlist for yt-dlp to index them by — and the platform gives nothing to sort on either (every
/// entry on the page that prompted this reported the same upload date and no episode number).
/// Page order alone is a poor substitute, since a page may list newest-first and repeat its
/// featured video at the top; see [`naming::sequence`].
fn download_each(
    siblings: &[String],
    platform: &platforms::Platform,
    page: &str,
    home: &Path,
    env: Env,
    numbering: Option<&[String]>,
) -> i32 {
    let limit = names::limit();
    let (mut worst, mut done) = (0, 0usize);
    let mut unreachable = Vec::new();
    // Page order is the fallback, padded to the width the set needs so it sorts as text too.
    let width = naming::digits(siblings.len());
    for (index, id) in siblings.iter().enumerate() {
        // The video's own index where the collection has one, else its place on the page.
        let label = numbering
            .and_then(|labels| labels.get(index).cloned())
            .unwrap_or_else(|| format!("{:0width$}", index + 1));
        let Some(target) = (platform.address)(id, page) else {
            unreachable.push(id.clone());
            continue;
        };
        println!("=== {}/{} {id} ===", index + 1, siblings.len());
        let mut code = launch(generic_argv(&target, home, env));
        if code != 0 {
            match diagnose_failure(generic_argv(&target, home, env), &target, GeoRescue::XffSweep) {
                None => code = 0, // the plain retry or a spoofed region came through
                Some(line) => write_ledger(home, &[line]),
            }
        }
        if code == 0 {
            done += 1;
        } else if worst == 0 {
            worst = code;
        }
        // Per sibling, because the position belongs to this one file and the next download would
        // overwrite the record it was named from.
        names::rename_downloaded(home, limit, Some(&label));
    }
    if !unreachable.is_empty() {
        eprintln!(
            "{}",
            style::problematic(&format!(
                "vidl: {} of {} videos could not be addressed — the page carries no {} account id",
                unreachable.len(),
                siblings.len(),
                platform.name
            ))
        );
    }
    println!("{}", style::approved(&format!("{done} of {} collected", siblings.len())));
    worst
}

/// Ask yt-dlp what the page resolves to, and keep the page it fetched to do so.
///
/// One metadata invocation, which `--multi` is the licence for. yt-dlp does the fetching rather
/// than this crate because the page may need everything yt-dlp brings to a request — cookies, a
/// forced address family, a spoofed region. The page that prompted this is edge-blocked without
/// them.
fn scan_page(url: &str, env: Env) -> Option<Scan> {
    let scratch = std::env::temp_dir().join(format!("vidl_page_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).ok()?;
    let mut argv = seeded(env);
    argv.extend([OsString::from("--write-pages"), "--print".into(), "%(id)s".into()]);
    // The caller's passthrough flags too, which the other probes in this crate deliberately do
    // without. This one has to *reach the page* the download will reach, and some pages need a
    // lever to be fetched at all — the one this was built against is edge-blocked without a
    // spoofed region. A download-only flag in here is harmless: the worst it does is fail the
    // scan, and a failed scan falls back to the ordinary single download.
    argv.extend(env.extra.iter().map(OsString::from));
    argv.push(url.into());
    let (program, args) = ytdlp_invocation(argv);
    // `--write-pages` writes beside the working directory, so the run gets one of its own instead
    // of littering wherever `vidl` was started from.
    let printed = capture_stdout_in(&scratch, program, args);
    let resolved = printed.and_then(|text| {
        text.lines()
            .map(str::trim)
            .rev()
            .find(|line| !line.is_empty() && *line != "NA")
            .map(str::to_string)
    });
    let page = page_dump(&scratch, url);
    let _ = std::fs::remove_dir_all(&scratch);
    Some(Scan { resolved: resolved?, page: page? })
}

/// The page itself, out of everything `--write-pages` left behind.
///
/// It writes every response it fetched, the platform's API replies included, and those repeat ids
/// the page doesn't list — reading them all inflates the count (measured: 23 against a true 18).
///
/// Picked by **host**, because yt-dlp names each dump after the URL it came from and the page's
/// carries the site's host where the platform's replies carry the platform's. Not by size: that
/// looked reliable on one site and was wrong on the next, where a 5 KB API reply sat beside a
/// 1.4 KB page. Size only breaks ties among the site's own responses.
fn page_dump(dir: &Path, url: &str) -> Option<String> {
    let host = host_of(url);
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let named_for_the_site =
            entry.file_name().to_string_lossy().contains(host.as_str()) && !host.is_empty();
        if !named_for_the_site {
            continue;
        }
        let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
        if best.as_ref().is_none_or(|(largest, _)| size > *largest) {
            best = Some((size, entry.path()));
        }
    }
    std::fs::read_to_string(best?.1).ok()
}

/// The host part of `url`, as it appears inside a `--write-pages` dump's file name.
///
/// The **port is dropped**, because yt-dlp rewrites its colon to an underscore when it builds the
/// name — so a URL with an explicit port would never match itself. The hostname itself is left
/// exactly as the URL spelled it, case included, since that part is preserved verbatim.
///
/// Deliberately not `crate::host_of`, which lowercases: that one decides whose *site* a URL is,
/// where this one has to match a substring of a file name someone else wrote.
fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    host.split(':').next().unwrap_or_default().to_string()
}

/// The ids on `page` that are siblings of `resolved`, in the order the page lists them.
///
/// Two tests, not one. Sharing the resolved video's **id shape** is the first, and on a tidy page it
/// is enough. It is not always: a page was found whose player URL carries a widget id of exactly
/// the same shape as an entry id, so a shape-only sweep collected it and the download failed with
/// `Invalid entry id`. The second test is that a sibling is written under the same **key** as the
/// resolved video — `entry_id=` there, not `wid=` — which separated those cleanly and cost nothing
/// on a page where every entry already shared one key.
///
/// A page that never spells out a key for the resolved video falls back to shape alone: a sweep
/// that finds too much is recoverable, one that finds nothing is the feature not working.
///
/// First-appearance order, deduplicated: the featured video is usually named twice over, in the
/// player's configuration as well as in the list, and must not be collected twice.
fn siblings(page: &str, platform: &platforms::Platform, resolved: &str) -> Vec<String> {
    let candidates: Vec<(usize, &str)> =
        id_runs(page).into_iter().filter(|(_, run)| (platform.claims)(run)).collect();
    let anchors: HashSet<&str> = candidates
        .iter()
        .filter(|(_, run)| *run == resolved)
        .filter_map(|(at, _)| key_before(page, *at))
        .collect();
    let mut seen = HashSet::new();
    candidates
        .iter()
        .filter(|(at, _)| {
            anchors.is_empty()
                || key_before(page, *at).is_some_and(|key| anchors.contains(key))
        })
        .filter(|(_, run)| seen.insert(*run))
        .map(|(_, run)| run.to_string())
        .collect()
}

/// The key an id at `offset` is written under: the identifier just before it, past whatever
/// punctuation separates the two. Covers the forms seen in the wild — `"entryId":"…`, `entry_id=…`,
/// `&wid=…`. `None` when nothing identifier-shaped precedes it, as for an id sitting bare in an
/// array.
fn key_before(page: &str, offset: usize) -> Option<&str> {
    let before = page.get(..offset)?;
    let separators = |c: char| matches!(c, '"' | '\'' | ':' | '=' | ' ' | '\t' | '\n' | '\r');
    let end = before.trim_end_matches(separators).len();
    let start = before[..end]
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
        .last()
        .map(|(at, _)| at)?;
    Some(&before[start..end])
}

/// Every run of characters an id could be made of, with where it starts.
///
/// Splitting on everything else is what separates an id from the JSON quoting and punctuation
/// around it, and keeping letters in the run is what stops `entryId` being broken into pieces that
/// might themselves pass for one. The offsets are what [`key_before`] needs. Byte-wise on purpose:
/// an id is ASCII, so every boundary found this way is a character boundary too.
fn id_runs(text: &str) -> Vec<(usize, &str)> {
    let mut runs = Vec::new();
    let mut start = None;
    for (at, byte) in text.bytes().enumerate() {
        match (byte.is_ascii_alphanumeric() || byte == b'_', start) {
            (true, None) => start = Some(at),
            (false, Some(from)) => {
                runs.push((from, &text[from..at]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        runs.push((from, &text[from..]));
    }
    runs
}

/// The folder a collection lands in: `Something[page-id]`, mirroring the shape
/// `download_playlist` gives a YouTube playlist so a `--multi` download reads like the collection
/// it practically is.
///
/// The descriptive half is layered, because the good name is rarely written down as such:
///
/// 1. **The opening the videos' own names share** — effectively the season's name, worked out by
///    [`naming::shared_opening`]. This is the accurate one where it exists.
/// 2. **The page's `<title>`**, but *only* when there is no shared opening. When there is one, the
///    title is the featured episode plus the site's branding — the very name this layering exists
///    to avoid, and it would pass a naive "is it different from the videos?" test.
/// 3. **The site's domain**, so a folder always says where its contents came from.
///
/// The page's own identifier is the suffix in every case. So the worst outcome is still
/// `site.example[2025881]`: a source and an id, which is enough to find the page again.
fn collection_dir(url: &str, page: &str, ids: &[String]) -> String {
    let id = page_id(url);
    let described = naming::shared_opening(page, ids)
        .or_else(|| page_title(page))
        .unwrap_or_else(|| host_of(url));
    if described.is_empty() {
        format!("[{id}]")
    } else {
        format!("{described}[{id}]")
    }
}

/// The page's own identifier: the last non-empty path segment of its URL, which is what yt-dlp's
/// generic extractor uses for the same purpose.
fn page_id(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let path = rest.split(['?', '#']).next().unwrap_or(rest);
    path.rsplit('/').find(|segment| !segment.is_empty()).unwrap_or("page").to_string()
}

/// The text of the page's `<title>` element, trimmed. `None` when there isn't one, or it is empty.
fn page_title(page: &str) -> Option<String> {
    let open = page.find("<title")?;
    let start = open + page[open..].find('>')? + 1;
    let end = start + page[start..].find("</title")?;
    let title = page[start..end].trim();
    (!title.is_empty()).then(|| title.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{scratch_dir, ytdlp_or_skip};

    /// The shape of the page this feature was built against: a homogeneous list of objects, each
    /// with a title, a description and an entry id, plus the player's own configuration naming the
    /// featured entry a second time. Synthetic — the real page's text is its own.
    fn episode_page(ids: &[&str]) -> String {
        let mut page = String::from("<html><head><title> Some Series, Season 6 </title></head>");
        page.push_str(r#"<script>{"playerConfig":{"partnerId":2748741,"#);
        page.push_str(&format!(
            r#""url":"https://cdnapi.kaltura.com/p/2748741/sp/274874100/x","entryId":"{}"}},"#,
            ids[0]
        ));
        page.push_str(r#""items":["#);
        for (i, id) in ids.iter().enumerate() {
            page.push_str(&format!(
                r#"{{"title":"Episode {i}","description":"what happens","entryId":"{id}"}},"#
            ));
        }
        page.push_str("]}</script></html>");
        page
    }

    #[test]
    fn the_featured_video_and_its_siblings_come_back_once_each_in_page_order() {
        let ids = ["1_8jx8mama", "1_kmayywon", "1_7ot11uev", "1_zh173l2l"];
        let page = episode_page(&ids);
        let platform = platforms::claiming(ids[0]).expect("a Kaltura id");
        let found = siblings(&page, platform, ids[0]);
        // The featured id appears twice in the page (player config and list) and must be collected
        // once — and the order must be the page's, since nothing else orders these videos.
        assert_eq!(found, ids, "one entry each, in the order the page lists them");
    }

    #[test]
    fn the_scan_ignores_everything_that_is_not_an_id_of_the_resolved_shape() {
        // The precision claim: the sub-partner id, the partner id, and the surrounding JSON keys
        // all sit right beside the ids and none of them may be swept up.
        let page = episode_page(&["1_8jx8mama", "1_kmayywon"]);
        let platform = platforms::claiming("1_8jx8mama").unwrap();
        let found = siblings(&page, platform, "1_8jx8mama");
        assert_eq!(found.len(), 2, "exactly the two ids: {found:?}");
        for noise in ["2748741", "274874100", "entryId", "partnerId", "description"] {
            assert!(!found.iter().any(|id| id == noise), "{noise} was swept up");
        }
    }

    #[test]
    fn a_look_alike_id_under_a_different_key_is_not_a_sibling() {
        // The false positive that cost a failed download: a player URL carrying a widget id of
        // exactly an entry id's shape. Shape alone collects it; the key it is written under is
        // what tells them apart. Both forms below are the ones seen in the wild.
        let page = "player.html?wid=1_l0m5kgl7&entry_id=1_9gzouybz&entry_id=1_aaaaaaaa";
        let platform = platforms::claiming("1_9gzouybz").unwrap();
        let found = siblings(page, platform, "1_9gzouybz");
        assert_eq!(found, ["1_9gzouybz", "1_aaaaaaaa"], "only the entry_id ones: {found:?}");
        assert!(!found.iter().any(|id| id == "1_l0m5kgl7"), "the widget id must not be collected");
    }

    #[test]
    fn a_resolved_id_with_no_key_of_its_own_falls_back_to_matching_on_shape() {
        // A sweep that finds too much is recoverable — a failed download and a ledger line. One
        // that finds nothing is the feature not working. So an id sitting bare, with nothing
        // identifier-shaped before it, must not filter every sibling away.
        let page = r#"["1_9gzouybz","1_aaaaaaaa"] and entry_id=1_bbbbbbbb"#;
        let platform = platforms::claiming("1_9gzouybz").unwrap();
        let found = siblings(page, platform, "1_9gzouybz");
        assert_eq!(found.len(), 3, "no anchor key, so no filtering: {found:?}");
    }

    #[test]
    fn the_key_an_id_is_written_under_is_read_past_whatever_separates_them() {
        // Every separator shape seen in the wild, plus the two cases with no key at all.
        for (page, expected) in [
            (r#"{"entryId":"1_x"}"#, Some("entryId")),          // JSON, quoted
            ("player?entry_id=1_x", Some("entry_id")),          // a query parameter
            ("a&amp;&wid=1_x", Some("wid")),                    // past escaped punctuation
            (r#"{ "key" : "1_x" }"#, Some("key")),              // past whitespace
            (r#"["1_x"]"#, None),                               // bare in an array: no key
        ] {
            let found = key_before(page, page.find("1_x").unwrap());
            assert_eq!(found, expected, "in {page}");
        }
        assert_eq!(key_before("1_x", 0), None, "nothing precedes the start of the page");
    }

    #[test]
    fn a_page_with_one_video_is_not_a_collection() {
        // The promise of the flag: a page that already works must come back as "not mine", so the
        // caller downloads it exactly as it did before.
        let page = episode_page(&["1_8jx8mama"]);
        let platform = platforms::claiming("1_8jx8mama").unwrap();
        assert_eq!(siblings(&page, platform, "1_8jx8mama").len(), 1);
        assert!(1 < COLLECTION_MINIMUM, "one video is below the threshold");
    }

    #[test]
    fn the_collection_folder_is_named_for_the_page_and_falls_back_to_its_id() {
        let url = "https://broadcaster.example/allshows/series/69/season/6/2025881/";
        assert_eq!(page_id(url), "2025881", "the last path segment, trailing slash and all");
        assert_eq!(page_id("https://x.test/a/b?c=d#e"), "b", "query and fragment are not the id");
        assert_eq!(page_id("https://x.test/"), "x.test", "with no path, the host will do");

        let ids: Vec<String> =
            ["1_aaaaaaaa", "1_bbbbbbbb"].iter().map(|id| id.to_string()).collect();

        // Layer 1: the opening the videos' own names share wins, even though the page has a
        // perfectly good-looking `<title>`. On the real page that title is the *featured episode*,
        // which is the whole reason it must not be preferred.
        let listed = episode_page(&["1_aaaaaaaa", "1_bbbbbbbb"]);
        assert_eq!(collection_dir(url, &listed, &ids), "Episode[2025881]");

        // Layer 2: with nothing shared among the videos, the page title is the best there is.
        let unrelated = "<html><head><title>A Whole Season</title></head>\
                         <body>{\"title\":\"Alpha\",\"entryId\":\"1_aaaaaaaa\"}\
                         {\"title\":\"Zulu\",\"entryId\":\"1_bbbbbbbb\"}</body></html>";
        assert_eq!(collection_dir(url, unrelated, &ids), "A Whole Season[2025881]");

        // Layer 3: nothing shared and no title, so the folder at least says where it came from.
        let bare = "<html>{\"title\":\"Alpha\",\"entryId\":\"1_aaaaaaaa\"}\
                    {\"title\":\"Zulu\",\"entryId\":\"1_bbbbbbbb\"}</html>";
        assert_eq!(collection_dir(url, bare, &ids), "broadcaster.example[2025881]");

        // And the identifier is the suffix in every case, so the worst outcome is still a source
        // and an id — enough to find the page again.
        for page in [listed.as_str(), unrelated, bare] {
            assert!(collection_dir(url, page, &ids).ends_with("[2025881]"), "{page:.40}");
        }
    }

    /// Serve `body` over HTTP on localhost and give back the URL to fetch it at.
    ///
    /// A dozen lines of `std::net` rather than a test-server dependency, for the one test that needs
    /// **real yt-dlp** to do a real fetch. The thread serves a few requests and is then left to be
    /// reaped when the test binary exits; a test has no one to hand it back to.
    fn serve(body: String) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a local port");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(4).flatten() {
                let mut stream = stream;
                let _ = stream.read(&mut [0u8; 4096]);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            }
        });
        format!("http://127.0.0.1:{port}/series/42/index.html")
    }

    #[test]
    fn real_ytdlp_leaves_a_page_this_module_reads_back_correctly() {
        let test = "real_ytdlp_leaves_a_page_this_module_reads_back_correctly";
        if !ytdlp_or_skip(test) {
            return;
        }
        // The durable end-to-end test: our own page, served locally, fetched by the real yt-dlp.
        // It exercises the one thing no hand-written fixture can — the file names `--write-pages`
        // actually produces — and it cannot rot, because nobody else owns the page.
        let ids = ["1_aaaaaaaa", "1_bbbbbbbb", "1_cccccccc"];
        let url = serve(episode_page(&ids));
        let dir = scratch_dir("page_live");
        let (program, args) = crate::ytdlp::ytdlp_invocation(vec![
            OsString::from("--write-pages"),
            "--print".into(),
            "%(id)s".into(),
            url.clone().into(),
        ]);
        // The exit status is ignored on purpose: these ids are not real entries, so extraction
        // fails. The page is fetched and written either way, which is what is under test.
        let _ = capture_stdout_in(&dir, program, args);

        let left: Vec<String> = std::fs::read_dir(&dir)
            .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        let page = page_dump(&dir, &url)
            .unwrap_or_else(|| panic!("no page dump for {url}; dir holds {left:?}"));
        let platform = platforms::claiming(ids[0]).expect("a Kaltura id");
        assert_eq!(
            siblings(&page, platform, ids[0]),
            ids,
            "every entry, once, in page order — read off a page yt-dlp really wrote"
        );
        // And the folder name is derived from the same real page: the opening the entries share,
        // plus the page's own identifier as the suffix.
        let owned: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
        assert_eq!(collection_dir(&url, &page, &owned), "Episode[index.html]");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_page_is_picked_out_of_the_dumps_by_host_and_never_by_size() {
        // `--write-pages` leaves every response yt-dlp fetched. Picking the wrong one is not a
        // near miss: a platform's API replies repeat ids the page never lists, which inflates the
        // sibling count. These are the real file names yt-dlp produced for two different sites.
        let dir = scratch_dir("page_dumps");
        let page = "lecture-05sep2017_https_-_www.cns.nyu.edu_~eero_math-tools17_Videos_x.dump";
        let api = "1_9gzouybz_dde13be2610b0773875b15718fb75705_http_-_cdnapi.kaltura.com_api.dump";
        let manifest = "1_9gzouybz_http_-_cdnapi.kaltura.com_p_1674401_playManifest.dump";
        // Deliberately the sizes that broke the first version: the API reply is the biggest file.
        std::fs::write(dir.join(page), "the page".repeat(20)).unwrap();
        std::fs::write(dir.join(api), "an api reply".repeat(200)).unwrap();
        std::fs::write(dir.join(manifest), "a manifest").unwrap();

        let url = "https://www.cns.nyu.edu/~eero/math-tools17/Videos/lecture-05sep2017.html";
        let found = page_dump(&dir, url).expect("the page is there");
        assert!(found.starts_with("the page"), "picked the wrong dump: {}", &found[..20]);

        // No dump named for the site means no page, rather than the platform's reply standing in
        // for one — a wrong page is worse than none, since it would be swept for siblings.
        assert!(page_dump(&dir, "https://elsewhere.test/x").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_host_is_read_off_a_url_the_way_a_dump_name_carries_it() {
        assert_eq!(host_of("https://broadcaster.example/allshows/series/69/"), "broadcaster.example");
        assert_eq!(host_of("https://www.cns.nyu.edu/~eero/x.html"), "www.cns.nyu.edu");
        assert_eq!(host_of("http://user:pw@media.example.com/v/1"), "media.example.com");
        assert_eq!(host_of("https://x.test?a=b"), "x.test");
        // The port goes: yt-dlp writes its colon as an underscore in the dump's name, so a URL
        // carrying one would otherwise never match its own page. Caught by the localhost test,
        // which serves on an ephemeral port and so always has one.
        assert_eq!(host_of("http://127.0.0.1:38267/series/42/index.html"), "127.0.0.1");
        // Not a URL parser, and it doesn't need to be — it only ever sees the page URL a run was
        // given. A schemeless input yields its leading segment, which matches no dump name and so
        // fails closed rather than picking the wrong file.
        assert_eq!(host_of("no-scheme-here/x"), "no-scheme-here");
    }

    #[test]
    fn the_page_title_is_read_out_of_whatever_shape_the_tag_takes() {
        assert_eq!(page_title("<title>Plain</title>").as_deref(), Some("Plain"));
        assert_eq!(page_title(r#"<title lang="he">With attrs</title>"#).as_deref(), Some("With attrs"));
        assert_eq!(page_title("<TITLE>Upper</TITLE>"), None, "matched case-sensitively, as served");
        assert_eq!(page_title("<title>unclosed"), None);
        assert_eq!(page_title("nothing"), None);
    }
}
