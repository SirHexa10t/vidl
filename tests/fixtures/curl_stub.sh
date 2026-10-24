#!/usr/bin/env bash
# A curl stand-in for the offline tests (tests/stubbed_flows.rs). The rig installs it beside the
# yt-dlp one, first on the child's PATH, so nothing a run fetches can leave the machine — the
# thumbnail pass every YouTube run makes being the caller. Each invocation's argv is appended to
# curl.log in VIDL_STUB_DIR. Each fails the way `curl -f` does on an HTTP error (exit 22) — what
# the pass meets when the CDN has no image for an id — unless the test put a picture at thumb.jpg
# there, which is then served to the `-o` target, as the image server answering.
#
# Bash builtins only, like the yt-dlp stand-in, but for the `cp` serving that picture: a JPEG's
# bytes hold NULs no builtin can carry.

dir="${VIDL_STUB_DIR:?VIDL_STUB_DIR must point at the test scratch dir}"
printf '%s\n' "$*" >> "$dir/curl.log"
if [[ -f "$dir/thumb.jpg" ]]; then
    target=""
    prev=""
    for arg in "$@"; do
        if [[ "$prev" == "-o" ]]; then target="$arg"; fi
        prev="$arg"
    done
    cp -- "$dir/thumb.jpg" "$target" && exit 0
fi
exit 22
