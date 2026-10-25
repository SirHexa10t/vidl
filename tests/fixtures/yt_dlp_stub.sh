#!/usr/bin/env bash
# A yt-dlp stand-in for the offline tests (tests/stubbed_flows.rs, tests/media_flags.rs):
# deterministic and network-free. The tests put this first on PATH (as `yt-dlp`) under a scratch
# HOME with no bundled tools, so the binary's own resolution lands here.
#
# Bash builtins only — no cat/cp/touch — so it also runs on a HERMETIC PATH holding nothing but
# {yt-dlp, python3, bash}, which is what the cookie-readability rig gives it.
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
#     range_refused_unless_restarted  refuse a resume the way a server does when the leftover
#                     .part is already as long as the remote file, until told --no-continue
#     range_refused_then_other  the same refusal, but the restart hits an unrelated error —
#                     observed for real when ffmpeg was missing
#     format_missing_unless_checked  the media 404 a CDN gives for a transcode that is listed but
#                     absent, until --check-formats makes yt-dlp pick a servable one
#
# Probe invocations (--print) succeed: --flat-playlist replays scan.txt, a --playlist-items probe
# replays probe.txt — then, when probe_errors.txt holds refusals, prints them to stderr and exits 1,
# as yt-dlp does under --ignore-errors once any entry was refused — and a lone-video probe prints
# video_probe.txt when present (else a bare id/NA/NA/NA) — except a repair's question about what
# YouTube offers now (told apart by the chapters field only it prints), which replays facts.txt and,
# when facts_errors.txt holds refusals, prints them to stderr and exits 1, as yt-dlp does under
# --ignore-errors once any video was refused. A subtitle fetch (--skip-download) writes the sidecar
# of each requested language listed in subs_served.txt, named as yt-dlp names it, and none for the
# rest, as a 429'd track leaves none; a subs_refused file fails the whole fetch. A SUCCESSFUL
# download mimics the real yt-dlp's side effects: it appends archive_adds.txt (if present) to the
# run's --download-archive (the collection post-mortem reads it), appends names_adds.txt (if
# present) to the run's --print-to-file target (the shortened-name record reads that), and copies
# cookie_dump.txt (if present) to a `--cookies <file>` target (the cookie-readability check reads
# that).

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
if [[ "$args" == *"%(chapters)+j"* ]]; then
    emit "$dir/facts.txt"
    if [[ -s "$dir/facts_errors.txt" ]]; then
        printf '%s\n' "$(<"$dir/facts_errors.txt")" >&2
        exit 1
    fi
    exit 0
fi
if [[ "$args" == *" --skip-download "* ]]; then
    if [[ -e "$dir/subs_refused" ]]; then
        echo "ERROR: Unable to download video subtitles for 'en': HTTP Error 429: Too Many Requests" >&2
        exit 1
    fi
    output=""
    langs=""
    prev=""
    for arg in "$@"; do
        if [[ "$prev" == "--output" ]]; then output="$arg"; fi
        if [[ "$prev" == "--sub-langs" ]]; then langs="$arg"; fi
        prev="$arg"
    done
    served=" "
    if [[ -s "$dir/subs_served.txt" ]]; then
        served=" $(<"$dir/subs_served.txt") "
        served="${served//$'\n'/ }"
    fi
    IFS=',' read -r -a wanted <<< "$langs"
    for lang in "${wanted[@]}"; do
        if [[ "$served" == *" $lang "* ]]; then
            printf 'WEBVTT\n\n00:00.000 --> 00:01.000\nserved %s\n' "$lang" > "${output%.%(ext)s}.$lang.vtt"
        fi
    done
    exit 0
fi
if [[ "$args" == *" --print "* ]]; then
    if [[ "$args" == *" --playlist-items "* ]]; then
        emit "$dir/probe.txt"
        if [[ -s "$dir/probe_errors.txt" ]]; then
            printf '%s\n' "$(<"$dir/probe_errors.txt")" >&2
            exit 1
        fi
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
    range_refused_unless_restarted)
        # A leftover `.part` at least as long as the remote file: the resume asks for a byte range
        # that cannot exist, and asks for exactly the same one every time. Keyed on the ARGV rather
        # than a call counter, like nametoolong_unless_shorter and for the same reason — a plain
        # re-run of an unchanged command is refused just as the first attempt was, and only a
        # request that stops trying to resume gets through. The resume offset goes to STDOUT, where
        # the real yt-dlp reports it.
        if [[ "$args" == *" --no-continue "* ]]; then succeed; fi
        echo "[download] Resuming download at byte 749233884"
        echo "ERROR: unable to download video data: HTTP Error 416: Requested Range Not Satisfiable" >&2
        exit 1 ;;
    range_refused_then_other)
        # The restart gets past the stale file and the download then dies of something unrelated.
        # Seen in a real run: the resume was refused, starting over fetched the whole file, and the
        # postprocessor failed because ffmpeg was not installed.
        if [[ "$args" == *" --no-continue "* ]]; then
            echo "ERROR: ffmpeg not found. Please install or provide the path using --ffmpeg-location" >&2
            exit 1
        fi
        echo "[download] Resuming download at byte 749233884"
        echo "ERROR: unable to download video data: HTTP Error 416: Requested Range Not Satisfiable" >&2
        exit 1 ;;
    format_missing_unless_checked)
        # The highest-ranked format is listed by the site but missing at its origin, so the media
        # 404s while the video itself is fine. Keyed on the ARGV, like the other rescues: a plain
        # re-run asks for the same dead file and gets the same answer, and only a request that
        # checks before committing gets through.
        if [[ "$args" == *" --check-formats "* ]]; then succeed; fi
        echo "ERROR: unable to download video data: HTTP Error 404: Not Found" >&2
        exit 1 ;;
    *)
        echo "yt-dlp stub: unknown VIDL_STUB_MODE '${VIDL_STUB_MODE}'" >&2
        exit 2 ;;
esac
