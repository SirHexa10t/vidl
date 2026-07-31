# Test cookies

One test — `live_extended::an_age_restricted_video_downloads_with_supplied_cookies` — needs a real
signed-in YouTube session to get past an age gate. There is no way to test that honestly without
one, so the session is supplied out of band and never committed.

**Everything in this directory except this file is gitignored.** Check before you commit anyway.

Without a session the test prints a `SKIPPED` line and passes. Nothing else in the suite needs
cookies.

## Two accepted shapes

Either works; the browser store is tried first.

### A browser store — `browser.spec` + `store/`

What a tool that copies a browser profile's cookies produces, and the shape an embedder hands
this crate. `browser.spec` holds just the browser name; `store/` holds its cookie database:

```
tests/cookies/browser.spec        # e.g. a single line: firefox
tests/cookies/store/cookies.sqlite
```

Becomes `--cookies-from-browser firefox:<…>/tests/cookies/store`.

If you already have such a store somewhere, point at its directory rather than copying it — this
keeps the live session out of the checkout entirely, and yt-dlp re-reads the database each run
instead of working from a snapshot that goes stale:

```sh
VIDL_TEST_COOKIES=~/path/to/that/store-dir ./TEST.sh
```

The directory you name is the one holding `browser.spec` and `store/`.

Reading a *copy* is deliberate — pointing yt-dlp at a live browser profile fails when the browser
has the database locked.

### A Netscape jar — `youtube.txt`

The portable shape, and what browser extensions export:

```
tests/cookies/youtube.txt
```

Becomes `--cookies <…>/tests/cookies/youtube.txt`. `VIDL_TEST_COOKIES` may also point straight at
a jar file rather than a directory.

yt-dlp will produce one from a browser:

```sh
yt-dlp --cookies-from-browser firefox --cookies tests/cookies/youtube.txt
```

That exits non-zero with "You must provide at least one URL" — expected, and the file is already
written by then.

Format is tab-separated, one cookie per line, `#` for comments:

```
# Netscape HTTP Cookie File
.youtube.com	TRUE	/	TRUE	1799999999	SID	<value>
```

## Exporting cleanly

Whichever shape: open a private window, sign in, visit only `youtube.com/robots.txt`, export, then
close the window. That avoids sweeping up unrelated sites, and YouTube rotates cookies on open
tabs — a session you keep using goes stale sooner.

## When it doesn't take

A store with an empty `store/`, or a jar with no non-comment rows, counts as **not supplied** —
the test skips rather than failing at the gate, so a half-finished setup reads as missing rather
than as a broken session.

## Handling

This is a live credential. It grants access to the account it came from.

- Never commit it, paste it into an issue, or attach it to a bug report.
- Prefer `VIDL_TEST_COOKIES` pointing outside the repo.
- Revoke by signing out of the browser profile you exported from — that invalidates the session.
