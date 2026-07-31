//! The embedder seam: [`vidl::tools::install`], exercised through the library rather than the
//! binary.
//!
//! An embedder that pins its own yt-dlp (because yt-dlp breaks often enough that pinning is the
//! point) depends on two promises — that the path it names is the one actually run, and that a
//! later call cannot move the ground under a run in progress. Nothing else covers those: the
//! other suites drive the binary, which takes the PATH default.
//!
//! This lives in its own file deliberately. `install` writes to a process-wide `OnceLock`, so a
//! test that calls it would fix the tools for every other test sharing the binary — and each
//! integration test file is its own process.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// A scratch root with the yt-dlp stub installed somewhere PATH will never look.
struct Rig {
    root: PathBuf,
    into: PathBuf,
    stub_dir: PathBuf,
    ytdlp: PathBuf,
}

impl Rig {
    fn new() -> Rig {
        let root = std::env::temp_dir().join(format!("vidl_inject_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (into, stub_dir, hidden) =
            (root.join("into"), root.join("stub"), root.join("not-on-path"));
        for dir in [&into, &stub_dir, &hidden] {
            fs::create_dir_all(dir).unwrap();
        }
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/yt_dlp_stub.sh");
        // Deliberately NOT named `yt-dlp`: if injection silently failed and something fell back
        // to a PATH lookup, it could not find this by accident.
        let ytdlp = hidden.join("pinned-downloader");
        fs::copy(&script, &ytdlp).unwrap();
        fs::set_permissions(&ytdlp, fs::Permissions::from_mode(0o755)).unwrap();
        Rig { root, into, stub_dir, ytdlp }
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.stub_dir.join("calls.log")).unwrap_or_default()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn an_injected_ytdlp_is_the_one_run_and_the_first_install_wins() {
    let rig = Rig::new();
    // SAFETY: single-threaded — this test is the whole binary, and the stub reads these at exec.
    unsafe {
        std::env::set_var("VIDL_STUB_DIR", &rig.stub_dir);
        std::env::set_var("VIDL_STUB_MODE", "ok");
    }

    vidl::tools::install(vidl::tools::Tools {
        ytdlp: Some(rig.ytdlp.clone().into_os_string()),
        ..Default::default()
    });
    // The second call must be ignored. If it won instead, the run below would try to execute a
    // path that does not exist and the call log would stay empty — so this assertion is load
    // bearing, not decorative.
    vidl::tools::install(vidl::tools::Tools {
        ytdlp: Some("/nonexistent/would-break-the-run".into()),
        ..Default::default()
    });

    let env = vidl::Env::default();
    vidl::download_generic("https://media.example.com/v/1", &rig.into, env);

    let calls = rig.calls();
    assert!(
        !calls.is_empty(),
        "the injected yt-dlp must actually be executed — nothing was invoked at all"
    );
    assert!(
        calls.contains("--download-archive"),
        "and it must receive the real download argv:\n{calls}"
    );
}
