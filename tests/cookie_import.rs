//! Offline end-to-end tests of `--cookies-extract-for-domain` and the imported-store plumbing: real
//! sqlite cookie DBs inside fake browser profiles under a scratch HOME, the real binary, python3
//! doing the actual row filtering, and the scripted yt-dlp stand-in (tests/fixtures/yt_dlp_stub.sh)
//! making the readability verdicts deterministic. The heart of it is the feature's privacy
//! contract: ONLY the target site's rows ever reach the store.
//!
//! The child's world is HERMETIC: its environment starts empty, its PATH holds only the rig's bin
//! dir — the stub as `yt-dlp`, a HOME-independent `python3`, and `bash` for the stub's shebang —
//! and its HOME and XDG data home are the rig's, so the store a run reads by default is the one the
//! rig imports into. Anything less deterministic leaks the running machine in: real browsers on
//! PATH flip `any_browser_installed`'s message, an inherited `$XDG_CONFIG_HOME` widens the browser
//! scan to the real profiles, and a `$HOME`-relative python shim resolves to nothing under the
//! scratch HOME. Network-free; the sqlite-backed tests skip-with-notice without a python3.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const YT_VIDEO: &str = "https://www.youtube.com/watch?v=stubvid0000";

/// The first existing file among fixed guesses for `name`, then every PATH dir — for wiring
/// real binaries (bash, python) into the rig's hermetic bin dir by absolute path.
fn locate(name: &str, fixed: &[&str]) -> Option<PathBuf> {
    let from_path = std::env::var_os("PATH").map(|paths| {
        std::env::split_paths(&paths).map(|dir| dir.join(name)).collect::<Vec<_>>()
    });
    fixed
        .iter()
        .map(PathBuf::from)
        .chain(from_path.unwrap_or_default())
        .find(|candidate| candidate.is_file())
}

/// Skip-with-notice: an absolute python3 that works independently of `$HOME`. A python shim on
/// PATH that `exec`s a `$HOME`-relative path (a venv's wrapper, say) points at nothing under the
/// rig's scratch HOME — every candidate is therefore probed with a bogus HOME and the first real
/// interpreter wins. `None` (after a visible SKIP line) when no candidate works; the test then
/// passes vacuously.
fn hermetic_python3(test: &str) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> =
        ["/usr/bin/python3", "/usr/local/bin/python3", "/bin/python3"]
            .iter()
            .map(PathBuf::from)
            .collect();
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            candidates.push(dir.join("python3"));
            candidates.push(dir.join("python"));
        }
    }
    let found = candidates.into_iter().filter(|path| path.is_file()).find(|path| {
        Command::new(path)
            .env("HOME", "/nonexistent")
            .args(["-c", "import sqlite3"])
            .output()
            .is_ok_and(|out| out.status.success())
    });
    if found.is_none() {
        eprintln!("SKIPPED {test}: no HOME-independent python3 (with sqlite3) found");
    }
    found
}

/// Run a python snippet with `args`, asserting success.
fn py(python: &Path, code: &str, args: &[&std::ffi::OsStr]) {
    let out = Command::new(python).arg("-c").arg(code).args(args).output().expect("run python3");
    assert!(out.status.success(), "python failed: {}", String::from_utf8_lossy(&out.stderr));
}

/// Run a python snippet with `args`, returning stdout.
fn py_read(python: &Path, code: &str, args: &[&std::ffi::OsStr]) -> String {
    let out = Command::new(python).arg("-c").arg(code).args(args).output().expect("run python3");
    assert!(out.status.success(), "python failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// One test's world: a scratch HOME (the browser profiles), an XDG data home (the stores a run reads
/// by default), a download dir, the stub's state dir, and the hermetic bin dir that becomes the
/// child's ENTIRE PATH. Removed on drop.
struct Rig {
    root: PathBuf,
    home: PathBuf,
    into: PathBuf,
    stub: PathBuf,
}

impl Rig {
    /// `python`: the HOME-independent interpreter to expose as the child's `python3` (tests
    /// whose flow never reaches python pass `None`).
    fn new(tag: &str, python: Option<&Path>) -> Rig {
        let root =
            std::env::temp_dir().join(format!("vidl_cookie_import_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (home, into, stub, bin) =
            (root.join("home"), root.join("into"), root.join("stub"), root.join("bin"));
        for dir in [&home, &into, &stub, &bin] {
            fs::create_dir_all(dir).unwrap();
        }
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/yt_dlp_stub.sh");
        let ytdlp = bin.join("yt-dlp");
        fs::copy(&script, &ytdlp).unwrap();
        fs::set_permissions(&ytdlp, fs::Permissions::from_mode(0o755)).unwrap();
        // The stub's `#!/usr/bin/env bash` resolves bash via the child's (hermetic) PATH.
        if let Some(bash) = locate("bash", &["/bin/bash", "/usr/bin/bash"]) {
            let _ = std::os::unix::fs::symlink(bash, bin.join("bash"));
        }
        if let Some(python) = python {
            let _ = std::os::unix::fs::symlink(python, bin.join("python3"));
        }
        Rig { root, home, into, stub }
    }

    fn bin(&self) -> PathBuf {
        self.root.join("bin")
    }

    /// Where a run looks for imported stores when nobody names a root: `vidl/browser_cookies`
    /// under the rig's XDG data home. Every import here goes to it, so the runs after it find what
    /// it wrote.
    fn store_root(&self) -> PathBuf {
        self.root.join("data/vidl/browser_cookies")
    }

    /// Run `vidl <args…>` in the rig's world, optionally feeding stdin (the import's pick menu).
    /// The environment is cleared and rebuilt from the rig's own HOME, PATH and XDG data home —
    /// hermetic on purpose; see the module doc.
    fn run(&self, mode: &str, args: &[&str], stdin: Option<&str>) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_vidl"));
        cmd.args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", self.bin())
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("VIDL_STUB_DIR", &self.stub)
            .env("VIDL_STUB_MODE", mode);
        match stdin {
            Some(text) => {
                cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
                let mut child = cmd.spawn().expect("spawn vidl");
                child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
                child.wait_with_output().expect("run vidl")
            }
            None => {
                cmd.stdin(Stdio::null());
                cmd.output().expect("run vidl")
            }
        }
    }

    /// `--cookies-extract-for-domain <target>` into [`Self::store_root`].
    fn import_args(&self, target: &str) -> Vec<String> {
        let root = self.store_root().display().to_string();
        vec!["--cookies-extract-for-domain".into(), target.into(), root]
    }

    fn import(&self, mode: &str, target: &str) -> Output {
        let args = self.import_args(target);
        self.run(mode, &args.iter().map(String::as_str).collect::<Vec<_>>(), None)
    }

    fn import_with_stdin(&self, mode: &str, target: &str, stdin: &str) -> Output {
        let args = self.import_args(target);
        self.run(mode, &args.iter().map(String::as_str).collect::<Vec<_>>(), Some(stdin))
    }

    /// The per-site store dir an import writes under the rig's store root.
    fn site_dir(&self, key: &str) -> PathBuf {
        self.store_root().join(key)
    }

    /// Every yt-dlp invocation the run made, one argv per line.
    fn calls(&self) -> String {
        fs::read_to_string(self.stub.join("calls.log")).unwrap_or_default()
    }

    /// Seed a ready-made store for `site`, skipping the import flow. Its contents are never read
    /// by the stub — what matters downstream is only that a store EXISTS to be selected.
    fn seed_store(&self, site: &str) {
        let dir = self.site_dir(site);
        fs::create_dir_all(dir.join("store")).unwrap();
        fs::write(dir.join("browser.spec"), "firefox").unwrap();
        fs::write(dir.join("store/cookies.sqlite"), "junk").unwrap();
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn text(out: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

/// Build a realistic Firefox profile DB (schema from tests/fixtures/moz_cookies.sql) in the
/// rig's HOME: two cookies for `site`, plus one from another site whose value is the canary
/// `SECRET`.
fn build_firefox_profile(python: &Path, rig: &Rig, site: &str) {
    let profile = rig.home.join(".mozilla/firefox/test.default-release");
    fs::create_dir_all(&profile).unwrap();
    let schema = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/moz_cookies.sql");
    py(
        python,
        r#"import sqlite3, sys, time
db, schema, site = sys.argv[1], sys.argv[2], sys.argv[3]
con = sqlite3.connect(db)
con.executescript(open(schema).read())
exp = int(time.time()) + 86400
rows = [("." + site, "sessionid", "s1"), ("www." + site, "csrf", "s2"),
        (".evil-bank.com", "account", "SECRET")]
for host, name, value in rows:
    con.execute(
        "INSERT INTO moz_cookies (name, value, host, path, expiry, lastAccessed, creationTime, isSecure, isHttpOnly) "
        "VALUES (?, ?, ?, '/', ?, 0, 0, 1, 1)",
        (name, value, host, exp))
con.commit()"#,
        &[profile.join("cookies.sqlite").as_os_str(), schema.as_os_str(), site.as_ref()],
    );
}

/// A Netscape dump the stub "extracts" for the readability check — 2 readable cookies.
fn readable_dump(rig: &Rig, site: &str) {
    fs::write(
        rig.stub.join("cookie_dump.txt"),
        format!(
            "# Netscape HTTP Cookie File\n.{site}\tTRUE\t/\tTRUE\t0\tsessionid\ts1\n.{site}\tTRUE\t/\tTRUE\t0\tcsrf\ts2\n"
        ),
    )
    .unwrap();
}

/// The cookie names in a store DB, sorted, one per line.
fn store_names(python: &Path, db: &Path) -> String {
    py_read(
        python,
        r#"import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
table = "moz_cookies" if con.execute("SELECT count(*) FROM sqlite_master WHERE name='moz_cookies'").fetchone()[0] else "cookies"
for (n,) in con.execute("SELECT name FROM %s ORDER BY name" % table):
    print(n)"#,
        &[db.as_os_str()],
    )
}

/// The *download* invocations among a run's calls, told apart by the one flag only a download
/// carries: `--output`. Not by the absence of `--print` — every download argv also carries
/// `--print-to-file`, while the import's readability check carries neither.
fn downloads(calls: &str) -> Vec<&str> {
    calls.lines().filter(|line| line.contains("--output")).collect()
}

#[test]
fn cookie_import_copies_only_the_target_sites_rows_and_records_the_store() {
    let Some(python) = hermetic_python3("cookie import e2e") else { return };
    let rig = Rig::new("happy", Some(&python));
    build_firefox_profile(&python, &rig, "tiktok.com");
    readable_dump(&rig, "tiktok.com");

    let out = rig.import("ok", "tiktok");
    let all = text(&out);
    assert!(out.status.success(), "{all}");
    assert!(all.contains("one cookie store found"), "a lone store auto-selects, no prompt: {all}");
    assert!(all.contains("imported 2 tiktok cookie(s)"), "{all}");
    assert!(all.contains("validated — 2 tiktok cookie(s) readable"), "readability verified: {all}");

    let site = rig.site_dir("tiktok");
    assert_eq!(fs::read_to_string(site.join("browser.spec")).unwrap().trim(), "firefox");
    let store_db = site.join("store/cookies.sqlite");
    assert_eq!(store_names(&python, &store_db), "csrf\nsessionid\n", "ONLY tiktok rows on disk");
    // The privacy contract, byte-level: the other site's cookie value must not appear ANYWHERE
    // in the copied store file.
    let leaked = fs::read(&store_db).unwrap().windows(6).any(|window| window == b"SECRET");
    assert!(!leaked, "a foreign cookie value leaked into the store file");

    // A re-import replaces the store — same two rows, never an accumulation.
    let out = rig.import("ok", "tiktok");
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(store_names(&python, &store_db), "csrf\nsessionid\n", "wiped and rebuilt, not merged");
}

#[test]
fn a_store_whose_cookies_cannot_be_read_back_is_discarded_not_kept_broken() {
    let Some(python) = hermetic_python3("zero-decrypt verdict") else { return };
    let rig = Rig::new("zerodecrypt", Some(&python));
    build_firefox_profile(&python, &rig, "tiktok.com");
    // The stub's "extraction" yields a dump with no cookie rows — the filtered store is useless.
    fs::write(rig.stub.join("cookie_dump.txt"), "# Netscape HTTP Cookie File\n# empty\n").unwrap();

    let out = rig.import("ok", "tiktok");
    assert!(!out.status.success(), "a discarded import must fail: {}", text(&out));
    assert!(text(&out).contains("none could be read"), "{}", text(&out));
    let site = rig.site_dir("tiktok");
    assert!(!site.join("browser.spec").exists(), "the spec must be forgotten");
    assert!(!site.join("store").exists(), "the store must be forgotten");
}

#[test]
fn an_unverifiable_import_keeps_the_store_provisionally() {
    let Some(python) = hermetic_python3("unverifiable verdict") else { return };
    let rig = Rig::new("unverifiable", Some(&python));
    build_firefox_profile(&python, &rig, "tiktok.com");
    // The readability probe itself dies (mode `fail`) — the count is unknowable, so the store
    // stays on its provisional count rather than being overclaimed or thrown away.
    let out = rig.import("fail", "tiktok");
    let all = text(&out);
    assert!(out.status.success(), "{all}");
    assert!(all.contains("couldn't verify readability"), "{all}");
    assert!(rig.site_dir("tiktok").join("store/cookies.sqlite").exists(), "store kept");
}

#[test]
fn cookie_import_with_a_url_falls_through_to_a_download_using_the_fresh_store() {
    let Some(python) = hermetic_python3("import fall-through") else { return };
    let rig = Rig::new("fallthrough", Some(&python));
    build_firefox_profile(&python, &rig, "youtube.com");
    readable_dump(&rig, "youtube.com");

    let into = rig.into.to_string_lossy().into_owned();
    let mut args = rig.import_args("youtube");
    args.extend([YT_VIDEO.to_string(), "--into".to_string(), into]);
    let out = rig.run("ok", &args.iter().map(String::as_str).collect::<Vec<_>>(), None);
    let all = text(&out);
    assert!(out.status.success(), "{all}");
    assert!(all.contains("imported 2 youtube cookie(s)"), "{all}");
    assert!(all.contains("YouTube rotates cookies"), "the rotation note rides a youtube import: {all}");
    // The download that follows must authenticate with the store imported moments ago.
    let calls = rig.calls();
    let download = *downloads(&calls).first().expect("a download ran");
    assert!(
        download.contains("--cookies-from-browser") && download.contains("browser_cookies/youtube/store"),
        "the fresh store must be used: {download}"
    );
}

#[test]
fn multiple_stores_prompt_a_menu_and_honor_or_reject_the_choice() {
    let Some(python) = hermetic_python3("store menu") else { return };
    let rig = Rig::new("menu", Some(&python));
    build_firefox_profile(&python, &rig, "tiktok.com");
    // A second store: a legacy flat Chromium profile (values encrypted; irrelevant to the menu).
    let chrome = rig.home.join(".config/google-chrome/Default");
    fs::create_dir_all(&chrome).unwrap();
    py(
        &python,
        r#"import sqlite3, sys
con = sqlite3.connect(sys.argv[1])
con.execute("CREATE TABLE cookies (creation_utc INTEGER, host_key TEXT, name TEXT, encrypted_value BLOB)")
con.execute("CREATE TABLE meta (key LONGVARCHAR NOT NULL UNIQUE PRIMARY KEY, value LONGVARCHAR)")
con.execute("INSERT INTO meta VALUES ('version', '24')")
con.executemany("INSERT INTO cookies (creation_utc, host_key, name, encrypted_value) VALUES (0, ?, ?, x'76')",
                [(".tiktok.com", "sessionid"), ("www.tiktok.com", "csrf")])
con.commit()"#,
        &[chrome.join("Cookies").as_os_str()],
    );
    readable_dump(&rig, "tiktok.com");

    // A number off the menu is rejected without touching anything.
    let out = rig.import_with_stdin("ok", "tiktok", "9\n");
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("not a listed number"), "{}", text(&out));
    assert!(!rig.site_dir("tiktok").join("browser.spec").exists(), "nothing written on a bad pick");

    // A valid pick imports from the chosen browser.
    let out = rig.import_with_stdin("ok", "tiktok", "1\n");
    let all = text(&out);
    assert!(out.status.success(), "{all}");
    assert!(all.contains("Import tiktok cookies from:"), "{all}");
    assert!(all.contains(" 1) ") && all.contains(" 2) "), "a numbered menu: {all}");
    let spec = fs::read_to_string(rig.site_dir("tiktok").join("browser.spec")).unwrap();
    assert!(["firefox", "chrome"].contains(&spec.trim()), "a real browser was recorded: {spec}");
}

#[test]
fn an_expired_store_warns_in_red_before_the_download_and_a_live_one_stays_silent() {
    let Some(python) = hermetic_python3("expiry warning") else { return };
    // Seed an already-imported store directly (spec + a real cookie DB), with the given expiry.
    let seed = |rig: &Rig, expiry: &str| {
        let site = rig.site_dir("youtube");
        fs::create_dir_all(site.join("store")).unwrap();
        fs::write(site.join("browser.spec"), "firefox").unwrap();
        py(
            &python,
            r#"import sqlite3, sys, time
offset = 86400 if sys.argv[2] == "future" else -86400
con = sqlite3.connect(sys.argv[1])
con.execute("CREATE TABLE moz_cookies (expiry INTEGER)")
con.execute("INSERT INTO moz_cookies VALUES (?)", (int(time.time()) + offset,))
con.commit()"#,
            &[site.join("store/cookies.sqlite").as_os_str(), expiry.as_ref()],
        );
    };

    let rig = Rig::new("expired", Some(&python));
    seed(&rig, "past");
    let into = rig.into.to_string_lossy().into_owned();
    let out = rig.run("ok", &[YT_VIDEO, "--into", &into], None);
    let all = text(&out);
    assert!(out.status.success(), "the warning must not block the download: {all}");
    assert!(
        all.contains("cookies have expired") && all.contains("--cookies-extract-for-domain youtube"),
        "the red heads-up names the fix: {all}"
    );

    let rig = Rig::new("live", Some(&python));
    seed(&rig, "future");
    let into = rig.into.to_string_lossy().into_owned();
    let out = rig.run("ok", &[YT_VIDEO, "--into", &into], None);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!text(&out).contains("have expired"), "a live store must not cry wolf: {}", text(&out));
}

#[test]
fn import_on_a_machine_with_no_browser_stores_reports_and_fails() {
    // The hermetic PATH holds no browser binaries, so the "no browsers at all" branch is
    // deterministic here — regardless of what the running machine has installed.
    let rig = Rig::new("bare", None);
    let out = rig.import("ok", "tiktok");
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("no browser cookie stores found"), "{}", text(&out));
}

#[test]
fn a_browser_on_path_without_stores_prompts_a_sign_in_instead() {
    // The sibling branch: a browser binary IS reachable (planted on the hermetic PATH) but no
    // profile holds cookies yet — the message should invite a sign-in, not claim no browsers.
    let rig = Rig::new("nostores", None);
    let firefox = rig.bin().join("firefox");
    fs::write(&firefox, "#!/usr/bin/env bash\nexit 0\n").unwrap();
    fs::set_permissions(&firefox, fs::Permissions::from_mode(0o755)).unwrap();
    let out = rig.import("ok", "tiktok");
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("found browsers but no cookie stores yet"),
        "{}",
        text(&out)
    );
}

/// The other half of the cookie-store contract: having imported one, a run on that site must USE
/// it by default — and `--no-cookies` must keep it out of every yt-dlp call. Tested here rather
/// than with the download flows because the store is what's under test, not the download.
#[test]
fn an_imported_store_is_the_standing_default_and_no_cookies_suppresses_it() {
    let with_store = |tag: &str, flags: &[&str]| {
        let rig = Rig::new(tag, None);
        rig.seed_store("youtube");
        let mut args = vec!["https://www.youtube.com/watch?v=stubvid0000", "--into"];
        let into = rig.into.display().to_string();
        args.push(&into);
        args.extend_from_slice(flags);
        let out = rig.run("ok", &args, None);
        assert!(out.status.success(), "{}", text(&out));
        rig.calls()
    };
    let used = with_store("cookies_on", &[]);
    assert!(used.contains("--cookies-from-browser"), "the store is the standing default:\n{used}");
    let suppressed = with_store("cookies_off", &["--no-cookies"]);
    assert!(
        !suppressed.contains("--cookies-from-browser"),
        "--no-cookies must keep the store out of every yt-dlp call:\n{suppressed}"
    );
}

/// Precedence among the three ways cookies can reach yt-dlp. A run decides only ONE of them — the
/// auto-selected store — so anything the user names explicitly has to win, and win intact.
#[test]
fn an_explicitly_named_cookie_source_beats_the_auto_selected_store() {
    let run = |tag: &str, flags: &[&str]| {
        let rig = Rig::new(tag, None);
        rig.seed_store("youtube");
        let mut args = vec!["https://www.youtube.com/watch?v=stubvid0000", "--into"];
        let into = rig.into.display().to_string();
        args.push(&into);
        args.extend_from_slice(flags);
        let out = rig.run("ok", &args, None);
        assert!(out.status.success(), "{}", text(&out));
        rig.calls()
    };

    // A --cookies file: the store must not also be sent, or yt-dlp gets two conflicting sources.
    // The jar has to actually exist — vidl checks before downloading, so that a bad path is
    // reported up front instead of as a Python codec error four lines into the first attempt.
    let jar = std::env::temp_dir().join(format!("vidl_cookie_import_jar_{}.txt", std::process::id()));
    std::fs::write(&jar, "# Netscape HTTP Cookie File\n").unwrap();
    let jar_arg = jar.display().to_string();
    let file = run("prec_file", &["--cookies-use", &jar_arg]);
    assert!(file.contains(&format!("--cookies {jar_arg}")), "the file is passed:\n{file}");
    assert!(
        !file.contains("--cookies-from-browser"),
        "an explicit jar must suppress the auto-selected store:\n{file}"
    );

    // An explicit --cookies-from-browser: passed through verbatim, NOT replaced by the store's
    // own spec (which would silently point at a different profile than the user asked for).
    let spec = run("prec_spec", &["--cookies-use", "chrome:/somewhere/else"]);
    assert!(
        spec.contains("--cookies-from-browser chrome:/somewhere/else"),
        "the user's spec reaches yt-dlp unchanged:\n{spec}"
    );
    assert!(
        !spec.contains("browser_cookies/youtube"),
        "the auto-selected store must not override an explicit spec:\n{spec}"
    );
    let _ = std::fs::remove_file(&jar);
}

/// `--no-cookies` is refused alongside every way of asking FOR cookies, rather than silently
/// picking a winner — the flags contradict each other and a quiet resolution would send (or
/// withhold) a session against the user's stated intent.
#[test]
fn no_cookies_conflicts_with_every_way_of_supplying_them() {
    let rig = Rig::new("conflicts", None);
    for other in [
        vec!["--cookies-use", "/tmp/j.txt"],
        vec!["--cookies-use", "firefox"],
        vec!["--cookies-extract-for-domain", "youtube", "/tmp/jars"],
    ] {
        let mut args = vec!["https://www.youtube.com/watch?v=stubvid0000", "--no-cookies"];
        args.extend_from_slice(&other);
        let out = rig.run("ok", &args, None);
        assert!(!out.status.success(), "--no-cookies {other:?} must be rejected");
        let text = text(&out);
        assert!(
            text.contains("cannot be used with"),
            "clap should name the conflict for --no-cookies {other:?}:\n{text}"
        );
    }
}
