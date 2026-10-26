//! The command line, as reusable types.
//!
//! [`Cli`] and its halves are public on purpose: the `vidl` binary is nothing but
//! `Cli::parse().run()`, so *anything* the binary can do is reachable from the library with the
//! same flags and the same behaviour. An embedder that wants `vidl`'s command line as part of
//! its own flattens [`Args`] (a download's options, for [`run`]) or [`RepairArgs`] (a repair's,
//! for [`run_repair`]) into its parser rather than restating flags that would then drift. Each
//! holds the [`GlobalArgs`] the two share, so either one is the whole of its command.
//!
//! A caller that wants to make the routing decision itself skips all of them and drives
//! [`crate::classify`] plus the `download_*` functions with an [`Env`].

use std::path::PathBuf;

use clap::{Args as ClapArgs, Parser, Subcommand};

use crate::ytdlp::names;
use crate::{Env, Link, download_channel, download_generic, download_playlist, download_video, taglist};

/// The `vidl` command line whole: a download's options ([`Args`]), or another action named first
/// ([`Action`]). What the binary parses; an embedder flattens [`Args`] and [`RepairArgs`] into
/// commands of its own instead, offering each as it sees fit.
#[derive(Parser, Debug)]
#[command(
    name = "vidl",
    version,
    about = "Download a video with yt-dlp: YouTube playlists and channels build folder trees; any \
             other site downloads flat",
    // Clap's own puts the command after `-- <EXTRA>…`, where it could never be given.
    override_usage = "vidl [OPTIONS] [URL] [-- <EXTRA>...]\n       vidl repair [OPTIONS] [PATH]"
)]
pub struct Cli {
    #[command(flatten)]
    pub args: Args,
    /// What to do instead of downloading
    #[command(subcommand)]
    pub action: Option<Action>,
}

impl Cli {
    /// Do what the command line says, and give back its exit code.
    pub fn run(self) -> i32 {
        match self.action {
            None => run(self.args),
            Some(Action::Repair(repair)) => {
                if let Some(url) = &self.args.url {
                    eprintln!("error: `repair` adds to what is already downloaded, and takes no URL (given {url})");
                    return 2;
                }
                // What comes before `repair` is a download's, even an option a repair has too:
                // refused, not ignored, or `vidl --no-cookies repair` would ask YouTube with the
                // very cookies it was told to leave out.
                if Cli::try_parse_from(["vidl"]).is_ok_and(|bare| bare.args != self.args) {
                    eprintln!(
                        "vidl: a repair's options go after `repair` — those before it are a download's \
                         (see `vidl repair --help`)"
                    );
                    return 2;
                }
                run_repair(repair)
            }
        }
    }
}

/// What `vidl` does instead of downloading.
#[derive(Subcommand, Clone, Debug)]
pub enum Action {
    /// Add to what is already downloaded the data it lacks
    ///
    /// Every YouTube video in PATH and the folders below it — or the one video file PATH names —
    /// gets the data a download of it would hold now and it lacks: its thumbnail, its subtitle
    /// tracks (tags, in an audio extraction) and their auto-generated marks, its metadata tags, its
    /// chapters. A video is known by the id in its file name, or else by the page its metadata says
    /// it was downloaded from; one from another site is named and passed over. Nothing is
    /// downloaded again, and nothing a file holds is replaced — a tag it has keeps its value.
    /// YouTube is asked about every video once
    Repair(RepairArgs),
}

/// Everything `vidl repair` takes: where, whether only to look, and the [`GlobalArgs`] it shares
/// with a download.
#[derive(ClapArgs, Clone, Debug, PartialEq)]
pub struct RepairArgs {
    /// A folder to complete, with every folder below it — or one video file
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// Only say what each video lacks, adding nothing
    #[arg(long)]
    pub check: bool,
    #[command(flatten)]
    pub global: GlobalArgs,
}

/// A download's options: everything the `vidl` command line takes but an [`Action`]. Mirrors
/// [`Env`] one-for-one, plus what is the CLI's own business: what to download and where to put it,
/// and `--taglist` — with the [`GlobalArgs`] a repair takes too.
#[derive(ClapArgs, Clone, Debug, Default, PartialEq)]
pub struct Args {
    // Deliberately not `required_unless_present = "taglist"`: an embedder that flattens this
    // struct cannot widen a clap-level requirement to cover its own no-URL actions (a cookie
    // import, a site listing), so the check lives in `run` instead — see its test.
    /// A video URL — YouTube (video, playlist, or channel) or any other site yt-dlp supports
    pub url: Option<String>,
    /// Destination root — YouTube playlists/channels build folder trees here; other sites
    /// download into it directly
    #[arg(long, default_value = ".")]
    pub into: PathBuf,
    /// A YouTube video link that also names a playlist (`watch?v=…&list=…`) downloads the whole
    /// playlist by default; this takes just the video
    #[arg(long)]
    pub single: bool,
    #[command(flatten)]
    pub global: GlobalArgs,
    /// Audio only: extract the best audio track (kept as-is, no re-encode); on the YouTube
    /// path subtitles arrive as metadata tags (`subtitles_en`, `subtitles_he_autogenerated`, …)
    #[arg(long)]
    pub audio: bool,
    /// Cap the video height (e.g. 1080) — takes the best formats at or under it
    #[arg(long, value_name = "HEIGHT")]
    pub res: Option<u32>,
    /// Force a subtitle scan of the target video(s) and embed any expected tracks that are
    /// missing. Subtitles already ride a normal download; this late, idempotent pass (ignoring
    /// the download archive) is for patching previously-downloaded videos on a re-run
    #[arg(long)]
    pub subtitles: bool,
    /// Look for more than one video on the page. Some sites hide a whole collection behind one
    /// URL, swapping which video plays without changing the address; this downloads all of them
    /// into a folder. Off-YouTube only — YouTube publishes collections as playlists and channels,
    /// which are walked already
    #[arg(long)]
    pub multi: bool,
    /// What to call the downloaded file, when the site offers no title worth having — a bare
    /// `.m3u8`, `.mpd` or media URL, where yt-dlp has nothing to read a title from and names the
    /// file after the manifest. Off-YouTube and one video at a time; for anything else, `-- -o
    /// <template>` gives full control
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// Longest file name the destination accepts, in bytes. Measured from the destination when
    /// left out, which is right for every mainstream filesystem (255); set it for one that takes
    /// less than it reports — an encrypted-filename mount, say, where 143 is typical
    #[arg(long, value_name = "BYTES")]
    pub name_max: Option<usize>,
    /// Print the notable yt-dlp flags (usable after `--`), then yt-dlp's full option list
    #[arg(short = 't', long)]
    pub taglist: bool,
    /// Anything after `--` is handed to yt-dlp verbatim, after our defaults — a repeated
    /// flag resolves in its favor; `-t` lists the ones worth knowing
    #[arg(last = true)]
    pub extra: Vec<String>,
}

/// The options a download and a repair share — the cookies yt-dlp is given, IPv6, the thumbnail each
/// YouTube video gets — flattened into [`Args`] and [`RepairArgs`] alike rather than declared
/// `global = true` on [`Cli`]: an embedder that lifts one of them into a command of its own never
/// parses that root, so a global there would be unreachable from its command line.
#[derive(ClapArgs, Clone, Debug, Default, PartialEq)]
pub struct GlobalArgs {
    /// Cookies you already have, from either source: a Netscape cookies file (as browser
    /// extensions export), or an installed browser to read them from —
    /// `<browser>[:<profile-or-dir>]`, e.g. `firefox`. A value whose first word names a known
    /// browser is read as a browser, anything else as a path, so a file actually called `firefox`
    /// is `./firefox`. Only needed for age-restricted content or networks that bot-wall; home
    /// connections rarely do
    #[arg(long, value_name = "COOKIES/SPEC")]
    pub cookies_use: Option<String>,
    /// Import a browser's cookies for one site into a directory of your choosing — first, before
    /// the download or repair the same line asks for, or alone when it asks for neither
    #[arg(
        long,
        num_args = 2,
        value_names = ["TARGET", "OUTPUT-DIR"],
        conflicts_with = "cookies_use",
        help = cookies_extract_help()
    )]
    pub cookies_extract_for_domain: Option<Vec<String>>,
    /// Where a run looks for a previously imported store. Deliberately NOT a flag: an embedder owns
    /// its own data directory and sets this once, whereas a person running `vidl` says where the
    /// cookies go on the `--cookies-extract-for-domain` line itself. Left unset, runs fall back to
    /// `$XDG_DATA_HOME/vidl/browser_cookies`.
    #[arg(skip)]
    pub cookie_root: Option<PathBuf>,
    /// Ignore cookies extracted earlier for this site, and reach it anonymously — useful when a
    /// site bot-walls a signed-in session (e.g. TikTok) and sending your real cookies risks getting
    /// that session flagged
    #[arg(long, conflicts_with_all = ["cookies_use", "cookies_extract_for_domain"])]
    pub no_cookies: bool,
    /// Let yt-dlp use IPv6. IPv4 is forced by default — on a broken or slow IPv6 route, every
    /// request otherwise stalls ~5s on the fallback. Pass this on an IPv6-only network
    #[arg(long)]
    pub allow_ipv6: bool,
    /// Leave out the cover-art thumbnail a YouTube video without one otherwise gets, at one extra
    /// request per video: a download embeds it in a late, idempotent pass that, ignoring the
    /// download archive, also patches videos downloaded before; a repair adds it with whatever
    /// else a video lacks. Audio gets none either way
    #[arg(long)]
    pub no_thumbnail: bool,
}

/// `--cookies-extract-for-domain`'s help, built from the site registry rather than restating it —
/// the list has grown twice already, and a help text that repeats it drifts the moment it grows
/// again.
fn cookies_extract_help() -> String {
    format!(
        "Import a browser's cookies for one site, so later runs on that site authenticate. TARGET \
         is a site keyword ({}), a domain (example.com), or a URL; the destination gains one \
         subdirectory per imported site. Only that site's cookies are copied — never your whole \
         cookie DB. Scans installed browsers (native/Flatpak/Snap/Nix) and copies from your pick, \
         so a running browser can't lock it. Made first — before the download or repair the same \
         line asks for, or alone when it asks for neither",
        crate::cookies::sites::known_site_names()
    )
}

/// Where a run looks for an imported store: the embedder's directory when one was set, else the
/// per-user default. `None` only when neither exists (no embedder and no `$HOME`), which simply
/// means no store can be found and the run goes out anonymous.
fn read_root(global: &GlobalArgs) -> Option<PathBuf> {
    global.cookie_root.clone().or_else(crate::cookies::store::default_root)
}

impl Args {
    /// The download options these arguments describe, borrowed from `self`. Splitting this out
    /// lets a caller reuse the parsed flags while overriding one field — the usual reason being
    /// cookies it acquired some other way.
    pub fn env(&self) -> Env<'_> {
        // Every field named, so one added to `Env` must be decided here as well.
        let shared = self.global.env();
        Env {
            cookies: shared.cookies,
            cookies_from_browser: shared.cookies_from_browser,
            audio: self.audio,
            res: self.res,
            allow_ipv6: shared.allow_ipv6,
            thumbnail: shared.thumbnail,
            subtitles: self.subtitles,
            multi: self.multi,
            name: self.name.as_deref(),
            extra: &self.extra,
        }
    }
}

impl GlobalArgs {
    /// The options these describe, and what only a download decides at its default — what a
    /// repair runs with.
    pub fn env(&self) -> Env<'_> {
        let (file, browser) = crate::cookies::split_source(self.cookies_use.as_deref());
        Env {
            cookies: file,
            cookies_from_browser: browser,
            allow_ipv6: self.allow_ipv6,
            thumbnail: !self.no_thumbnail,
            ..Env::default()
        }
    }
}

/// Do what the `vidl` command line says, and give back its exit code.
///
/// Creates the destination, works out what the URL points at, and hands off to the matching
/// `download_*`. `--taglist` prints the flag reference and stops without touching the network.
pub fn run(args: Args) -> i32 {
    if args.taglist {
        return taglist();
    }
    // A cookie import is a setup step, and the only action besides `--taglist` that needs no URL:
    // done first, then either stop (no URL) or fall through and download, now able to use what was
    // just imported.
    match import_cookies(&args.global) {
        Err(code) => return code,
        Ok(Some(imported)) if args.url.is_none() => return i32::from(!imported),
        _ => {}
    }
    let Some(url) = args.url.clone() else {
        eprintln!("error: a URL is required (or --taglist for the flag reference; `vidl repair` completes what is downloaded)");
        return 2;
    };
    if let Err(err) = std::fs::create_dir_all(&args.into) {
        eprintln!("error: cannot create {}: {err}", args.into.display());
        return 1;
    }
    // Settle the file-name limit ONCE, here, before any output template is built: the caller's
    // number if they gave one, else measured from the destination we just made sure exists. Every
    // download in this run reads it back from that single answer, so a playlist of 400 videos
    // measures nothing 400 times.
    let limit = match args.name_max {
        Some(bytes) => bytes,
        None => names::probe_limit(&args.into),
    };
    names::install_limit(limit);
    if names::limit() != names::DEFAULT_NAME_MAX {
        println!(
            "file names here are limited to {} bytes (not {}) — titles are cut to fit",
            names::limit(),
            names::DEFAULT_NAME_MAX
        );
    }
    // Also once, and for the same reason as the limit probe above: leftovers from an interrupted
    // run sit wherever it was writing, so the destination is swept as a whole before anything new
    // is downloaded into it rather than checked per video.
    crate::media::discard_stale_parts(&args.into);
    // Cookies are settled ONCE, here, for the same reason as the name limit: every invocation of
    // the run reads the answer back, and the checks involved touch the filesystem. It also means a
    // cookie argument that cannot work is reported before anything is downloaded rather than as a
    // Python codec error four lines into the first attempt.
    let resolved = cookies_for(&args.global, &url);
    let Some(env) = env_with(args.env(), &resolved) else { return 2 };
    let link = crate::classify(&url, args.single);
    // `--multi` is for pages that give yt-dlp no structure to read. YouTube gives it plenty, and
    // this crate walks it; saying so beats letting the flag look like it did something.
    if args.multi && !matches!(link, Link::Generic) {
        eprintln!(
            "vidl: --multi is for sites that hide several videos behind one URL — YouTube \
             publishes those as playlists and channels, which this walks already; ignoring it"
        );
    }
    // `--name` names ONE file. It is applied in the generic path's rename pass, which the other
    // paths do not have — and a collection would give every entry the same name, which is worse
    // than not honouring the flag. Refused out loud in both cases, with the lever that does work.
    if args.name.is_some() && (!matches!(link, Link::Generic) || args.multi) {
        eprintln!(
            "{}",
            crate::util::style::problematic(
                "vidl: --name is for one video off a site with no title to offer; a playlist, a \
                 channel or --multi would put one name on every file. Use `-- -o <template>` to \
                 name those, and see --help-flags for what a template can hold"
            )
        );
        return 2;
    }
    match link {
        Link::Video => download_video(&url, &args.into, env),
        Link::Playlist { id } => download_playlist(&url, &id, &args.into, env),
        Link::Channel { root } => download_channel(&root, &args.into, env),
        Link::Generic => download_generic(&url, &args.into, env),
    }
}

/// Do what `vidl repair` says, and give back its exit code. A cookie import asked for on the same
/// line comes first, as before a download.
pub fn run_repair(repair: RepairArgs) -> i32 {
    if let Err(code) = import_cookies(&repair.global) {
        return code;
    }
    if !repair.path.exists() {
        eprintln!("error: there is nothing at {}", repair.path.display());
        return 2;
    }
    // What a repair asks about is YouTube, so YouTube's imported store is the one it uses.
    let resolved = cookies_for(&repair.global, "https://www.youtube.com/");
    let Some(env) = env_with(repair.global.env(), &resolved) else { return 2 };
    crate::repair(&repair.path, env, repair.check)
}

/// The cookie import `--cookies-extract-for-domain` asks for, if it does: whether it imported, or —
/// as `Err` — the exit code for one that cannot start.
fn import_cookies(global: &GlobalArgs) -> Result<Option<bool>, i32> {
    let Some(request) = &global.cookies_extract_for_domain else { return Ok(None) };
    let Some(into) = import_destination(global) else {
        eprintln!("error: --cookies-extract-for-domain needs an output directory");
        return Err(2);
    };
    Ok(Some(crate::cookies::import::run(&request[0], &into, read_root(global).as_deref())))
}

/// Where the import `global` asks for puts what it imports: the directory the flag's second value
/// names — two values on the flag, except where an embedder narrowed it to the target alone and
/// supplies the directory itself through `cookie_root` (which is the point of that field). `None`
/// when neither says, or no import is asked for.
fn import_destination(global: &GlobalArgs) -> Option<PathBuf> {
    let request = global.cookies_extract_for_domain.as_ref()?;
    request.get(1).map(PathBuf::from).or_else(|| global.cookie_root.clone())
}

/// The cookies a run on `url` uses. A store imported for its site is the standing default — that is
/// what an import is FOR. An explicit `--cookies-use` wins over it, and `--no-cookies` opts out
/// entirely.
fn cookies_for(global: &GlobalArgs, url: &str) -> crate::cookies::Cookies {
    let asked_for_cookies = global.cookies_use.is_some();
    let stored = (!global.no_cookies && !asked_for_cookies)
        .then(|| read_root(global).and_then(|root| crate::cookies::import::spec_for(&root, url)))
        .flatten();
    crate::cookies::resolve(stored.as_deref().or(global.cookies_use.as_deref()))
}

/// `env` with the cookies `resolved` settled on; `None`, said why, when those cannot work.
fn env_with<'a>(mut env: Env<'a>, resolved: &'a crate::cookies::Cookies) -> Option<Env<'a>> {
    match resolved {
        crate::cookies::Cookies::None => {
            env.cookies = None;
            env.cookies_from_browser = None;
        }
        crate::cookies::Cookies::File(path) => {
            env.cookies = Some(path);
            env.cookies_from_browser = None;
        }
        crate::cookies::Cookies::FromBrowser(spec) => {
            println!("reading cookies from {spec}");
            env.cookies = None;
            env.cookies_from_browser = Some(spec);
        }
        crate::cookies::Cookies::Unusable(why) => {
            eprintln!("{}", crate::util::style::problematic(&format!("error: {why}")));
            return None;
        }
    }
    Some(env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::path::Path;

    /// What the binary makes of `argv`, which must parse.
    fn cli(argv: &[&str]) -> Cli {
        let parsed = Cli::try_parse_from(std::iter::once("vidl").chain(argv.iter().copied()));
        parsed.unwrap_or_else(|err| panic!("{argv:?} must parse: {err}"))
    }

    fn parse(argv: &[&str]) -> Args {
        cli(argv).args
    }

    #[test]
    fn the_parsed_flags_reach_the_env_unchanged() {
        let args = parse(&["u", "--audio", "--res", "720", "--subtitles", "--", "-q"]);
        let env = args.env();
        assert!(env.audio);
        assert_eq!(env.res, Some(720));
        assert!(env.subtitles);
        assert_eq!(env.extra, ["-q"], "everything after `--` is yt-dlp's, verbatim");
        assert!(env.thumbnail, "thumbnails come unless `--no-thumbnail` says otherwise");
        assert!(!parse(&["u", "--no-thumbnail"]).env().thumbnail);
        assert!(Cli::try_parse_from(["vidl", "u", "--thumbnail"]).is_err(), "the old opt-in is gone");
    }

    /// The repair `argv` names, which must be one.
    fn repair(argv: &[&str]) -> RepairArgs {
        match cli(argv).action {
            Some(Action::Repair(repair)) => repair,
            None => panic!("{argv:?} must be a repair"),
        }
    }

    #[test]
    fn repair_is_its_own_command_holding_every_option_it_takes_after_its_name() {
        let bare = repair(&["repair"]);
        assert_eq!((bare.path.as_path(), bare.check, &bare.global), (Path::new("."), false, &GlobalArgs::default()));
        let file = repair(&["repair", "/v/a.mkv", "--check"]);
        assert_eq!((file.path.as_path(), file.check), (Path::new("/v/a.mkv"), true), "a folder, or one file");
        let shared = repair(&["repair", "--no-cookies", "--allow-ipv6", "--no-thumbnail"]).global;
        assert!(shared.no_cookies && shared.allow_ipv6 && shared.no_thumbnail, "what it shares with a download is its own");
        assert_eq!(repair(&["repair", "--cookies-use", "/c.txt"]).global.cookies_use.as_deref(), Some("/c.txt"));
        assert!(cli(&["u"]).action.is_none(), "a URL alone is a download");
        assert!(Cli::try_parse_from(["vidl", "--check"]).is_err(), "--check is the repair's own");
        assert!(Cli::try_parse_from(["vidl", "repair", "--audio"]).is_err(), "a download's options are no repair's");

        // An empty folder, so a run that wrongly went ahead would complete nothing — never the
        // working directory, which is wherever the tests happen to run. `--no-cookies` keeps the
        // run off any store imported on this machine.
        let empty = std::env::temp_dir().join(format!("vidl_empty_{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        let empty_dir = empty.to_str().unwrap();
        assert_eq!(cli(&["repair", "--no-cookies", empty_dir]).run(), 0, "a repair, not a download without its URL");
        assert_eq!(cli(&["https://x/v", "repair", empty_dir]).run(), 2, "a repair takes no URL");
        for before in ["--no-cookies", "--audio"] {
            assert_eq!(cli(&[before, "repair", empty_dir]).run(), 2, "{before} before `repair` is a download's, and refused");
        }
        let _ = std::fs::remove_dir(&empty);
        let nowhere = std::env::temp_dir().join(format!("vidl_nothing_here_{}", std::process::id()));
        assert_eq!(cli(&["repair", nowhere.to_str().unwrap()]).run(), 2, "nothing there, nothing to complete");
    }

    #[test]
    fn a_repair_runs_with_what_it_shares_with_a_download_and_a_downloads_defaults_for_the_rest() {
        let shared = repair(&["repair", "--allow-ipv6", "--no-thumbnail", "--cookies-use", "/c.txt"]).global;
        let env = shared.env();
        assert_eq!(env, Env { cookies: Some(Path::new("/c.txt")), allow_ipv6: true, thumbnail: false, ..Env::default() });
        assert_eq!(parse(&["u", "--allow-ipv6", "--no-thumbnail", "--cookies-use", "/c.txt"]).env(), env, "as a download reads them");
    }

    #[test]
    fn the_library_default_is_the_command_lines_default() {
        // Two doors into one run: a library caller starts from `Env::default()`, a command line
        // from its parsed flags (or an embedder's `Args::default()`). Whole values, so a field
        // added to one but defaulted differently in the other cannot slip through.
        assert_eq!(Env::default(), parse(&["u"]).env(), "a bare command line");
        assert_eq!(Env::default(), Args::default().env(), "an `Args` built in code");
        assert_eq!(Env::default(), repair(&["repair"]).global.env(), "a bare repair");
    }

    #[test]
    fn a_missing_url_is_refused_by_run_not_by_the_parser() {
        // The parser stays permissive so an embedder can add its own no-URL actions (a cookie
        // import, a compatibility listing) without fighting a `required_unless_present` it
        // cannot extend through `#[command(flatten)]`.
        assert!(Cli::try_parse_from(["vidl"]).is_ok(), "the parser allows it");
        assert_eq!(run(parse(&[])), 2, "`run` is what refuses, and says so");
        assert_eq!(parse(&["https://x/v"]).url.as_deref(), Some("https://x/v"));
    }

    #[test]
    fn cookies_reach_the_env_by_either_route() {
        // One flag, both destinations: the value's first word decides which of yt-dlp's two
        // cookie flags it becomes, and `env()` reaches that split without touching the disk.
        assert_eq!(parse(&["u", "--cookies-use", "/c.txt"]).env().cookies.unwrap().to_str(), Some("/c.txt"));
        assert!(parse(&["u", "--cookies-use", "/c.txt"]).env().cookies_from_browser.is_none());
        assert_eq!(parse(&["u", "--cookies-use", "firefox:/d"]).env().cookies_from_browser, Some("firefox:/d"));
        assert!(parse(&["u", "--cookies-use", "firefox:/d"]).env().cookies.is_none());
    }

    #[test]
    fn an_embedder_can_override_one_field_and_keep_the_rest() {
        // The reason `env()` is public: an embedder parses the same flags, then substitutes
        // cookies it acquired itself. Nothing else about the parsed arguments should shift.
        let args = parse(&["u", "--audio", "--res", "480"]);
        let mut env = args.env();
        env.cookies_from_browser = Some("firefox:/imported");
        assert_eq!(env.cookies_from_browser, Some("firefox:/imported"));
        assert!(env.audio);
        assert_eq!(env.res, Some(480));
    }

    /// The import flag names both halves of the job: which site, and where to put it. The
    /// destination is not optional on the command line — a person running `vidl` says where their
    /// cookies land rather than having a directory chosen for them.
    #[test]
    fn cookies_extract_takes_a_target_and_a_destination() {
        let args = parse(&["--cookies-extract-for-domain", "youtube", "/tmp/jars"]);
        assert_eq!(
            args.global.cookies_extract_for_domain.as_deref(),
            Some(["youtube".to_string(), "/tmp/jars".to_string()].as_slice())
        );
        assert!(args.url.is_none(), "an import needs no URL");
        // A target with no destination is refused by the parser, not silently defaulted.
        assert!(Cli::try_parse_from(["vidl", "--cookies-extract-for-domain", "youtube"]).is_err());
        // The read root is NOT reachable from argv — it is the embedder's channel, and the only
        // way a one-value import (a narrowed flag) can find a directory.
        assert!(parse(&["u"]).global.cookie_root.is_none());
    }

    /// An import goes where its own line says or, where an embedder narrowed the flag to the target
    /// alone, into the `cookie_root` it keeps its cookies in — the one place that embedder's runs
    /// then read them from.
    #[test]
    fn an_import_goes_where_its_line_says_or_where_the_embedder_keeps_its_cookies() {
        let mut global = parse(&["--cookies-extract-for-domain", "youtube", "/jars"]).global;
        assert_eq!(import_destination(&global), Some(PathBuf::from("/jars")));
        global.cookie_root = Some(PathBuf::from("/kept"));
        assert_eq!(import_destination(&global), Some(PathBuf::from("/jars")), "the line's own wins");
        global.cookies_extract_for_domain = Some(vec!["youtube".into()]);
        assert_eq!(import_destination(&global), Some(PathBuf::from("/kept")), "narrowed: the embedder's root");
        global.cookie_root = None;
        assert_eq!(import_destination(&global), None, "and nothing when neither says");
        assert_eq!(import_destination(&GlobalArgs::default()), None, "no import, no destination");
    }

    #[test]
    fn the_destination_defaults_to_here() {
        assert_eq!(parse(&["u"]).into, PathBuf::from("."));
        assert_eq!(parse(&["u", "--into", "/tmp/v"]).into, PathBuf::from("/tmp/v"));
    }
}
