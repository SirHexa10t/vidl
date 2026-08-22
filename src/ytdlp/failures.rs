//! Diagnose why a download died — yt-dlp's phrasing classified into a [`Failure`], each with
//! its user-facing advice line — plus the geo-rescue region list and the failed-downloads
//! ledger (written, matched, and scrubbed here).

use std::ffi::OsString;
use std::path::Path;

use crate::util::exec::capture_output;
use crate::util::size::roughly;
use super::{ARCHIVE_NAME, names, ytdlp_invocation};
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
    /// The filesystem refused the name, not the site. Local, and so the one failure here that no
    /// cookie, region or retry can help with.
    NameTooLong,
    /// A resume asked for bytes the server does not have. Caused by a leftover `.part` at least as
    /// long as the remote file — the shape an interrupt leaves when it lands after the last byte
    /// but before the rename. Local in origin like [`Failure::NameTooLong`], and likewise immune
    /// to a plain retry: the same impossible range is asked for every time.
    RangeRefused,
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
    // Checked first, and safely so: this is the OS talking, not an extractor, and its phrasing
    // overlaps none of the gates below. `ENAMETOOLONG` is errno 36 on Linux but not everywhere,
    // so the message text is the portable half of the match.
    if has("file name too long") || has("filename too long") || has("errno 36") {
        Failure::NameTooLong
    // Also local in origin, and checked early for the same reason: a refused byte range says
    // nothing about the site's willingness to serve, only that the leftover `.part` is too long
    // for the range to exist. Matched on the phrase and on `error 416` rather than on a bare
    // "416", which would happily match a video id or a title.
    } else if has("range not satisfiable") || has("error 416") {
        Failure::RangeRefused
    } else if has("in your country") || has("from your location") || has("in your region") || has("geo restrict") || has("geo_restrict") {
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

/// The line yt-dlp meant as the cause — its first `ERROR`, or an admission that there wasn't one.
fn error_line(stderr: &str) -> &str {
    stderr.lines().find(|line| line.contains("ERROR")).unwrap_or("unknown error").trim()
}

/// The same download, told to start the file from the beginning instead of resuming it.
///
/// `None` when the argv already says so, which means starting over has already been tried and
/// failed — there is no third thing to attempt, and re-running an identical command would only
/// produce an identical failure.
fn restarting(base: &[OsString]) -> Option<Vec<OsString>> {
    if base.iter().any(|arg| arg == "--no-continue") {
        return None;
    }
    let mut fresh = base.to_vec();
    fresh.push("--no-continue".into());
    Some(fresh)
}

/// The offset a refused resume was going to continue from, read back out of yt-dlp's own report of
/// it (`[download] Resuming download at byte 749233884`).
///
/// Worth reading rather than measuring the `.part` on disk: this is the number the downloader
/// actually acted on, and finding the file would mean reconstructing its name from the `--output`
/// template — the same guess, made less reliably.
fn resumed_at(output: &str) -> Option<u64> {
    output
        .split("Resuming download at byte ")
        .nth(1)?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

/// What to record when starting over cleared the stale file and something else stopped the
/// download anyway.
///
/// Two facts, both needed: the range refusal is why the run looked odd, and the second error is
/// why it still failed. Naming only the first would send the user after a `.part` the restart has
/// already consumed.
fn restarted_into_line(label: &str, detail: &str) -> String {
    format!(
        "{label} — the partial file could not be resumed (the server refused the byte range), so it \
         was started over; that got past the leftover file and then failed: {detail}"
    )
}

/// What to record when a refused resume could not be rescued.
///
/// Names the `.part` file as the thing to remove, because that is the fix a person can apply and
/// the one the crate could not: when even starting over fails, the leftover file is still sitting
/// there and still poisoning every future run of this command.
fn range_refused_line(label: &str, restarted: bool) -> String {
    let tail = if restarted {
        "starting it over did not help either — delete the leftover `.part` file beside the \
         destination and run this again"
    } else {
        "the download was already told to start over, so the leftover `.part` file beside the \
         destination is not the whole story — delete it and run this again to see the real error"
    };
    format!("{label} — a partial file could not be resumed (the server refused the byte range); {tail}")
}

/// The ledger line for a failure this crate cannot name — plus the one hint worth giving for the
/// commonest of them.
///
/// The 403 note lives *here*, and not beside the launch, because by this point the plain re-run has
/// already been tried and has already failed. A lone 403 is very often transient — the first request
/// from an address the far end has not seen gets refused and the next one does not — so the same
/// words printed before that re-run would be contradicted as often as not.
///
/// And the obvious-sounding remedy is partly backwards, which is why it is worded carefully: moving
/// to another exit node hands the site an address it has *never* seen, and that is precisely what
/// gets refused on its own first request. Worth trying for a persistent block; not a free action.
fn other_line(label: &str, detail: &str) -> String {
    let mut line = format!("{label} — failed: {detail}");
    if detail.contains("403") {
        line.push_str(
            "\n  (a 403 that survives a retry suggests the site has blocklisted this network's \
             address — VPN and datacenter exits often are, and --cookies is the other lever. Note \
             that switching networks presents an address the site has never seen, which can itself \
             be refused once before it settles.)",
        );
    }
    line
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
    let Some((ok, stdout, stderr)) = capture_ytdlp(base.clone()) else {
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
        // The OS refused the name, not the site the video — so a plain retry is pointless, but a
        // *shorter* name is a different request. One attempt at half the title budget: a name only
        // a little too long comes through, and the file lands with an uglier name instead of not
        // landing at all. Nothing about the download changes but the `--output` template.
        Failure::NameTooLong => {
            let mut tried_shorter = false;
            if let Some(shorter) = names::halve_title_bound(&base) {
                println!("{label}: the file name was refused — retrying with a shorter one");
                tried_shorter = true;
                if matches!(capture_ytdlp(shorter), Some((true, _, _))) {
                    println!("{label}: succeeded with a shortened name");
                    return None;
                }
            }
            Some(dead(name_too_long_line(label, tried_shorter)))
        }
        // The resume asked the server for bytes past the end of what it has, so the request can
        // never succeed however many times it is repeated — and yt-dlp's own recovery for this is
        // unreachable (it reopens the connection to re-read the length, but the Range header rides
        // along on the shared header dict, so the "retry without the range" is the same request).
        // Starting the file over is the one thing that clears it. Targeted at THIS failure and no
        // other: `--no-continue` everywhere would throw away every legitimate half-done resume.
        Failure::RangeRefused => {
            let mut restarted = false;
            if let Some(fresh) = restarting(&base) {
                // Say what it costs before spending it. The bytes are already on disk and about to
                // be fetched a second time, which on a large file is worth a chance to interrupt.
                let spend = resumed_at(&stdout)
                    .or_else(|| resumed_at(&stderr))
                    .map(|bytes| format!("; the {} already downloaded will be fetched again", roughly(bytes)))
                    .unwrap_or_default();
                println!("{label}: the partial file cannot be resumed — starting it over{spend}");
                restarted = true;
                match capture_ytdlp(fresh) {
                    Some((true, _, _)) => {
                        println!("{label}: succeeded after starting the file over");
                        return None;
                    }
                    // The restart overwrote the stale file and then died of something else. Say so:
                    // the advice below would send the user to delete a `.part` that is no longer
                    // there, and blame the range for a failure that has nothing to do with it.
                    Some((false, _, again)) if classify_failure(&again) != Failure::RangeRefused => {
                        return Some(dead(restarted_into_line(label, error_line(&again))));
                    }
                    _ => {}
                }
            }
            Some(dead(range_refused_line(label, restarted)))
        }
        Failure::Other => Some(dead(other_line(label, error_line(&stderr)))),
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

/// The ledger line for a name the filesystem refused outright. Terminal, and it never suggests a
/// retry: nothing about the site changed, and the same name will be refused every time.
///
/// The title inside every file name is already bounded ([`names::TITLE_BYTES`]), so by the time
/// this fires the length is coming from somewhere this crate does not shorten — which is exactly
/// what the line has to say, since "the name is too long" is not actionable on its own. The
/// candidates, in the order they turn up: a long playlist or channel title expanding into a folder
/// name, a long component in `--into`, or an `-o` template passed through after `--`.
fn name_too_long_line(label: &str, tried_shorter: bool) -> String {
    let limit = names::limit();
    // Only claim the shorter attempt when there was a bound to shrink. The generic path downloads
    // to a name that cannot overrun in the first place, so a refusal there was never about the
    // title and no retry was offered — saying otherwise would send the reader after the wrong
    // thing entirely.
    let already = if tried_shorter { " even at a halved title" } else { "" };
    format!(
        "{label} — the filesystem refused the file name{already}, so a path component is \
         overrunning the {limit}-byte limit in force on something this crate doesn't shorten: a \
         playlist/channel folder name, or a component of --into. If the destination's real limit \
         is below {limit}, pass --name-max <bytes>; otherwise shorten --into, or name the files \
         yourself with a bounded field (`-- -o '%(title).<bytes>B__%(id)s.%(ext)s'`)"
    )
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
        // A name the filesystem refused — the OS talking, not a site. The errno phrasing is what
        // yt-dlp actually emits (verified against a real over-long name); the other two spellings
        // cover the platforms where the number differs or the message is worded the other way.
        for msg in [
            "ERROR: unable to open for writing: [Errno 36] File name too long: '/dl/aaa… [1].mp4.part'",
            "ERROR: [Errno 63] Filename too long",
            "OSError: errno 36",
        ] {
            assert_eq!(classify_failure(msg), Failure::NameTooLong, "{msg}");
        }
        // And it must not swallow a gate that merely mentions a long something.
        assert_eq!(
            classify_failure("ERROR: This video is DRM protected and has a very long title"),
            Failure::Drm,
            "the name check must not shadow the gates"
        );
        // A bare 403 stays Other on purpose — ambiguous (rate-limit vs. bot vs. transient), so we
        // don't mislabel it as a bot-wall.
        assert_eq!(classify_failure("ERROR: HTTP Error 403: Forbidden"), Failure::Other);
        assert_eq!(XFF_REGIONS.len(), 12, "a dozen regions, as specified");
    }

    #[test]
    fn a_refused_byte_range_is_recognised_without_mistaking_a_416_in_a_name_for_one() {
        // The real thing, as the server and yt-dlp phrase it between them.
        assert_eq!(
            classify_failure("ERROR: unable to download video data: HTTP Error 416: Requested Range Not Satisfiable"),
            Failure::RangeRefused
        );
        // Either half stands alone: some servers send the phrase without the number in yt-dlp's
        // wrapping, and some wrappings carry the number without the phrase.
        assert_eq!(classify_failure("ERROR: HTTP Error 416: whatever"), Failure::RangeRefused);
        assert_eq!(classify_failure("ERROR: requested range not satisfiable"), Failure::RangeRefused);
        // And what must NOT match: a bare "416" anywhere in a name or id. This is why the needle is
        // the phrase and `error 416`, not the digits.
        for innocent in [
            "ERROR: [generic] Room 416 Tour: Unable to download webpage",
            "ERROR: unable to open for writing: '/dl/Flight 416.mp4.part'",
        ] {
            assert_eq!(classify_failure(innocent), Failure::Other, "{innocent}");
        }
    }

    #[test]
    fn the_resume_offset_is_read_from_the_downloaders_own_report_of_it() {
        // Exactly as yt-dlp prints it, mid-stream among other lines.
        let output = "[info] x: Downloading 1 format(s): mp4-3882\n                      [download] Resuming download at byte 749233884\n                      [download] Destination: x.mp4\n";
        assert_eq!(resumed_at(output), Some(749_233_884));
        // Trailing punctuation or an end-of-string must not swallow the digits.
        assert_eq!(resumed_at("Resuming download at byte 12"), Some(12));
        assert_eq!(resumed_at("Resuming download at byte 12, continuing"), Some(12));
        // Nothing to read is not an error — the message just goes without the size.
        assert_eq!(resumed_at("[download] Destination: x.mp4"), None);
        assert_eq!(resumed_at("Resuming download at byte "), None);
    }

    #[test]
    fn starting_over_is_offered_once_and_then_admitted_to_be_exhausted() {
        let base: Vec<OsString> = ["-o", "x.mp4", "https://example.com/v"].iter().map(OsString::from).collect();
        let fresh = restarting(&base).expect("a first restart is available");
        assert_eq!(fresh.last().expect("an argument"), "--no-continue");
        assert_eq!(fresh.len(), base.len() + 1, "nothing else is disturbed");
        // Asked again on a command that already gave up resuming, there is nothing left to try —
        // and re-running the identical argv would only fail identically.
        assert!(restarting(&fresh).is_none(), "no second restart");
    }

    #[test]
    fn an_unrescuable_refused_range_names_the_file_the_user_has_to_remove() {
        // The crate cannot delete it — the name is yt-dlp's to choose and the bytes might be
        // wanted — so the least it can do is say what is in the way.
        let after_restart = range_refused_line("clip", true);
        assert!(after_restart.contains("`.part`"), "the culprit is named: {after_restart}");
        assert!(after_restart.contains("delete"), "and the action: {after_restart}");
        assert!(after_restart.contains("did not help"), "honest that a restart was tried: {after_restart}");
        // The other branch can only be reached by a caller that already passed --no-continue, and
        // must not claim a restart was attempted here.
        let never_tried = range_refused_line("clip", false);
        assert!(!never_tried.contains("did not help"), "no invented attempt: {never_tried}");
        assert!(never_tried.contains("delete"), "the action still stands: {never_tried}");
    }

    #[test]
    fn the_403_hint_appears_only_once_a_retry_has_already_failed_and_does_not_oversell_a_new_network() {
        // Reached only from the `Other` arm, which runs after the plain diagnostic re-run — so the
        // hint can afford to talk about a block, which it could not before that re-run.
        let refused = other_line("clip", "ERROR: unable to download: HTTP Error 403: Forbidden");
        assert!(refused.starts_with("clip — failed: ERROR:"), "the ledger line comes first: {refused}");
        assert!(refused.contains("survives a retry"), "it says WHEN it applies: {refused}");
        assert!(refused.contains("--cookies"), "the other lever is named: {refused}");
        // The trap this wording exists to avoid: the obvious remedy hands the site an address it
        // has never seen, and a never-seen address is what draws the first refusal.
        assert!(
            refused.contains("never seen"),
            "switching networks must not be sold as free: {refused}"
        );

        // Anything else is left alone — a hint attached to unrelated failures is noise, and this
        // arm is the catch-all for every failure the crate cannot name.
        for other in ["ERROR: HTTP Error 404: Not Found", "ERROR: ffmpeg exited with code 1"] {
            let line = other_line("clip", other);
            assert_eq!(line, format!("clip — failed: {other}"), "no hint for a non-403");
        }
    }

    #[test]
    fn the_name_too_long_line_says_where_the_length_came_from_and_never_says_retry() {
        // By the time this fires, both free-text fields are bounded AND the halved-title retry has
        // already failed — so the line's whole job is to name what this crate does NOT shorten,
        // and to be honest that trying again unchanged cannot help.
        let line = name_too_long_line("[dQw4w9WgXcQ]", true);
        assert!(ledger_line_refers(&line, "dQw4w9WgXcQ"), "carries the scrub key");
        assert!(!line.contains('\n'), "one line, so scrub_ledger can line-filter it");
        assert!(line.contains(&names::limit().to_string()), "names the limit in force");
        assert!(line.contains("halved"), "says the shorter-name retry was already tried");
        assert!(line.contains("--name-max"), "points at the flag for a smaller real limit");
        for culprit in ["folder name", "--into", "-o"] {
            assert!(line.contains(culprit), "must name {culprit} as a candidate: {line}");
        }
        // And where no bound existed to shrink — the generic path, which is named by its id — the
        // line must not claim a shorter attempt was made, or it sends the reader after the title
        // when the length was never coming from there.
        let untried = name_too_long_line("[dQw4w9WgXcQ]", false);
        assert!(!untried.contains("halved"), "no bound, no claim of one: {untried}");
        assert!(untried.contains("--name-max"), "the real levers are still named: {untried}");
        assert!(
            !line.to_lowercase().contains("retry") && !line.to_lowercase().contains("try again"),
            "the same name will be refused every time — never suggest a retry: {line}"
        );
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
