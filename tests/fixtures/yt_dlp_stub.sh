#!/usr/bin/env bash
# A yt-dlp stand-in for the offline tests (tests/dl_stubbed_flows.rs, tests/dl_cookie_import.rs):
# deterministic and network-free. The tests put this first on PATH (as `yt-dlp`) under a scratch
# HOME with no bundled tools, so the binary's own resolution lands here.
#
# Bash builtins only — no cat/cp/touch — so it also runs on the cookie-import rig's HERMETIC
# PATH, which holds nothing but {yt-dlp, python3, bash}.
#
# Contract:
#   VIDL_STUB_DIR   scratch dir — every invocation's argv is appended to calls.log; scan.txt
#                     and probe.txt hold the --flat-playlist / batch-probe outputs to replay;
#                     page.txt and page_id.txt hold what a `--multi` page scan finds; a
#                     refuse_scan_once file makes the FIRST scan of any kind fail with a 403.
#   VIDL_STUB_MODE  how a *download* invocation (one with no --print) behaves:
#     ok                 succeed
#     fail               fail with a generic error
#     geo                fail with YouTube's geo-restriction phrasing
#     geo_unless_xff_us  fail with a geo phrasing unless invoked with `--xff US`
#     fail_once_then_ok  fail the first download call, succeed on any retry
#     members|age|botwall|drm  fail with that gate's real phrasing
#     nametoolong        fail the way the OS does when a path component won't fit
#     nametoolong_unless_shorter  refuse until the --output template asks for a shorter title
#
# Probe invocations (--print) always succeed: --flat-playlist replays scan.txt, a
# --playlist-items probe replays probe.txt, and a lone-video probe prints video_probe.txt when
# present (else a bare id/NA/NA/NA). A SUCCESSFUL download mimics the real yt-dlp's side effects:
# it appends archive_adds.txt (if present) to the run's --download-archive (the collection
# post-mortem reads it), appends names_adds.txt (if present) to the run's --print-to-file target
# (the shortened-name record reads that), and copies cookie_dump.txt (if present) to a
# `--cookies <file>` target (the cookie-import readability check reads that).

args=" $* "
dir="${VIDL_STUB_DIR:?VIDL_STUB_DIR must point at the test scratch dir}"
printf '%s\n' "$*" >> "$dir/calls.log"

# Print a replay file's contents — the builtin stand-in for `cat`. (`$(<f)` strips trailing
# newlines and printf restores one; line-oriented consumers see identical text. An absent or
# empty file prints nothing, like `cat` on an empty file.)
emit() {
    if [[ -s "$1" ]]; then printf '%s\n' "$(<"$1")"; fi
}

# A fetch refused on the FIRST ask and fine on the next — the transient a CDN produces for a cold
# client, and the reason a scan is asked more than once. Every kind of scan can draw it, so every
# kind of scan calls this. The "already refused" marker is written rather than the request one
# deleted, because this script is restricted to bash builtins.
refuse_once_if_asked() {
    if [[ -e "$dir/refuse_scan_once" && ! -e "$dir/scan_was_refused" ]]; then
        : > "$dir/scan_was_refused"
        echo "ERROR: [generic] x: Unable to download webpage: HTTP Error 403: Forbidden" >&2
        exit 1
    fi
}

# A flat scan. Channel-tab URLs get per-tab behaviour so one channel run exercises every
# TabScan outcome: videos → the scan, shorts → yt-dlp's "no such tab" error, streams → a hard
# failure, playlists → scan_playlists.txt (empty ⇒ a reachable-but-empty tab).
if [[ "$args" == *" --flat-playlist "* ]]; then
    refuse_once_if_asked
    case "$args" in
        *"/shorts "*)
            echo "ERROR: [youtube:tab] @stub: This channel does not have a shorts tab" >&2
            exit 1 ;;
        *"/streams "*)
            echo "ERROR: Unable to download webpage" >&2
            exit 1 ;;
        *"/playlists "*)
            emit "$dir/scan_playlists.txt"
            exit 0 ;;
        *)
            emit "$dir/scan.txt"
            exit 0 ;;
    esac
fi
# The `--multi` page scan. The real yt-dlp fetches the page and leaves it in the working directory;
# this writes page.txt there under a plausible dump name and prints the id the page resolved to,
# so the sibling sweep has something to read. Checked before the general --print branch, which this
# invocation also matches.
if [[ "$args" == *" --write-pages "* ]]; then
    refuse_once_if_asked
    if [[ -f "$dir/page.txt" ]]; then
        # yt-dlp names each dump after the URL it fetched, and the sibling sweep picks the page out
        # of the platform's other responses BY HOST — so the stand-in has to carry the host too.
        for last in "$@"; do :; done
        host="${last#*://}"
        host="${host%%/*}"
        printf '%s\n' "$(<"$dir/page.txt")" > "./page_https_-_${host}_x.dump"
    fi
    emit "$dir/page_id.txt"
    exit 0
fi
if [[ "$args" == *" --print "* ]]; then
    if [[ "$args" == *" --playlist-items "* ]]; then
        emit "$dir/probe.txt"
    elif [[ -f "$dir/video_probe.txt" ]]; then
        emit "$dir/video_probe.txt"
    else
        printf 'stubvid0000\nNA\nNA\nNA\n'
    fi
    exit 0
fi

# A download invocation. Locate the --download-archive, --cookies and --print-to-file values so
# success can reproduce the real yt-dlp's side effects on them. `--print-to-file` takes TWO
# operands (TEMPLATE then FILE), so its target sits two positions along rather than one.
archive=""
cookies_out=""
names_out=""
prev=""
prev2=""
for arg in "$@"; do
    if [[ "$prev" == "--download-archive" ]]; then archive="$arg"; fi
    if [[ "$prev" == "--cookies" ]]; then cookies_out="$arg"; fi
    if [[ "$prev2" == "--print-to-file" ]]; then names_out="$arg"; fi
    prev2="$prev"
    prev="$arg"
done
succeed() {
    if [[ -n "$archive" && -f "$dir/archive_adds.txt" ]]; then
        printf '%s\n' "$(<"$dir/archive_adds.txt")" >> "$archive"
    fi
    if [[ -n "$cookies_out" && -f "$dir/cookie_dump.txt" ]]; then
        printf '%s\n' "$(<"$dir/cookie_dump.txt")" > "$cookies_out"
    fi
    # What each downloaded entry was called, as the real yt-dlp's `--print-to-file after_move:`
    # appends it: one `<id>\t<filepath>\t<title>` line per video, the title unbounded (the whole
    # point — it is what a file name could not keep). Each recorded file is also *created*, since
    # a real download leaves one behind and the generic path's rename pass has to find it there.
    if [[ -n "$names_out" && -f "$dir/names_adds.txt" ]]; then
        printf '%s\n' "$(<"$dir/names_adds.txt")" >> "$names_out"
        while IFS=$'\t' read -r _id path _title || [[ -n "$_id" ]]; do
            if [[ -n "$path" ]]; then : > "$path"; fi
        done < "$dir/names_adds.txt"
    fi
    exit 0
}

case "${VIDL_STUB_MODE:-ok}" in
    ok)
        succeed ;;
    fail)
        echo "ERROR: stub failure" >&2
        exit 1 ;;
    geo)
        echo "ERROR: [youtube] stubvid0000: The uploader has not made this video available in your country" >&2
        exit 1 ;;
    geo_unless_xff_us)
        if [[ "$args" == *" --xff US "* ]]; then succeed; fi
        echo "ERROR: [generic] v1: This video is not available from your location due to geo restriction" >&2
        exit 1 ;;
    fail_once_then_ok)
        if [[ -e "$dir/failed_once" ]]; then succeed; fi
        : > "$dir/failed_once"
        echo "ERROR: transient stub failure" >&2
        exit 1 ;;
    members)
        echo "ERROR: Join this channel to get access to members-only content like this video" >&2
        exit 1 ;;
    age)
        echo "ERROR: Sign in to confirm your age. This video may be inappropriate for some users" >&2
        exit 1 ;;
    botwall)
        echo "ERROR: [youtube] stubvid0000: Sign in to confirm you're not a bot" >&2
        exit 1 ;;
    drm)
        echo "ERROR: [youtube] stubvid0000: This video is DRM protected" >&2
        exit 1 ;;
    nametoolong)
        # The real thing, verbatim in shape: the OS refusing a path component, reported by yt-dlp
        # against the in-progress `.part` name rather than the final one.
        echo "ERROR: unable to open for writing: [Errno 36] File name too long: '/dl/aaaa [1].mp4.part'" >&2
        exit 1 ;;
    nametoolong_unless_shorter)
        # Refuse while the template still asks for a long title; accept once it asks for little
        # enough. Keyed on the ARGV, not on a call counter, because that is what a real filesystem
        # does — the plain diagnostic re-run of an unchanged name is refused exactly as the first
        # attempt was, and only a genuinely shorter template gets through.
        bound="${args#*%(title).}"
        bound="${bound%%B*}"
        if [[ "$bound" =~ ^[0-9]+$ ]] && (( bound <= 100 )); then succeed; fi
        echo "ERROR: unable to open for writing: [Errno 36] File name too long: '/dl/aaaa [1].mp4.part'" >&2
        exit 1 ;;
    *)
        echo "yt-dlp stub: unknown VIDL_STUB_MODE '${VIDL_STUB_MODE}'" >&2
        exit 2 ;;
esac
