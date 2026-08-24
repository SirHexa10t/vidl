//! Where cookies come from, and how to tell yt-dlp about them.
//!
//! Two jobs, in two halves:
//!
//! - **Interpreting what the caller asked for** (this file): one `--cookies-use` value, which is
//!   either a Netscape cookies file or a browser to read from, told apart and checked once before
//!   anything downloads.
//! - **Acquiring them** ([`import`]): `--cookies-extract-for-domain <site> <dir>` copies one
//!   site's cookies out of an installed browser into a store of our own, which every later run
//!   on that site then uses.
//!
//! # Why an import exists at all
//!
//! Pointing yt-dlp straight at a live browser profile fails in two ways a person cannot act on: a
//! running browser holds its cookie database locked, and the whole database is every site you have
//! ever signed into. So an import takes a *copy*, pared down to the one site's rows
//! ([`filter`]) — a copy has no lock, and paring it means your banking and email cookies never
//! touch this tool's disk. The copy keeps its values encrypted exactly as the browser wrote them;
//! yt-dlp decrypts through the desktop keyring as it always did.
//!
//! # Two things this crate knows that yt-dlp does not
//!
//! Both learned from a run that failed:
//!
//! 1. **yt-dlp's `--cookies` wants a Netscape *text* file.** Hand it a browser's own database and
//!    yt-dlp reads it as text and reports `'utf-8' codec can't decode byte 0x95 in position 98` —
//!    a true statement about the bytes and no help whatever. The database is perfectly readable,
//!    just not that way: `--cookies-from-browser` opens exactly this format. So a database is
//!    recognised and named as one, with the flag that reads it.
//!
//!    Recognised, and *not* guessed at. The browser name decides which keyring entry holds the
//!    decryption key, and no property of the file can supply it — inferring one from the path would
//!    reproduce the very failure this replaces, on a machine where the guess is wrong. A caller
//!    that imported the cookies knows which browser it read them from and can say so; one that
//!    found a database lying around cannot, and should be told rather than gambled with.
//!
//! 2. **yt-dlp's sandbox paths cover Firefox only.** Its `cookies.py` lists
//!    `~/.var/app/org.mozilla.firefox/…` and nothing else; every Chromium-family browser gets
//!    `$XDG_CONFIG_HOME/…` alone. On a machine where Brave is a Flatpak,
//!    `--cookies-from-browser brave` therefore finds nothing at all. Finding browsers properly is
//!    [`software_inventory::browsers`]'s job — a shared crate, because more than one project wants
//!    the answer — and this defers to it rather than keeping a second, shallower opinion.

pub(crate) mod filter;
pub(crate) mod import;
pub(crate) mod sites;
pub(crate) mod store;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The first bytes of every SQLite file, and so of every browser cookie store.
const SQLITE_MAGIC: &[u8; 15] = b"SQLite format 3";

/// The user's home directory — the root every browser search hangs off. `None` in an environment
/// with no `$HOME`, where there is nothing to search and every caller degrades to "found nothing".
pub(crate) fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|value| !value.is_empty()).map(PathBuf::from)
}

/// Whether `path` is a browser's cookie database rather than a Netscape cookie file.
///
/// By the file's own first bytes, not its name: the browsers call it `Cookies`, `cookies.sqlite` or
/// whatever a copying script chose, and the magic is the only thing all three agree on. A file too
/// short to hold the magic cannot be a database.
fn is_database(path: &Path) -> bool {
    std::fs::read(path)
        .map(|bytes| bytes.len() >= SQLITE_MAGIC.len() && &bytes[..SQLITE_MAGIC.len()] == SQLITE_MAGIC)
        .unwrap_or(false)
}

/// Which of the two things a `--cookies-use` value names. One flag takes both because, to the
/// person typing it, they are one thing — "the cookies I already have" — and which of yt-dlp's two
/// flags carries them is a detail of yt-dlp, not a decision worth making them make.
enum Source<'a> {
    /// A Netscape cookies file.
    File(&'a Path),
    /// A browser to read from: `<browser>[+keyring][:profile-or-dir][::container]`.
    Browser(&'a str),
}

/// Tell the two apart, by the value's first word alone.
///
/// Registry-backed and purely syntactic: no filesystem, so the answer cannot depend on what
/// happens to exist at the moment it is asked, and a browser added to `software_inventory` is
/// recognised here without this function changing. The word ends at a `+` or `:`, which is where
/// yt-dlp's own spec grammar puts the keyring and profile.
///
/// The one ambiguity is a file *named after a browser*. It resolves in the browser's favour,
/// because `--cookies-use firefox` is overwhelmingly a request to read Firefox — and a file really
/// called `firefox` is still reachable as `./firefox`, which no browser name can be.
fn source_of(value: &str) -> Source<'_> {
    let first_word = value.split(['+', ':']).next().unwrap_or(value);
    match software_inventory::browsers::identity_kind(first_word) {
        Some(_) => Source::Browser(value),
        None => Source::File(Path::new(value)),
    }
}

/// The value split into the pair [`crate::Env`] carries, without touching the filesystem — so
/// [`crate::Args::env`] stays total and cheap. Validation is [`resolve`]'s job.
pub(crate) fn split_source(value: Option<&str>) -> (Option<&Path>, Option<&str>) {
    match value.map(source_of) {
        Some(Source::File(path)) => (Some(path), None),
        Some(Source::Browser(spec)) => (None, Some(spec)),
        None => (None, None),
    }
}

/// What a run should use for cookies, once the caller's value has been checked.
pub(crate) enum Cookies {
    /// No cookies: none were asked for, and no import covers this site.
    None,
    /// A Netscape cookies file, confirmed to exist and to be text.
    File(PathBuf),
    /// `--cookies-from-browser <spec>`, possibly pointed at a sandboxed profile yt-dlp would miss.
    FromBrowser(String),
    /// Unusable, and the string says why and what to do about it.
    Unusable(String),
}

/// Work out what to do with the caller's cookie value, once, before anything downloads.
///
/// This is where the filesystem gets consulted — a bad path is reported here rather than as a
/// Python codec error four lines into the first download.
pub(crate) fn resolve(source: Option<&str>) -> Cookies {
    let Some(value) = source else { return Cookies::None };
    match source_of(value) {
        // A browser named with no path of its own: yt-dlp knows where to look unless the install
        // is sandboxed, which its search misses for everything but Firefox.
        Source::Browser(spec) => {
            if !spec.contains(':') {
                if let Some(profile) = home()
                    .and_then(|home| software_inventory::browsers::sandboxed_profile_root(&home, spec))
                {
                    return Cookies::FromBrowser(format!("{spec}:{}", profile.display()));
                }
            }
            Cookies::FromBrowser(spec.to_string())
        }
        Source::File(file) => {
            if !file.exists() {
                return Cookies::Unusable(format!(
                    "no such cookies file: {} (and no browser goes by that name)",
                    file.display()
                ));
            }
            if !is_database(file) {
                return Cookies::File(file.to_path_buf()); // text, which is what a cookies file is
            }
            // A browser's own store. yt-dlp reads these, but only when asked by browser — and the
            // browser is the caller's to name, for the reason in the module header.
            let directory = if file.is_dir() { file } else { file.parent().unwrap_or(file) };
            Cookies::Unusable(format!(
                "{} is a browser's cookie database, not a Netscape cookies file. It is readable — \
                 but only by browser, because the browser name picks the keyring entry holding the \
                 key its values are encrypted with, and nothing about the file itself can say which \
                 browser wrote it. Re-run with `--cookies-use <browser>:{}` ({})",
                file.display(),
                directory.display(),
                software_inventory::browsers::identities().join(", ")
            ))
        }
    }
}

/// The cookie flags for one yt-dlp invocation, from an already-resolved [`crate::Env`].
///
/// An explicit file wins over a browser spec, which is the order the flags are documented in. The
/// resolution above has already turned a database into a spec, so nothing here has to look at the
/// filesystem — this runs for every invocation of a run, and a stat per invocation would be a stat
/// per invocation for no reason.
pub(crate) fn flags(file: Option<&Path>, from_browser: Option<&str>) -> Vec<OsString> {
    if let Some(file) = file {
        vec!["--cookies".into(), file.as_os_str().to_owned()]
    } else if let Some(spec) = from_browser {
        vec!["--cookies-from-browser".into(), spec.into()]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::scratch_dir;

    /// A file whose first bytes are SQLite's, which is all [`is_database`] judges by.
    fn database(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let mut bytes = SQLITE_MAGIC.to_vec();
        bytes.extend_from_slice(b"\0rest of a database");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn a_cookie_store_is_told_from_a_cookie_file_by_its_bytes() {
        let dir = scratch_dir("cookie_kind");
        // The name is no guide: a harvesting script calls it whatever it likes, and Firefox's own
        // store is `cookies.sqlite`. Only the magic is common to all of them.
        assert!(is_database(&database(&dir, "Cookies")));
        assert!(is_database(&database(&dir, "anything.txt")));
        // A Netscape file is text, and its first line says so.
        let text = dir.join("cookies.txt");
        std::fs::write(&text, "# Netscape HTTP Cookie File\n.x\tTRUE\t/\tTRUE\t0\ta\tb\n").unwrap();
        assert!(!is_database(&text));
        // Neither an empty file nor a missing one is a database.
        let empty = dir.join("empty");
        std::fs::write(&empty, b"").unwrap();
        assert!(!is_database(&empty));
        assert!(!is_database(&dir.join("absent")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_database_is_named_as_one_and_never_guessed_at() {
        let dir = scratch_dir("cookie_resolve");
        // Even a path that fairly shouts "brave" is refused: the shout is not evidence, and a wrong
        // guess picks a keyring entry that decrypts nothing — the failure this exists to replace.
        let profile = dir.join(".config/BraveSoftware/Brave-Browser/Default");
        std::fs::create_dir_all(&profile).unwrap();
        for store in [database(&profile, "Cookies"), database(&dir, "harvested")] {
            match resolve(Some(&store.display().to_string())) {
                Cookies::Unusable(why) => {
                    assert!(why.contains("--cookies-use"), "{why}");
                    assert!(why.contains("not a Netscape cookies file"), "{why}");
                    // The DIRECTORY is what yt-dlp looks in, so that is what the advice names.
                    let directory = store.parent().unwrap().display().to_string();
                    assert!(why.contains(&directory), "the advice must be runnable: {why}");
                }
                _ => panic!("a database must be named as one, not acted on"),
            }
        }
        // A real Netscape file is left exactly alone.
        let text = dir.join("cookies.txt");
        std::fs::write(&text, "# Netscape HTTP Cookie File\n").unwrap();
        assert!(matches!(resolve(Some(&text.display().to_string())), Cookies::File(_)));
        // As is a missing one — but by a different route, with its own message.
        assert!(matches!(
            resolve(Some(&dir.join("nope").display().to_string())),
            Cookies::Unusable(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_browser_spec_that_already_names_a_path_is_never_second_guessed() {
        // The caller said exactly where to look. Overriding that would be the one thing worse than
        // not finding a sandboxed profile.
        match resolve(Some("brave:/somewhere/else")) {
            Cookies::FromBrowser(spec) => assert_eq!(spec, "brave:/somewhere/else"),
            _ => panic!("an explicit profile path is passed through untouched"),
        }
        assert!(matches!(resolve(None), Cookies::None));
    }

    /// The single `--cookies-use` value has to land on the right one of yt-dlp's two flags, and
    /// the rule is the value's first word — no filesystem, so the answer never depends on what
    /// happens to be lying around.
    #[test]
    fn one_flag_tells_a_browser_from_a_file_by_its_first_word() {
        let browser = |value: &str| matches!(source_of(value), Source::Browser(_));
        // Bare names, and every shape of yt-dlp's spec grammar, are browsers.
        for spec in ["firefox", "brave", "chromium:Default", "firefox+gnomekeyring", "edge:/p::c"] {
            assert!(browser(spec), "{spec} names a browser");
        }
        // Anything whose first word is not a browser is a path — including a lookalike.
        for path in ["/tmp/cookies.txt", "cookies.txt", "./firefox", "firefoxen", "~/c.txt", ""] {
            assert!(!browser(path), "{path} is a path");
        }
        // The escape hatch is real: a file named after a browser is still reachable.
        assert!(!browser("./firefox") && browser("firefox"), "`./` disambiguates");
    }

    /// An unknown name is a *path* now, not a browser — so it fails as a missing file, and the
    /// message says both halves rather than leaving the user to guess which the tool thought it was.
    #[test]
    fn an_unknown_name_is_refused_as_neither_browser_nor_file() {
        match resolve(Some("netscape")) {
            Cookies::Unusable(why) => {
                assert!(why.contains("no such cookies file"), "{why}");
                assert!(why.contains("no browser goes by that name"), "{why}");
            }
            _ => panic!("a name that is neither must be refused, not passed to yt-dlp"),
        }
    }

    /// The cross-crate contract: `software_inventory` names browsers by a canonical identity, and
    /// every one of those has to be a name yt-dlp will accept — a store found under an identity
    /// yt-dlp does not know is a store nothing can read. This is the seam where a browser added
    /// upstream would otherwise fail silently, at import time, on someone's machine.
    #[test]
    fn every_identity_the_registry_can_hand_us_is_one_ytdlp_reads() {
        // yt-dlp's SUPPORTED_BROWSERS, minus the ones no Linux family maps to.
        const READS: &[&str] =
            &["brave", "chrome", "chromium", "edge", "firefox", "opera", "safari", "vivaldi", "whale"];
        for family in software_inventory::browsers::FAMILIES {
            assert!(
                READS.contains(&family.identity),
                "{} claims identity `{}`, which yt-dlp does not read",
                family.label,
                family.identity
            );
        }
    }

    #[test]
    fn the_flags_put_an_explicit_file_ahead_of_a_browser() {
        let file = PathBuf::from("/c.txt");
        assert_eq!(flags(Some(&file), Some("brave")), ["--cookies", "/c.txt"].map(OsString::from));
        assert_eq!(
            flags(None, Some("brave:/p")),
            ["--cookies-from-browser", "brave:/p"].map(OsString::from)
        );
        assert!(flags(None, None).is_empty());
    }
}
