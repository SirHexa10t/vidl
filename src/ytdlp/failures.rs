//! Diagnose why a download died — yt-dlp's phrasing classified into a [`Failure`], each with
//! its user-facing advice line — plus the geo-rescue region list and the failed-downloads
//! ledger (written, matched, and scrubbed here).

use std::ffi::OsString;
use std::path::Path;

use crate::util::exec::capture_output;
use super::{ARCHIVE_NAME, ytdlp_invocation};
use super::scan::{archived_ids};
use crate::util::stamp;

/// Regions tried in order when a download proves geo-blocked — `--xff` header spoofing
/// defeats softly-enforced blocks.
const XFF_REGIONS: &[&str] =
    &["US", "GB", "DE", "FR", "NL", "SE", "CA", "AU", "JP", "KR", "BR", "IN"];

/// How [`diagnose_failure`] treats a geo-block, chosen by the caller per site. Header spoofing
/// only shifts a *softly*-enforced block (a `--xff` region the site trusts); YouTube enforces geo
/// by the real connection IP and ignores the header (verified against yt-dlp's source), so a dozen
/// spoofed retries would only waste time — it reports the block instead.
pub(crate) enum GeoRescue {
    /// Walk [`XFF_REGIONS`], retrying each — for sites that may honor `X-Forwarded-For`.
    XffSweep,
    /// Report the block without retrying — for sites that enforce geo by IP (YouTube).
    IpEnforced,
}

/// The failure ledger, written beside the collection's archive: what stayed undownloaded and
/// why. Entries here are also deliberately NOT archived, so every future run retries them —
/// members-only videos get released publicly later, and blocks lift. Named for `dl`, not
/// YouTube — the generic path writes here too.
const FAILED_LEDGER: &str = ".dl_video_failed_download.txt";

// TODO: scrape historic snapshots (e.g. the WaybackMachine's copies of channel tabs) to find
// videos that were de-listed or disappeared entirely — candidates for the unplayable report and
// the failure ledger, which today only see what YouTube still admits exists.

/// What a failed download turned out to be, judged by yt-dlp's stderr.
#[derive(Debug, PartialEq)]
enum Failure {
    Geo,
    BotWall,
    Members,
    AgeRestricted,
    Sensitive,
    LoginRequired,
    Drm,
    Other,
}

fn classify_failure(stderr: &str) -> Failure {
    // Match case-insensitively against the phrasings yt-dlp's extractors actually emit (verified
    // against their source): the geo notices; the anti-bot/CAPTCHA and JS-challenge walls; the
    // members badge reasons; the YouTube age gate; the login/private/sensitive gates; and
    // report_drm's DRM notice. Order matters where phrasings could overlap — the bot-wall is
    // checked before the login/age gates (YouTube's "…confirm you're not a bot" is a bot-wall,
    // not a login). A missed geo phrasing costs the geo rescue, so that set errs wide; a bare
    // 403 is left as `Other` on purpose (it's ambiguous — rate-limit vs. bot vs. transient —
    // and we won't mislabel it).
    let s = stderr.to_lowercase();
    let has = |needle: &str| s.contains(needle);
    if has("in your country") || has("from your location") || has("in your region") || has("geo restrict") || has("geo_restrict") {
        Failure::Geo
    } else if has("not a bot") || has("captcha") || has("unusual traffic") || has("solve js challenge") || has("challenge data") || has("verify you are human") || has("verify you're human") {
        Failure::BotWall
    } else if has("members-only") || has("members only") || has("channel's members") || has("join this channel") {
        Failure::Members
    } else if has("confirm your age") || has("age-restricted") || has("age-verification") || has("age_check_required") {
        Failure::AgeRestricted
    } else if has("for some audiences") || has("not be comfortable") {
        Failure::Sensitive
    } else if has("requiring login") || has("log in for access") || has("log into an account") || has("permission to view") || has("account is private") || has("private video") {
        Failure::LoginRequired
    } else if has("drm protected") || has("drm-protected") || has("protected by drm") {
        Failure::Drm
    } else {
        Failure::Other
    }
}

pub(crate) fn capture_ytdlp(argv: Vec<OsString>) -> Option<(bool, String, String)> {
    let (program, args) = ytdlp_invocation(argv);
    capture_output(program, args)
}

/// Re-run a failed download with output captured to learn WHY (group runs stream live and keep no
/// stderr) and return the ledger line for whatever stays dead (`None` when a retry succeeded).
/// A geo-block is handled per `rescue`: [`GeoRescue::XffSweep`] walks [`XFF_REGIONS`] for sites
/// that may honor a spoofed `X-Forwarded-For`; [`GeoRescue::IpEnforced`] (YouTube) reports it
/// without retrying, since YouTube reads the real connection IP and ignores the header. A terminal
/// failure is also announced on stderr as it's decided, so the reason shows in the run's output —
/// not only later in the ledger.
pub(crate) fn diagnose_failure(base: Vec<OsString>, label: &str, rescue: GeoRescue) -> Option<String> {
    let Some((ok, _, stderr)) = capture_ytdlp(base.clone()) else {
        return Some(dead(format!("{label} — failed (could not even re-run yt-dlp)")));
    };
    if ok {
        println!("{label}: succeeded on retry");
        return None;
    }
    // Whether the failed attempt already carried cookies — the login/age gates give honest advice
    // from this (don't say "add cookies" when they were already there).
    let had_cookies = base.iter().any(|a| a == "--cookies" || a == "--cookies-from-browser");
    match classify_failure(&stderr) {
        Failure::Geo => match rescue {
            // Sites that may honor a spoofed X-Forwarded-For: try each region, take the first win.
            GeoRescue::XffSweep => {
                for region in XFF_REGIONS {
                    println!("{label}: geo-blocked — trying region {region}…");
                    let mut spoofed = base.clone();
                    spoofed.push("--xff".into());
                    spoofed.push((*region).into());
                    if matches!(capture_ytdlp(spoofed), Some((true, _, _))) {
                        println!("{label}: region {region} worked");
                        return None;
                    }
                }
                Some(dead(format!("{label} — geo-blocked (tried {})", XFF_REGIONS.join(","))))
            }
            // YouTube reads the real connection IP and ignores X-Forwarded-For, so spoofing can't
            // move the block — don't burn a dozen retries; report it and the only real fix.
            GeoRescue::IpEnforced => Some(dead(format!(
                "{label} — geo-blocked (enforced by IP; retry from a VPN or --proxy with an IP in an allowed region)"
            ))),
        },
        Failure::BotWall => Some(dead(bot_wall_line(label))),
        Failure::Members => {
            Some(dead(format!("{label} — members-only (channels often release these publicly later)")))
        }
        Failure::AgeRestricted => Some(dead(age_restricted_line(label, had_cookies))),
        Failure::Sensitive => Some(dead(sensitive_content_line(label, had_cookies))),
        Failure::LoginRequired => Some(dead(login_required_line(label, had_cookies))),
        Failure::Drm => Some(dead(drm_line(label, had_cookies))),
        Failure::Other => {
            let detail = stderr
                .lines()
                .find(|line| line.contains("ERROR"))
                .unwrap_or("unknown error")
                .trim();
            Some(dead(format!("{label} — failed: {detail}")))
        }
    }
}

/// Announce a terminal download failure on stderr and hand the same text back for the ledger,
/// so the reason shows in the live run and is recorded in one move.
pub(crate) fn dead(line: String) -> String {
    eprintln!("vidl: {line}");
    line
}

/// The ledger line for an age-restricted block, tailored to whether cookies were already tried.
/// Without cookies it's a plain nudge to add them; with cookies the gate is the harder kind — it
/// needs a signed-in *18+* account, and YouTube age-verifies the browser *session*, so the fix is
/// to verify in that browser and re-import, not to repeat "add cookies" when the user already did.
/// The cookies-present line also names the one lever past that (per the cookie research, entitled
/// cookies are necessary but not always sufficient without a PO token — not integrated yet).
fn age_restricted_line(label: &str, had_cookies: bool) -> String {
    if had_cookies {
        format!("{label} — age-restricted despite cookies (use a signed-in 18+ account; play it in that browser once to verify age, then re-import; past that, the last lever is a PO-token provider — not integrated yet)")
    } else {
        format!("{label} — age-restricted (needs cookies from a signed-in 18+ account: dl --cookie-import youtube, then retry)")
    }
}

/// The ledger line for content behind a login / private / sensitive gate, tailored to whether
/// cookies were already tried. Unlike a bot-wall this IS solvable — cookies from an account with
/// access are the fix — so without them it points at the import; with them it means the account
/// lacks access or the cookies went stale, not that a plain retry would help.
fn login_required_line(label: &str, had_cookies: bool) -> String {
    if had_cookies {
        format!("{label} — still blocked with cookies (the account may lack access, or the cookies went stale — re-import from a browser where it plays)")
    } else {
        format!("{label} — needs an account with access: import cookies (dl --cookie-import <site>), then retry")
    }
}

/// The ledger line for a post flagged "sensitive" / not-for-all-audiences (TikTok's "may not be
/// comfortable for some audiences"). Solvable like a login gate — an account allowed to view it —
/// but the account itself must be permitted (18+ / mature content enabled), so with cookies already
/// present the fix is that account condition, not another plain retry.
fn sensitive_content_line(label: &str, had_cookies: bool) -> String {
    if had_cookies {
        format!("{label} — flagged sensitive and still blocked with cookies (the account must be allowed to view sensitive/mature content — re-import from a browser where it plays)")
    } else {
        format!("{label} — flagged sensitive (not for all audiences): needs a signed-in account allowed to view it (dl --cookie-import <site>), then retry")
    }
}

/// The ledger line for an anti-bot / CAPTCHA / JS-challenge wall. Unlike the login/age gates this
/// has no reliable fix — yt-dlp can't solve a human-verification challenge — so the line is honest
/// that the post may be undownloadable and never tells the user to just try again. It does name
/// every real lever, including the one this crate doesn't wire up yet (a PO-token provider — per the
/// cookie research, the fourth mitigation besides cookies, IP, and yt-dlp freshness).
fn bot_wall_line(label: &str) -> String {
    format!("{label} — blocked by an anti-bot/CAPTCHA challenge yt-dlp can't solve; may be undownloadable (fresh cookies from a browser where it plays, a matching IP, and current yt-dlp sometimes help; the last lever is a PO-token provider — not integrated yet)")
}

/// The ledger line for DRM-protected content, tailored to whether cookies were already tried.
/// yt-dlp never circumvents DRM — but per the cookie research, YouTube's `tv` client serves
/// DRM'd formats to cookie-less requests while ANY cookies (even a logged-out browser session)
/// surface non-DRM formats. So without cookies, "import and retry" is a genuine fix, not a
/// platitude; with them, it's the real thing and honesty beats a retry loop.
fn drm_line(label: &str, had_cookies: bool) -> String {
    if had_cookies {
        format!("{label} — DRM-protected even with cookies (yt-dlp doesn't circumvent DRM — undownloadable)")
    } else {
        format!("{label} — DRM-protected formats only; cookies sometimes unlock non-DRM variants (on YouTube even a logged-out session works): dl --cookie-import <site>, then retry")
    }
}

/// Whether a ledger line refers to video `id` — via the bracketed `[id]` every writer now
/// emits, the `=id` shape of a URL label (`watch?v=id`), or a legacy bare-id label opening the
/// line. Deliberately delimited forms rather than raw substring: a short id from a non-YouTube
/// extractor must not match mid-word inside some other entry's title.
fn ledger_line_refers(line: &str, id: &str) -> bool {
    line.contains(&format!("[{id}]"))
        || line.contains(&format!("={id}"))
        || line.starts_with(&format!("{id} "))
}

/// Clear ledger entries whose videos have since downloaded — the archive is the proof of
/// success. Runs after every download pass, so a lifted geo-block or a members-only video the
/// channel later released drops off the list the moment its retry lands (entries are left
/// unarchived precisely so reruns keep retrying them). Timestamp headers left with no entries
/// go too, and a fully-cleared ledger file is removed. One line per entry keeps this a
/// line-filter; entries whose label carries no id (non-YouTube URL labels) stay until pruned
/// by hand.
pub(crate) fn scrub_ledger(dir: &Path) {
    let path = dir.join(FAILED_LEDGER);
    let Ok(text) = std::fs::read_to_string(&path) else { return };
    let archived = archived_ids(&dir.join(ARCHIVE_NAME));
    if archived.is_empty() {
        return;
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut cleared = 0usize;
    for line in text.lines() {
        if line.starts_with("── ") {
            // A header whose whole block was cleared is still on top of the stack — replace it.
            if kept.last().is_some_and(|last| last.starts_with("── ")) {
                kept.pop();
            }
            kept.push(line);
        } else if archived.iter().any(|id| ledger_line_refers(line, id)) {
            cleared += 1;
        } else if !line.trim().is_empty() {
            kept.push(line);
        }
    }
    if kept.last().is_some_and(|last| last.starts_with("── ")) {
        kept.pop();
    }
    if cleared == 0 {
        return;
    }
    let outcome = if kept.is_empty() {
        std::fs::remove_file(&path)
    } else {
        std::fs::write(&path, kept.join("\n") + "\n")
    };
    match outcome {
        Ok(()) => println!(
            "{cleared} previously-failed download(s) have since succeeded — cleared from {}{}",
            path.display(),
            if kept.is_empty() { " (nothing left; file removed)" } else { "" },
        ),
        Err(err) => eprintln!("vidl: could not rewrite {}: {err}", path.display()),
    }
}

/// Append this run's failures to the ledger and tell the user where it lives.
pub(crate) fn write_ledger(dir: &Path, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    let path = dir.join(FAILED_LEDGER);
    let mut block = format!("── {} ──\n", stamp::datehour_stamp());
    for line in lines {
        block += line;
        block.push('\n');
    }
    use std::io::Write;
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| file.write_all(block.as_bytes()));
    match written {
        Ok(()) => println!("{} download(s) failed — details in {}", lines.len(), path.display()),
        Err(err) => eprintln!("vidl: could not write {}: {err}", path.display()),
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::ytdlp::{ARCHIVE_NAME};

    #[test]
    fn failures_classify_by_ytdlps_phrasing() {
        // Geo — both real YouTube-extractor phrasings, plus the generic raise_geo_restricted
        // default and the region/case variants the widened matcher must still catch.
        for msg in [
            "ERROR: [youtube] x: The uploader has not made this video available in your country",
            "ERROR: [youtube] x: This video is not available from your location due to geo restriction",
            "ERROR: This playlist is likely not available in your region",
            "ERROR: Video is GEO restricted",
        ] {
            assert_eq!(classify_failure(msg), Failure::Geo, "{msg}");
        }
        // Members — the badge reasons YouTube returns, hyphen and space spellings.
        for msg in [
            "ERROR: Join this channel to get access to members-only content like this video",
            "ERROR: This video is available to this channel's members on level: Tier 1",
        ] {
            assert_eq!(classify_failure(msg), Failure::Members, "{msg}");
        }
        // DRM — report_drm's phrasing gets its own class, so the cookie quirk advice can fire.
        assert_eq!(
            classify_failure("ERROR: [youtube] x: This video is DRM protected"),
            Failure::Drm
        );
        // Age — the sign-in gate and the account age-verification wording.
        assert_eq!(classify_failure("ERROR: Sign in to confirm your age"), Failure::AgeRestricted);
        assert_eq!(
            classify_failure("ERROR: This video is age-restricted and YouTube is requiring account age-verification"),
            Failure::AgeRestricted
        );
        // Bot-wall — the anti-automation walls yt-dlp can't solve: YouTube's bot check, TikTok's
        // JS challenge, Google's unusual-traffic notice.
        for msg in [
            "ERROR: [youtube] x: Sign in to confirm you're not a bot. Use --cookies-from-browser",
            "ERROR: [TikTok] x: Unable to solve JS challenge",
            "ERROR: Our systems have detected unusual traffic from your computer network",
        ] {
            assert_eq!(classify_failure(msg), Failure::BotWall, "{msg}");
        }
        // Sensitive / not-for-all-audiences — flagged group-offensive; "for some audiences" is the
        // tell, and it's checked before the login gate even though it also says "Log in for access"
        // (distinct from YouTube's age gate, which says "for some users").
        assert_eq!(
            classify_failure("ERROR: [TikTok] x: This post may not be comfortable for some audiences. Log in for access"),
            Failure::Sensitive
        );
        // Login / private gates — solvable with cookies from an account with access.
        for msg in [
            "ERROR: [TikTok] x: TikTok is requiring login for access to this content",
            "ERROR: [youtube] x: Private video. Sign in if you've been granted access to this video",
        ] {
            assert_eq!(classify_failure(msg), Failure::LoginRequired, "{msg}");
        }
        // A bare 403 stays Other on purpose — ambiguous (rate-limit vs. bot vs. transient), so we
        // don't mislabel it as a bot-wall.
        assert_eq!(classify_failure("ERROR: HTTP Error 403: Forbidden"), Failure::Other);
        assert_eq!(XFF_REGIONS.len(), 12, "a dozen regions, as specified");
    }

    #[test]
    fn geo_ledger_lines_name_the_video_and_carry_the_scrub_key() {
        // Two geo outcomes reach the ledger. The generic path may sweep XFF regions, so its line
        // records every region tried; the YouTube path is IP-enforced (spoofing can't help), so
        // its line names the block and the real fix, no regions. Both must name the video, carry
        // the scrub key in a delimited form, and stay on one line (so scrub_ledger can line-filter).
        let swept = format!(
            "https://vid.example/watch?v=dQw4w9WgXcQ — geo-blocked (tried {})",
            XFF_REGIONS.join(",")
        );
        assert!(ledger_line_refers(&swept, "dQw4w9WgXcQ"), "sweep line carries the scrub key");
        assert!(swept.contains("geo-blocked") && !swept.contains('\n'), "one geo-blocked line");
        for region in XFF_REGIONS {
            assert!(swept.contains(region), "sweep line records region {region}");
        }

        let ip_enforced = "#7 Some Title [dQw4w9WgXcQ] — geo-blocked (enforced by IP; retry from a VPN or --proxy with an IP in an allowed region)".to_string();
        assert!(ip_enforced.contains("#7 Some Title"), "names the video");
        assert!(ledger_line_refers(&ip_enforced, "dQw4w9WgXcQ"), "carries the scrub key");
        assert!(ip_enforced.contains("geo-blocked") && !ip_enforced.contains('\n'), "one geo-blocked line");
        assert!(!ip_enforced.contains("tried"), "no region sweep on the IP-enforced path");
    }

    #[test]
    fn the_age_restricted_line_adapts_to_whether_cookies_were_tried() {
        let without = age_restricted_line("[dQw4w9WgXcQ]", false);
        assert!(without.contains("age-restricted"), "names the gate");
        assert!(without.contains("--cookie-import"), "nudges toward importing cookies");
        assert!(!without.contains("despite"), "the no-cookies case just asks for cookies");

        let with = age_restricted_line("[dQw4w9WgXcQ]", true);
        assert!(with.contains("despite cookies"), "the cookies-present case names the harder gate");
        assert!(with.contains("18+") && with.contains("verify age"), "points at the real fix");
        assert!(with.contains("PO-token"), "names the lever past cookies (necessary ≠ sufficient)");
        assert!(!without.contains("PO-token"), "the plain case keeps the simple fix simple");
        assert!(ledger_line_refers(&with, "dQw4w9WgXcQ"), "still carries the scrub key");
    }

    #[test]
    fn the_drm_line_reaches_for_cookies_first_and_is_terminal_with_them() {
        // The tv-client quirk: cookie-less requests get DRM'd formats, ANY cookies (even a
        // logged-out session) surface non-DRM ones — so cookies are a genuine first fix.
        let without = drm_line("[dQw4w9WgXcQ]", false);
        assert!(without.contains("DRM-protected"), "names the gate");
        assert!(without.contains("--cookie-import"), "cookies are the one real lever");
        let with = drm_line("[dQw4w9WgXcQ]", true);
        assert!(with.contains("even with cookies"), "won't repeat advice already taken");
        assert!(with.contains("doesn't circumvent"), "honest that this is terminal");
        assert!(!with.to_lowercase().contains("retry"), "no futile retry once cookies were tried");
        assert!(ledger_line_refers(&with, "dQw4w9WgXcQ"), "carries the scrub key");
    }

    #[test]
    fn the_login_required_line_points_at_cookies_and_adapts_to_whether_they_were_tried() {
        let without = login_required_line("[dQw4w9WgXcQ]", false);
        assert!(without.contains("--cookie-import"), "nudges toward importing cookies");
        assert!(without.contains("retry"), "with cookies as the real fix, retrying is right advice");

        let with = login_required_line("[dQw4w9WgXcQ]", true);
        assert!(with.contains("lack access") || with.contains("stale"), "names why the cookies didn't help");
        assert!(!with.contains("retry"), "no plain-retry advice once cookies already failed");
        assert!(ledger_line_refers(&with, "dQw4w9WgXcQ"), "carries the scrub key");
    }

    #[test]
    fn the_sensitive_content_line_points_at_an_allowed_account_and_adapts_to_cookies() {
        let without = sensitive_content_line("[dQw4w9WgXcQ]", false);
        assert!(without.contains("sensitive"), "names why it's gated");
        assert!(without.contains("--cookie-import") && without.contains("retry"), "cookies are the fix");

        let with = sensitive_content_line("[dQw4w9WgXcQ]", true);
        assert!(with.contains("sensitive/mature") || with.contains("allowed to view"), "names the account condition");
        assert!(!with.contains("retry"), "no plain-retry once cookies already failed");
        assert!(ledger_line_refers(&with, "dQw4w9WgXcQ"), "carries the scrub key");
    }

    #[test]
    fn the_bot_wall_line_is_honest_that_it_may_be_unsolvable_and_never_says_retry() {
        let line = bot_wall_line("[dQw4w9WgXcQ]");
        assert!(line.contains("anti-bot") || line.contains("CAPTCHA"), "names the difficulty");
        assert!(line.contains("undownloadable"), "is honest it may not be possible");
        assert!(!line.to_lowercase().contains("retry"), "must not tell the user to just download again");
        assert!(line.contains("PO-token"), "names every real lever, including the unintegrated one");
        assert!(ledger_line_refers(&line, "dQw4w9WgXcQ"), "carries the scrub key");
    }

    #[test]
    fn ledger_lines_match_ids_only_in_delimited_forms() {
        assert!(ledger_line_refers("#3 Title [abc123XYZ_-] — members-only", "abc123XYZ_-"));
        assert!(ledger_line_refers("https://www.youtube.com/watch?v=abc123XYZ_- — failed: gone", "abc123XYZ_-"));
        assert!(ledger_line_refers("abc123XYZ_- — geo-blocked (tried US)", "abc123XYZ_-"), "legacy bare-id label");
        // A short id must not clear someone else's entry by matching inside its title.
        assert!(!ledger_line_refers("#4 my abc mixtape [zzzzzzzzzzz] — members-only", "abc"));
    }

    #[test]
    fn the_scrub_clears_now_downloaded_entries_prunes_empty_blocks_and_removes_an_emptied_ledger() {
        let dir = std::env::temp_dir().join(format!("vidl_scrub_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ledger = dir.join(FAILED_LEDGER);
        std::fs::write(
            &ledger,
            "── 2026-01-01_1200 ──\n\
             #7 Released Later [vidAAAAAAAA] — members-only (channels often release these publicly later)\n\
             ── 2026-01-02_1200 ──\n\
             #2 Still Blocked [vidBBBBBBBB] — geo-blocked (tried US,GB)\n\
             #9 Also Freed [vidCCCCCCCC] — members-only (channels often release these publicly later)\n",
        )
        .unwrap();

        // Run 1: A and C have since downloaded (they're in the archive); B still hasn't.
        std::fs::write(dir.join(ARCHIVE_NAME), "youtube vidAAAAAAAA\nyoutube vidCCCCCCCC\n").unwrap();
        scrub_ledger(&dir);
        let text = std::fs::read_to_string(&ledger).unwrap();
        assert!(!text.contains("vidAAAAAAAA") && !text.contains("vidCCCCCCCC"), "cleared: {text}");
        assert!(text.contains("vidBBBBBBBB"), "unresolved entry stays: {text}");
        assert_eq!(
            text.matches("── ").count(),
            1,
            "the block whose only entry cleared lost its header too: {text}"
        );

        // Run 2: B downloads as well — nothing left, so the ledger file itself goes.
        std::fs::write(dir.join(ARCHIVE_NAME), "youtube vidAAAAAAAA\nyoutube vidBBBBBBBB\nyoutube vidCCCCCCCC\n").unwrap();
        scrub_ledger(&dir);
        assert!(!ledger.exists(), "an emptied ledger is removed");

        // And scrubbing with no ledger (or no archive) is a quiet no-op.
        scrub_ledger(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
