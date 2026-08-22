# vidl

A `yt-dlp` driver, as a Rust library and a small CLI.

`yt-dlp` already downloads anything. What `vidl` adds is the part you would otherwise script by
hand every time: working out whether a URL is one video, a playlist or a whole channel; picking
the right subtitle tracks per video instead of guessing one language for a whole batch; sorting a
channel's tabs into folders; and patching what it already downloaded — cover art, missing subtitle
tracks — without re-fetching the media.

```
vidl 'https://www.youtube.com/watch?v=…'
vidl 'https://www.youtube.com/@someone' --into ~/Videos --res 1080
vidl 'https://vimeo.com/12345' --audio
```

## Requirements

| | | |
|---|---|---|
| `yt-dlp` | **required** | Found on `PATH`, or named through `tools::install`. |
| `ffmpeg` + `ffprobe` | **required in practice** | Merging video+audio, muxing subtitle tracks, attaching cover art. Without them you get single-stream downloads and nothing else. |
| `deno` | recommended | YouTube serves obfuscated player JavaScript that computes a media URL's signature. yt-dlp needs a JS runtime to execute it; without one, YouTube downloads lose formats or throttle. |
| `curl` | optional | Only for fetching thumbnails from the image CDN. |

Rust dependencies are just two: `clap` (the CLI) and `lofty` (in-place audio tag writes).

```
cargo build --release      # target/release/vidl
cargo test                 # hermetic: no network. A yt-dlp on PATH adds a few offline
                           # checks of what it does with an output template; without one
                           # those skip with a visible SKIPPED line
./TEST.sh                  # the above plus the live network tests
```

## What it does

**Routing.** A YouTube video, playlist or channel URL each download differently — a channel walks
every tab into its own folder, a playlist reports the entries it *couldn't* fetch instead of
silently skipping them. Any other host downloads flat, since an arbitrary page offers no structure
to build a tree from.

**Subtitles, per video.** YouTube's caption matrix runs to ~157 languages, and which ones exist
differs video to video. Each video is probed for its actual list and only those tracks are
requested, with auto-generated tracks retitled so `English (auto-generated)` can't be mistaken for
the uploader's own. On `--audio`, where the container has nowhere to put a subtitle *stream*, the
text is written into metadata tags instead.

**Idempotent patch passes.** `--thumbnail` and `--subtitles` skip files that already have what
they'd add, and deliberately ignore the download archive, so pointing them at an existing library
patches it in place rather than re-downloading it.

**Names that fit.** Many sites put a whole post caption in the video's title, which is how a
"title" of several hundred characters reaches a filesystem that refuses any path component over
255 bytes — and the download fails outright. The two halves of the crate solve that differently,
because they know different things when the name has to be decided.

On YouTube the fields are predictable — an id is always 11 bytes, format ids are three characters
— so the title is bounded inside the output template and yt-dlp does the naming. That keeps the
`__<id>.` marker the thumbnail and subtitle patch passes locate files by. The budget is derived,
not picked: whatever the limit has left once the separators, the id, the extension and yt-dlp's
in-progress suffixes are reserved, which comes to 187 bytes of title.

Off YouTube nothing is predictable — the title is a caption and yt-dlp builds the id out of the
URL — so instead of guessing, the download goes to a name that cannot overrun (the id alone) and
the real name is put on afterwards, when the title, id and extension are all known and the
in-progress suffixes are gone. Nothing is reserved against ignorance, so the title gets **232 bytes
here against the 151 a bounded template had to settle for**, and a name too long to write stops
being possible rather than merely survivable. A rename that can't happen leaves the file called by
its id — a poor name, not a lost download.

The limit itself is measured from the destination once per run by trying to create a file there —
one probe in the common case. `--name-max <bytes>` overrides it for a mount that accepts less than
it reports (encrypted-filename mounts typically cap near 143).

Whenever a title still doesn't fit, the run says so in red and writes it in full to
`.dl_video_modified` beside the media, so nothing the filename can no longer carry is lost. And on
the YouTube path, where a budget is still a prediction, a name the filesystem refuses anyway earns
one retry at half the title — an uglier name instead of a lost download.

**Collections behind one URL (`--multi`).** Some sites put a whole season on one page, swapping
which video plays without changing the address. yt-dlp resolves such a page to whichever video the
player started on, and the rest are lost silently. `--multi` goes looking: it scans the page for
further references sharing the id shape of the video that *did* resolve, and if it finds several,
downloads them all into a folder of their own, numbered in page order — practically a playlist, and
named like one. Opt-in, so a page that already works cannot be disturbed by it, and off-YouTube
only, since YouTube publishes its collections as playlists and channels that are walked already.

**Failure diagnosis.** A non-zero yt-dlp exit is read for what actually went wrong — geo-block,
age gate, login wall, bot wall, DRM — and answered with the specific lever that helps, if one
exists. A geo-blocked video on a site that honours it triggers a region sweep; one enforced by IP
doesn't, because it can't work. Failures land in a ledger that a later successful run clears.

**Resumable.** Every download is recorded in a per-collection archive, so an interrupted run
picks up where it stopped.

## Using it as a library

The CLI is a thin shell over the library, and both can do the same things:

```rust
// Everything the binary does, from the same arguments it parses.
let code = vidl::run(args);

// Or drive it directly.
let env = vidl::Env { audio: true, res: Some(1080), ..Default::default() };
match vidl::classify(url, false) {
    vidl::Link::Channel { root } => vidl::download_channel(&root, dir, env),
    vidl::Link::Generic         => vidl::download_generic(url, dir, env),
    _                            => vidl::download_video(url, dir, env),
};
```

An embedder with its own command line flattens `vidl::Args` into its parser rather than restating
the flags, and calls `vidl::run`. To override one option — cookies acquired some other way is the
usual case — take `args.env()`, change the field, and drive the `download_*` functions.

Pinned copies of the external tools are declared once at startup:

```rust
vidl::tools::install(vidl::tools::Tools {
    ytdlp: Some("/opt/yt-dlp.pyz".into()),
    python: Some("/opt/python3".into()),   // only for a zipapp yt-dlp
    ffmpeg_dir: Some("/opt/ffmpeg/bin".into()),
    js_runtime: Some("/opt/deno".into()),
});
```

## Limitations

- **Cookies are not acquired here.** `--cookies <file>` and `--cookies-from-browser <spec>` are
  passed to yt-dlp as given. Finding browser profiles and paring a cookie database down to one
  site is a job with too many opinions in it to bury in a download tool.
- **One yt-dlp per process.** `tools::install` writes to a `OnceLock` — the first call wins, later
  ones are ignored. Installation paths don't change mid-run, and threading them through every
  call site would have distorted the whole crate to serve a case that doesn't arise.
- **The clever parts are YouTube-shaped.** Folder trees, channel tabs and the caption matrix are
  YouTube features. Other sites get a flat download with the same quality options, archive and
  failure handling, but no structure.
- **Videos download one at a time.** That costs an extra metadata request each, and buys the
  per-video subtitle selection a single batch invocation cannot express.
- **Folder names aren't bounded.** The title inside a *filename* is capped so it can't overrun the
  255-byte path component limit; a playlist or channel title expanding into a *folder* name isn't.
  Collection titles are far shorter than post captions in practice, and bounding them means
  keeping two independently-computed spellings of the same folder in exact agreement — the scan
  that decides where the archive goes, and the output template that decides where the videos go.
- **`--multi` has to know the embed platform, and only suits some.** *Finding* the other videos on
  a page is generic: the id shape is derived from the video that resolved, not looked up per site.
  Turning a bare id back into something yt-dlp accepts is not — there's no general id-to-URL
  function across yt-dlp's ~1800 extractors — so `src/page/platforms.rs` carries one entry per embed
  platform (Kaltura today). One entry serves every site embedding that platform, but a page using an
  unlisted one has its siblings reported as unreachable rather than guessed at. The table stays
  small for a reason: because videos are found *by id shape*, a platform only qualifies if its ids
  can be told apart from ordinary page text. Kaltura's can (`1_8jx8mama`); Brightcove's are plain
  digits and JW Player's are bare alphanumerics, so neither could be swept for without matching
  half the page.
- **No PO-token provider.** yt-dlp's strongest answer to bot-walling isn't wired up; the failure
  advice says so when it's what you'd need.
- **Linux-shaped.** Developed and tested there. Nothing is deliberately platform-specific beyond
  assuming the external tools are runnable, but nothing else is verified either.

## Licence

See [LICENSE](LICENSE).
