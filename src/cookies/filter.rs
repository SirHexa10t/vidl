//! Pare a browser cookie database down to one site's rows, and check what came out.
//!
//! Three reads of a cookie DB, none of which decrypt anything: the filtered copy an import writes
//! ([`filter_cookie_db`]), the expiry sweep that warns before a stale session fails a download
//! ([`cookies_expired`]), and the read-back through yt-dlp that proves the copy is usable at all
//! ([`readable_cookie_count`]).
//!
//! # Why python
//!
//! The first two need an SQLite reader and nothing else. The Rust one (`rusqlite`) means the
//! bundled SQLite C amalgamation — measured at +14 crates, +2 MB of binary and a C toolchain this
//! crate otherwise does not need — to run three queries. Python's standard library carries
//! `sqlite3`, and every install that runs yt-dlp as anything but a frozen binary already has an
//! interpreter; an embedder that pins its own passes it through [`crate::tools::install`]. The
//! cost of the choice is stated plainly: on a machine with a standalone yt-dlp binary and no
//! python at all, `--cookies-extract-for-domain` reports that it cannot run and nothing else
//! is affected.

use std::ffi::OsString;
use std::path::Path;

use software_inventory::browsers::{CookieStore, Kind};

use crate::util::exec::capture_output;

/// Pare a browser's cookie DB down to only `domains`' cookies, writing the filtered copy into
/// `store_dir`. The privacy crux: an import keeps *just* the target site's cookies, never the
/// whole DB (your banking/email cookies never touch this tool's disk). Done without decrypting
/// anything — the domain column (`host` / `host_key`) is plaintext in both families, so we filter
/// rows on it and copy the (still-encrypted, for Chromium) values through verbatim, leaving yt-dlp
/// to decrypt via the keyring exactly as before. Returns the number of cookies kept, or `None` if
/// the filter couldn't run. Reads the source read-only + lock-ignoring, so a running browser is
/// fine (it sees the checkpointed DB — the same WAL caveat the import message spells out).
pub(crate) fn filter_cookie_db(
    store: &CookieStore,
    store_dir: &Path,
    domains: &[String],
) -> Option<usize> {
    let (src, dest_name) = &store.files[0];
    let dest = store_dir.join(dest_name);
    let kind = shape(store.kind);
    let mut argv: Vec<OsString> = vec![
        "-c".into(), COOKIE_FILTER.into(),
        src.as_os_str().to_owned(), dest.into_os_string(), kind.into(),
    ];
    argv.extend(domains.iter().map(OsString::from));
    let (ok, out, _err) = capture_output(crate::tools::python(), argv)?;
    ok.then(|| out.trim().parse().ok()).flatten()
}

/// The token the embedded scripts read as a table shape. This is the scripts' own wire format —
/// how one snippet is told which of its two branches to take — not a fact about browsers. The
/// fact is [`Kind`], and it comes from the registry.
fn shape(kind: Kind) -> &'static str {
    match kind {
        Kind::Firefox => "firefox",
        Kind::Chromium => "chromium",
    }
}

/// The cookie-DB filter, embedded python (the standard library carries sqlite3). Recreates the
/// source table's exact schema in the destination, copies only the rows whose host matches a target
/// domain (equal or a dot-boundary subdomain), and carries the version metadata yt-dlp needs to
/// read the result — Firefox's `PRAGMA user_version` (cookie-expiry units) and Chromium's `meta`
/// table (encryption version). Prints the kept-row count. Flush-left on purpose: a raw literal
/// keeps python's indentation — a `\`-continued Rust string strips the leading whitespace and
/// hands python an IndentationError.
const COOKIE_FILTER: &str = r#"import sqlite3, sys
src, dst, kind = sys.argv[1], sys.argv[2], sys.argv[3]
domains = [d.lower() for d in sys.argv[4:]]
def keep(host):
    h = (host or "").lstrip(".").lower()
    return any(h == d or h.endswith("." + d) for d in domains)
table, hostcol = ("moz_cookies", "host") if kind == "firefox" else ("cookies", "host_key")
s = sqlite3.connect("file:%s?immutable=1" % src, uri=True)
d = sqlite3.connect(dst)
schema = s.execute("SELECT sql FROM sqlite_master WHERE type='table' AND name=?", (table,)).fetchone()
if not schema:
    print(0); sys.exit(0)
d.execute(schema[0])
cols = [r[1] for r in s.execute("PRAGMA table_info(%s)" % table)]
hi = cols.index(hostcol)
kept = [r for r in s.execute("SELECT * FROM %s" % table) if keep(r[hi])]
d.executemany("INSERT INTO %s VALUES (%s)" % (table, ",".join("?" * len(cols))), kept)
if kind == "firefox":
    d.execute("PRAGMA user_version=%d" % s.execute("PRAGMA user_version").fetchone()[0])
else:
    meta = s.execute("SELECT sql FROM sqlite_master WHERE type='table' AND name='meta'").fetchone()
    if meta:
        d.execute(meta[0])
        d.executemany("INSERT INTO meta VALUES (?,?)", list(s.execute("SELECT key, value FROM meta")))
d.commit()
print(len(kept))
"#;

/// Whether the imported store's cookies have all expired — `Some(true)` if every persistent cookie's
/// expiry is in the past, `Some(false)` if any is still live (or there are only session cookies,
/// whose death isn't a timestamp), `None` if the check couldn't run. Reads the expiry column
/// straight from the store DB via python's sqlite3 (like [`filter_cookie_db`]) — expiry is
/// plaintext in both families, so no decryption and no yt-dlp.
pub(crate) fn cookies_expired(db: &Path, kind: Kind) -> Option<bool> {
    let argv: Vec<OsString> =
        vec!["-c".into(), COOKIE_EXPIRY.into(), db.as_os_str().to_owned(), shape(kind).into()];
    let (ok, out, _err) = capture_output(crate::tools::python(), argv)?;
    if !ok {
        return None;
    }
    match out.trim() {
        "expired" => Some(true),
        "live" => Some(false),
        _ => None,
    }
}

/// Prints `expired` (every persistent cookie is in the past) or `live`. Firefox `moz_cookies.expiry`
/// is unix seconds; Chromium `cookies.expires_utc` is microseconds since 1601-01-01, converted to
/// unix. Session cookies (0/null expiry) are skipped — their death isn't a timestamp — so a store of
/// only session cookies reads `live`, never a false `expired`. Flush-left raw literal to keep
/// python's indentation (see [`COOKIE_FILTER`]).
const COOKIE_EXPIRY: &str = r#"import sqlite3, sys, time
db, kind = sys.argv[1], sys.argv[2]
table, col = ("moz_cookies", "expiry") if kind == "firefox" else ("cookies", "expires_utc")
con = sqlite3.connect("file:%s?immutable=1" % db, uri=True)
exps = [r[0] for r in con.execute("SELECT %s FROM %s" % (col, table)) if r[0]]
if kind != "firefox":
    exps = [e / 1000000.0 - 11644473600 for e in exps]
print("expired" if exps and max(exps) < time.time() else "live")
"#;

/// Have yt-dlp read the imported (already domain-filtered) store back and count what decrypts, so
/// an import can confirm the store is actually usable *before* every future run leans on it.
/// Runs fully offline — no URL, so yt-dlp extracts the cookies to a throwaway Netscape file and
/// quits without fetching — yet it drives the real decryption path, so a keyring-locked Chromium
/// store reports `0` here instead of failing quietly on the first download. The throwaway file
/// (decrypted cookies, briefly) is removed immediately. `None` when the check couldn't run (no
/// yt-dlp, or the read crashed before writing) — the caller then keeps its provisional count
/// rather than claiming a verified one.
pub(crate) fn readable_cookie_count(spec: &str, site_dir: &Path) -> Option<usize> {
    let probe = site_dir.join(".probe-cookies.txt");
    let _ = std::fs::remove_file(&probe); // clear any stale probe an interrupted run left behind
    let argv: Vec<OsString> = vec![
        "--cookies-from-browser".into(),
        spec.into(),
        "--cookies".into(),
        probe.clone().into_os_string(),
    ];
    crate::ask_ytdlp(argv)?; // None only if yt-dlp couldn't be launched at all
    let dump = std::fs::read_to_string(&probe).ok();
    let _ = std::fs::remove_file(&probe); // the decrypted dump must not linger
    Some(count_cookie_dump(&dump?))
}

/// Count the cookie rows in a Netscape dump (`#HttpOnly_` lines are cookies too; only genuine
/// comments and blanks are skipped). Split out so it's testable without yt-dlp on disk.
fn count_cookie_dump(dump: &str) -> usize {
    dump.lines()
        .map(|line| line.strip_prefix("#HttpOnly_").unwrap_or(line))
        .filter(|entry| !entry.starts_with('#') && !entry.trim().is_empty())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::scratch_dir;

    /// Skip-with-notice: the resolved python when it can do sqlite work, else `None` after
    /// printing a visible SKIP line — the test then passes vacuously instead of failing on a
    /// machine with no python.
    fn python_or_skip(test: &str) -> Option<OsString> {
        let python = crate::tools::python();
        let works = std::process::Command::new(&python)
            .args(["-c", "import sqlite3"])
            .output()
            .is_ok_and(|out| out.status.success());
        if !works {
            eprintln!("SKIPPED {test}: no usable python (with sqlite3) available");
        }
        works.then_some(python)
    }

    /// Run a python snippet with `args`, returning its stdout (asserting it succeeded).
    fn py(python: &std::ffi::OsStr, code: &str, args: &[&std::ffi::OsStr]) -> String {
        let out = std::process::Command::new(python)
            .arg("-c")
            .arg(code)
            .args(args)
            .output()
            .expect("run python");
        assert!(out.status.success(), "python failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn the_cookie_filter_keeps_only_target_domain_rows_and_carries_read_metadata() {
        // The feature's privacy contract: an import copies the target site's rows — dot-hosts,
        // the bare domain, true subdomains — and NOTHING else; lookalike hosts stay behind. The
        // version metadata each family needs to read the copy travels with it.
        let Some(python) = python_or_skip("cookie filter") else { return };
        let dir = scratch_dir("cookiefilter");
        let domains = vec!["tiktok.com".to_string()];

        // Firefox family: `moz_cookies.host`, expiry units recorded in `PRAGMA user_version`.
        let src = dir.join("cookies.sqlite");
        py(
            &python,
            r#"import sqlite3, sys, time
con = sqlite3.connect(sys.argv[1])
con.execute("CREATE TABLE moz_cookies (id INTEGER PRIMARY KEY, host TEXT, name TEXT, value TEXT, expiry INTEGER)")
exp = int(time.time()) + 86400
rows = [(".tiktok.com", "keep_dot"), ("tiktok.com", "keep_bare"), ("sub.tiktok.com", "keep_sub"),
        ("evil.com", "other_site"), ("xtiktok.com", "suffix_lookalike"), ("tiktok.com.evil.net", "prefix_lookalike")]
con.executemany("INSERT INTO moz_cookies (host, name, value, expiry) VALUES (?, ?, 'v', %d)" % exp, rows)
con.execute("PRAGMA user_version = 10")
con.commit()"#,
            &[src.as_os_str()],
        );
        let store_dir = dir.join("ff_store");
        std::fs::create_dir_all(&store_dir).unwrap();
        let store = CookieStore {
            label: "test firefox".into(),
            identity: "firefox",
            kind: Kind::Firefox,
            files: vec![(src, "cookies.sqlite")],
        };
        assert_eq!(filter_cookie_db(&store, &store_dir, &domains), Some(3), "dot + bare + subdomain, nothing else");
        let read = py(
            &python,
            r#"import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
print(con.execute("PRAGMA user_version").fetchone()[0])
for (n,) in con.execute("SELECT name FROM moz_cookies ORDER BY name"):
    print(n)"#,
            &[store_dir.join("cookies.sqlite").as_os_str()],
        );
        assert_eq!(read, "10\nkeep_bare\nkeep_dot\nkeep_sub\n", "rows filtered, user_version carried");

        // Chromium family: `cookies.host_key`, encryption version in the `meta` table.
        let src = dir.join("Cookies");
        py(
            &python,
            r#"import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
con.execute("CREATE TABLE cookies (creation_utc INTEGER, host_key TEXT, name TEXT, encrypted_value BLOB)")
con.execute("CREATE TABLE meta (key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, value LONGVARCHAR)")
con.execute("INSERT INTO meta VALUES ('version', '24')")
rows = [(".tiktok.com", "keep_dot"), ("www.tiktok.com", "keep_www"), ("accounts.evil.com", "other_site")]
con.executemany("INSERT INTO cookies (creation_utc, host_key, name, encrypted_value) VALUES (0, ?, ?, x'76')", rows)
con.commit()"#,
            &[src.as_os_str()],
        );
        let store_dir = dir.join("cr_store");
        std::fs::create_dir_all(&store_dir).unwrap();
        let store = CookieStore {
            label: "test chrome".into(),
            identity: "chrome",
            kind: Kind::Chromium,
            files: vec![(src, "Cookies")],
        };
        assert_eq!(filter_cookie_db(&store, &store_dir, &domains), Some(2));
        let read = py(
            &python,
            r#"import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
print(con.execute("SELECT value FROM meta WHERE key='version'").fetchone()[0])
for (n,) in con.execute("SELECT name FROM cookies ORDER BY name"):
    print(n)"#,
            &[store_dir.join("Cookies").as_os_str()],
        );
        assert_eq!(read, "24\nkeep_dot\nkeep_www\n", "rows filtered, meta carried");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_expiry_probe_reads_both_epochs_and_never_cries_wolf_on_session_cookies() {
        let Some(python) = python_or_skip("cookie expiry") else { return };
        let dir = scratch_dir("cookieexpiry");
        // One builder for both families: rows are `past`/`future`/`session`, converted to the
        // family's epoch (firefox: unix seconds; chromium: microseconds since 1601).
        let builder = r#"import sqlite3, sys, time
db, kind, rows = sys.argv[1], sys.argv[2], sys.argv[3].split(",")
now = int(time.time())
val = {"past": now - 86400, "future": now + 86400, "session": 0}
table, col = ("moz_cookies", "expiry") if kind == "firefox" else ("cookies", "expires_utc")
con = sqlite3.connect(db)
con.execute("CREATE TABLE %s (%s INTEGER)" % (table, col))
for r in rows:
    e = val[r]
    if kind != "firefox" and e:
        e = int((e + 11644473600) * 1000000)
    con.execute("INSERT INTO %s VALUES (?)" % table, (e,))
con.commit()"#;
        let case = |name: &str, kind: Kind, rows: &str| {
            let db = dir.join(name);
            py(&python, builder, &[db.as_os_str(), shape(kind).as_ref(), rows.as_ref()]);
            cookies_expired(&db, kind)
        };
        assert_eq!(case("ff_dead", Kind::Firefox, "past,past"), Some(true), "all past → expired");
        assert_eq!(case("ff_live", Kind::Firefox, "past,future"), Some(false), "one live → live");
        assert_eq!(case("ff_sess", Kind::Firefox, "session,session"), Some(false), "session-only → never a false alarm");
        assert_eq!(case("cr_dead", Kind::Chromium, "past"), Some(true), "webkit epoch converts");
        assert_eq!(case("cr_live", Kind::Chromium, "past,future"), Some(false));
        assert_eq!(case("cr_sess", Kind::Chromium, "session"), Some(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cookie_dump_count_skips_comments_and_blanks_but_keeps_httponly() {
        // A Netscape dump as yt-dlp writes it: header + comment + blank skipped, `#HttpOnly_`
        // kept (the auth cookies are usually HttpOnly). The store is pre-filtered per site, so
        // the read-back only needs the decryptable total — not a per-domain tally.
        let dump = "# Netscape HTTP Cookie File\n\
                    # a comment\n\
                    \n\
                    #HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t2000000000\tLOGIN_INFO\ttok\n\
                    .google.com\tTRUE\t/\tTRUE\t2000000000\tSID\tsid\n";
        assert_eq!(count_cookie_dump(dump), 2, "two cookie rows, comments/blank excluded");
    }

    #[test]
    fn cookie_dump_count_of_an_empty_or_header_only_dump_is_zero() {
        assert_eq!(count_cookie_dump(""), 0);
        assert_eq!(count_cookie_dump("# Netscape HTTP Cookie File\n\n"), 0);
    }

    /// A source DB with no cookie table at all (a truncated copy, or the wrong file entirely)
    /// must come back as a clean zero rather than an error or a partial store — the caller reads
    /// zero as "nothing to import" and rolls the store back.
    #[test]
    fn a_source_without_the_expected_table_filters_to_zero() {
        let Some(python) = python_or_skip("empty filter") else { return };
        let dir = scratch_dir("cookieempty");
        let src = dir.join("cookies.sqlite");
        py(
            &python,
            "import sqlite3, sys\ncon = sqlite3.connect(sys.argv[1])\ncon.execute('CREATE TABLE unrelated (a)')\ncon.commit()",
            &[src.as_os_str()],
        );
        let store_dir = dir.join("store");
        std::fs::create_dir_all(&store_dir).unwrap();
        let store = CookieStore {
            label: "empty".into(),
            identity: "firefox",
            kind: Kind::Firefox,
            files: vec![(src, "cookies.sqlite")],
        };
        assert_eq!(filter_cookie_db(&store, &store_dir, &["tiktok.com".to_string()]), Some(0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
