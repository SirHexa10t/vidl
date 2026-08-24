//! The on-disk shape of an imported cookie store.
//!
//! One directory per site under a *cookie root*, each holding the pared-down copy and a marker
//! recording which browser identity it came from:
//!
//! ```text
//! <root>/<site key>/store/cookies.sqlite   the filtered copy (or `Cookies`, for Chromium)
//! <root>/<site key>/browser.spec           one line: the identity that decrypts it
//! ```
//!
//! Per-site directories are the privacy boundary made structural: importing tiktok cannot disturb
//! or read the youtube store, and forgetting one leaves the others alone. The `store/` nesting is
//! what lets the whole directory be handed to `--cookies-from-browser` — yt-dlp searches it
//! recursively, so the spec is `<identity>:<dir>` for every family regardless of the DB's name.

use std::path::{Path, PathBuf};

use software_inventory::browsers::{Kind, identity_kind};

/// Where the imported copy sits inside a site's directory.
const STORE_SUBDIR: &str = "store";
/// The one-line marker recording which browser identity the copy came from.
const SPEC_FILE: &str = "browser.spec";

/// The default cookie root when no caller names one: `$XDG_DATA_HOME/vidl/browser_cookies`, else
/// `~/.local/share/vidl/browser_cookies`.
///
/// Data, not cache — a cookie store is not regenerable without the user signing in again, so it
/// does not belong anywhere a cleaner may reap it. An embedder with its own data directory passes
/// that instead and never reaches this.
pub(crate) fn default_root() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => super::home()?.join(".local/share"),
    };
    Some(base.join("vidl").join("browser_cookies"))
}

/// One site's directory under `root`. The key is a curated site name or a bare registrable
/// domain — a plain path segment either way (pinned by a test in [`super::sites`]).
pub(crate) fn site_dir(root: &Path, key: &str) -> PathBuf {
    root.join(key)
}

/// Clear `site_dir` and create its empty `store/`, returned ready for a filtered import to write
/// the pared-down cookie DB into. Wiping first means a re-import of a site never mingles with the
/// previous one (and switching browsers for a site replaces cleanly).
pub(crate) fn reset_site(site_dir: &Path) -> std::io::Result<PathBuf> {
    let _ = std::fs::remove_dir_all(site_dir); // a stale prior import must not linger
    let store_dir = site_dir.join(STORE_SUBDIR);
    std::fs::create_dir_all(&store_dir)?;
    Ok(store_dir)
}

/// Record which browser identity a site's imported store came from, in `site_dir/browser.spec` —
/// the marker [`imported_spec`] reads to build the `--cookies-from-browser` spec.
pub(crate) fn write_spec(site_dir: &Path, identity: &str) -> std::io::Result<()> {
    std::fs::write(site_dir.join(SPEC_FILE), identity)
}

/// Drop a previously imported store — the copied DB and the spec marker both. Used to roll back
/// an import that read back as entirely unusable (e.g. a keyring-locked Chromium store yielding
/// zero cookies): leaving it would make every future run attempt the same doomed read, so
/// forgetting it keeps later runs clean until a working store is imported. Idempotent.
pub(crate) fn forget(site_dir: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(site_dir.join(SPEC_FILE));
    match std::fs::remove_dir_all(site_dir.join(STORE_SUBDIR)) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// The `--cookies-from-browser` spec for a previously imported store (`<identity>:<abs dir>`), or
/// `None` when nothing has been imported. Consulted on every run. yt-dlp's own recursive search
/// locates the DB within the dir, so the spec is the same for every family.
pub(crate) fn imported_spec(site_dir: &Path) -> Option<String> {
    let identity = std::fs::read_to_string(site_dir.join(SPEC_FILE)).ok()?;
    let identity = identity.trim();
    let store_dir = site_dir.join(STORE_SUBDIR);
    let has_files = std::fs::read_dir(&store_dir).ok()?.flatten().next().is_some();
    (!identity.is_empty() && has_files).then(|| format!("{identity}:{}", store_dir.display()))
}

/// The imported store's cookie DB file and its database shape, or `None` when nothing's imported.
/// The store dir holds exactly one file (the filtered DB), so we take it; the shape comes from the
/// recorded identity, resolved through the browser registry rather than guessed at here — an
/// identity it does not recognise yields `None` instead of a default that would read the wrong
/// table. Lets a caller read the DB's plaintext columns (cookie expiry) without yt-dlp or
/// decryption. Sibling of [`imported_spec`].
pub(crate) fn imported_db(site_dir: &Path) -> Option<(PathBuf, Kind)> {
    let identity = std::fs::read_to_string(site_dir.join(SPEC_FILE)).ok()?;
    let kind = identity_kind(identity.trim())?;
    let db = std::fs::read_dir(site_dir.join(STORE_SUBDIR))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.is_file())?;
    Some((db, kind))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::scratch_dir;

    #[test]
    fn store_layout_round_trips_a_uniform_spec_and_reset_wipes_it() {
        let root = scratch_dir("cookie_layout");
        let site = site_dir(&root, "youtube");
        assert!(imported_spec(&site).is_none(), "nothing imported yet");

        let store_dir = reset_site(&site).unwrap();
        std::fs::write(store_dir.join("cookies.sqlite"), "db").unwrap(); // stand-in for a filtered DB
        write_spec(&site, "firefox").unwrap();
        let spec = imported_spec(&site).expect("a spec once a store exists");
        assert!(spec.starts_with("firefox:") && spec.ends_with("youtube/store"), "{spec}");

        // A re-import wipes the whole site dir first — a stale DB from a prior browser can't linger.
        let store_dir2 = reset_site(&site).unwrap();
        assert_eq!(std::fs::read_dir(&store_dir2).unwrap().count(), 0, "store emptied on reset");
        assert!(imported_spec(&site).is_none(), "spec gone until re-written");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn per_site_dirs_keep_different_sites_and_browsers_apart() {
        // A user can import youtube from Firefox and tiktok from Chrome — the two stores coexist
        // in their own subdirs and each resolves independently.
        let root = scratch_dir("cookie_persite");
        let yt = site_dir(&root, "youtube");
        let tk = site_dir(&root, "tiktok");
        std::fs::write(reset_site(&yt).unwrap().join("cookies.sqlite"), "ff").unwrap();
        write_spec(&yt, "firefox").unwrap();
        std::fs::write(reset_site(&tk).unwrap().join("Cookies"), "cr").unwrap();
        write_spec(&tk, "chrome").unwrap();
        assert!(imported_spec(&yt).unwrap().starts_with("firefox:"));
        assert!(imported_spec(&tk).unwrap().starts_with("chrome:"));
        forget(&yt).unwrap();
        assert!(imported_spec(&yt).is_none(), "forgetting youtube leaves tiktok alone");
        assert!(imported_spec(&tk).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn forget_removes_the_store_and_spec_and_is_idempotent() {
        let root = scratch_dir("cookie_forget");
        let site = site_dir(&root, "vimeo");
        std::fs::write(reset_site(&site).unwrap().join("cookies.sqlite"), "db").unwrap();
        write_spec(&site, "firefox").unwrap();
        assert!(imported_spec(&site).is_some());

        forget(&site).unwrap();
        assert!(imported_spec(&site).is_none(), "spec + store gone after forget");
        assert!(!site.join(STORE_SUBDIR).exists());
        forget(&site).unwrap(); // idempotent: a second forget is a clean no-op
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn imported_db_finds_the_store_file_and_its_family() {
        let root = scratch_dir("cookie_impdb");
        let site = site_dir(&root, "tiktok");
        assert!(imported_db(&site).is_none(), "nothing imported yet");
        let store_dir = reset_site(&site).unwrap();
        std::fs::write(store_dir.join("cookies.sqlite"), "db").unwrap();
        write_spec(&site, "firefox").unwrap();
        let (db, kind) = imported_db(&site).expect("a db once imported");
        assert!(db.ends_with("cookies.sqlite"), "{db:?}");
        assert_eq!(kind, Kind::Firefox, "the shape comes from the registry, not a string guess");

        // A spec naming a browser the registry doesn't know reads as nothing, rather than
        // defaulting to Chromium and then querying a table the file does not have.
        write_spec(&site, "netscape").unwrap();
        assert!(imported_db(&site).is_none(), "an unknown identity is refused, not defaulted");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A half-written import must not read as a working one: the spec and the store file are two
    /// separate writes, and a crash between them (or a wiped store dir) leaves one without the
    /// other. Either alone means "nothing imported", so a run falls back to anonymous rather than
    /// handing yt-dlp a spec pointing at an empty directory.
    #[test]
    fn a_spec_without_a_store_and_a_store_without_a_spec_both_read_as_nothing() {
        let root = scratch_dir("cookie_partial");

        let spec_only = site_dir(&root, "spec-only");
        reset_site(&spec_only).unwrap(); // creates an EMPTY store dir
        write_spec(&spec_only, "firefox").unwrap();
        assert!(imported_spec(&spec_only).is_none(), "a spec over an empty store is not an import");

        let store_only = site_dir(&root, "store-only");
        std::fs::write(reset_site(&store_only).unwrap().join("cookies.sqlite"), "db").unwrap();
        assert!(imported_spec(&store_only).is_none(), "a store with no spec names no browser");
        assert!(imported_db(&store_only).is_none());

        // And an empty spec file is no better than a missing one.
        let blank = site_dir(&root, "blank");
        std::fs::write(reset_site(&blank).unwrap().join("cookies.sqlite"), "db").unwrap();
        write_spec(&blank, "  ").unwrap();
        assert!(imported_spec(&blank).is_none(), "whitespace names no browser");
        let _ = std::fs::remove_dir_all(&root);
    }
}
