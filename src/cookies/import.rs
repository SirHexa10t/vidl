//! `--cookies-extract-for-domain <target> <dir>`: copy one site's cookies out of an
//! installed browser.
//!
//! The whole setup step, start to finish — resolve the target site, find the browsers, let the
//! user pick one, pare its cookie DB to that site alone, and prove the result is readable before
//! claiming success. Every later run on the same site then finds the store by itself
//! ([`spec_for`]).
//!
//! Nothing here is silent about failure. An import that produced no cookies, or cookies nothing
//! can decrypt, is rolled back rather than left to make every future download attempt the same
//! doomed read.

use std::io::Write;
use std::path::Path;

use software_inventory::browsers::{self, CookieStore};

use super::sites::{self, SiteTarget};
use super::{filter, store};
use crate::util::style;

/// Do the `--cookies-extract-for-domain` action: write `target`'s cookies into a per-site
/// directory under `into`, and say whether a usable store came out of it. `target` is a site
/// keyword, a domain, or a URL.
///
/// `auto_read_root` is where *later* runs will go looking. It is not where anything is written —
/// it is passed only so that an import into some other directory can say so, instead of reporting
/// success for a store nothing will ever pick up.
pub(crate) fn run(target: &str, into: &Path, auto_read_root: Option<&Path>) -> bool {
    // Reject a target we can't map to cookie domains (a bare word like `tiktok2`) up front —
    // otherwise we'd build an empty store and report a misleading "no cookies found".
    if !sites::is_importable_target(target) {
        eprintln!(
            "vidl: '{target}' isn't a site I can target — pass a keyword ({}), a domain (example.com), or a video URL",
            sites::known_site_names()
        );
        return false;
    }
    let site = sites::resolve_target(target);
    let Some(home) = super::home() else {
        eprintln!("vidl: no $HOME, so there is nowhere to look for browsers");
        return false;
    };
    let stores = browsers::cookie_stores(&home);
    if stores.is_empty() {
        if browsers::any_browser_installed(&home) {
            eprintln!("vidl: found browsers but no cookie stores yet — browse (and sign in) once, then retry");
        } else {
            eprintln!("vidl: no browser cookie stores found on this system");
        }
        return false;
    }
    let Some(chosen) = pick(&stores, &site.label) else { return false };
    let site_dir = store::site_dir(into, &site.key);
    let store_dir = match store::reset_site(&site_dir) {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("vidl: could not prepare the {} cookie store: {err}", site.label);
            return false;
        }
    };
    // Cherry-pick: only the target site's cookies are copied — never the whole DB.
    let Some(matched) = filter::filter_cookie_db(chosen, &store_dir, &site.domains) else {
        let _ = store::forget(&site_dir);
        eprintln!("vidl: could not filter the cookie store (is python available?)");
        return false;
    };
    if matched == 0 {
        let _ = store::forget(&site_dir);
        eprintln!(
            "vidl: no {} cookies in {} — sign in to {} in that browser/profile, then re-import",
            site.label, chosen.label, site.label
        );
        return false;
    }
    if let Err(err) = store::write_spec(&site_dir, chosen.identity) {
        let _ = store::forget(&site_dir);
        eprintln!("vidl: could not record the {} cookie store: {err}", site.label);
        return false;
    }
    println!("imported {matched} {} cookie(s) from {}", site.label, chosen.label);
    if site.key == "youtube" {
        println!(
            "note: YouTube rotates cookies on open tabs — if auth later fails, re-export via a \
             private window (sign in, open only youtube.com/robots.txt, then close it) and re-import"
        );
    }
    let usable = report_check(&site, chosen.identity, matched, &site_dir);
    if usable && auto_read_root != Some(into) {
        // Written somewhere a download run won't think to look, so the spec that does work is
        // printed rather than left for the user to reconstruct from the layout.
        if let Some(spec) = store::imported_spec(&site_dir) {
            let looks_in = auto_read_root
                .map_or_else(|| "nowhere (no $HOME)".to_string(), |root| root.display().to_string());
            println!(
                "note: downloads look for stores under {looks_in}, not here — use this one with \
                 `--cookies-from-browser {spec}`"
            );
        }
    }
    usable
}

/// The `--cookies-from-browser` spec for `url`'s site, when a prior import covers it — and a
/// warning first if what was imported has since expired.
///
/// This is what makes an import a one-off: every later run on the same site picks the store up
/// without being told. `None` when nothing has been imported for that site, which simply leaves
/// the run anonymous.
pub(crate) fn spec_for(root: &Path, url: &str) -> Option<String> {
    let site = sites::resolve_target(url);
    let site_dir = store::site_dir(root, &site.key);
    let spec = store::imported_spec(&site_dir)?;
    warn_if_expired(&site_dir, &site);
    Some(spec)
}

/// Warn (in red) when the imported store a download is about to use holds only expired cookies,
/// so a gated fetch that's about to fail for a stale session says so up front — with the
/// re-import fix — rather than surfacing as a bare auth error. Silent when the store is fresh or
/// the check can't run.
fn warn_if_expired(site_dir: &Path, site: &SiteTarget) {
    let Some((db, kind)) = store::imported_db(site_dir) else { return };
    if filter::cookies_expired(&db, kind) == Some(true) {
        eprintln!(
            "{}",
            style::problematic(&format!(
                "vidl: the imported {} cookies have expired — extract them again with `--cookies-extract-for-domain {}`",
                site.label, site.key
            ))
        );
    }
}

/// Read the freshly imported (already domain-filtered) store back through yt-dlp to confirm
/// the cookies actually *decrypt* — a keyring-locked Chromium store filters fine but reads
/// back empty — and report the concrete result, catching that failure now instead of on
/// every later run. Returns whether a usable store remains. When the read-back can't run
/// (no yt-dlp yet), keeps the store on the provisional count rather than overclaiming.
fn report_check(site: &SiteTarget, identity: &str, matched: usize, site_dir: &Path) -> bool {
    // The WAL caveat holds whenever cookies came through: a copy captures only what the
    // browser already flushed to the DB, so a sign-in from moments ago may not be in it yet.
    let wal_note =
        || println!("note: if a fresh sign-in isn't recognized, fully quit the browser and re-import");
    let Some(spec) = store::imported_spec(site_dir) else { return true };
    match filter::readable_cookie_count(&spec, site_dir) {
        Some(n) if n > 0 => {
            println!(
                "{}",
                style::approved(&format!(
                    "validated — {n} {} cookie(s) readable; runs on {} will use them",
                    site.label, site.label
                ))
            );
            wal_note();
            true
        }
        Some(_) => {
            // Filtered rows but none decrypt → useless; drop it so future runs don't keep
            // re-attempting a doomed read (a locked Chromium keyring can even prompt).
            let _ = store::forget(site_dir);
            if identity == "firefox" {
                eprintln!(
                    "vidl: {matched} {} cookie(s) imported but none could be read — the profile DB may be unreadable; re-import",
                    site.label
                );
            } else {
                eprintln!(
                    "vidl: {matched} {} cookie(s) imported but none decrypted — Chromium needs the desktop keyring unlocked; a Firefox store is more reliable. Import discarded.",
                    site.label
                );
            }
            false
        }
        None => {
            println!(
                "(couldn't verify readability — is yt-dlp installed? the {matched} imported cookie(s) will be tried on later runs)"
            );
            wal_note();
            true
        }
    }
}

/// Prompt for one of `stores` to import `site_label`'s cookies from (auto-selecting a lone
/// candidate).
fn pick<'a>(stores: &'a [CookieStore], site_label: &str) -> Option<&'a CookieStore> {
    if let [only] = stores {
        println!("one cookie store found — importing {site_label} cookies from {}", only.label);
        return Some(only);
    }
    println!("{}", style::header(&format!("Import {site_label} cookies from:")));
    for (i, store) in stores.iter().enumerate() {
        println!("  {}) {}", i + 1, store.label);
    }
    print!("choice [1-{}]: ", stores.len());
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok()?;
    match line.trim().parse::<usize>().ok().filter(|n| (1..=stores.len()).contains(n)) {
        Some(n) => Some(&stores[n - 1]),
        None => {
            eprintln!("vidl: not a listed number");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::scratch_dir;

    /// An unresolvable target is refused before any directory is created — the check that keeps a
    /// typo from producing an empty store and a misleading "no cookies found".
    #[test]
    fn an_unresolvable_target_is_refused_without_touching_the_root() {
        let root = scratch_dir("import_bad_target");
        assert!(!run("tiktok2", &root, Some(&root)), "a bare non-keyword word is not a site");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0, "nothing was written");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The read side, without any browser on the machine: a site with no import is simply
    /// anonymous, and one with an import hands back its spec.
    #[test]
    fn spec_for_finds_only_what_was_imported_for_that_site() {
        let root = scratch_dir("import_specfor");
        assert_eq!(spec_for(&root, "https://www.youtube.com/watch?v=x"), None, "nothing imported");

        let site_dir = store::site_dir(&root, "youtube");
        std::fs::write(store::reset_site(&site_dir).unwrap().join("cookies.sqlite"), "db").unwrap();
        store::write_spec(&site_dir, "firefox").unwrap();

        let spec = spec_for(&root, "https://www.youtube.com/watch?v=x").expect("the youtube store");
        assert!(spec.starts_with("firefox:"), "{spec}");
        assert!(spec.ends_with("youtube/store"), "{spec}");
        // A different site is unaffected — the per-site dirs are the isolation.
        assert_eq!(spec_for(&root, "https://www.tiktok.com/@u/video/1"), None);
        // And a YouTube URL by any of its addresses reaches the same store.
        assert!(spec_for(&root, "https://youtu.be/x").is_some(), "aliases route to the same store");
        let _ = std::fs::remove_dir_all(&root);
    }
}
