#!/usr/bin/env bash
# TEST.sh — run ALL of vidl's tests, including the live ones `cargo test` skips.
#
# Three categories, cheapest first:
#   1. offline        unit + integration (a scripted yt-dlp stand-in on PATH) — hermetic, fast
#   2. live-quick     real YouTube downloads of a tiny public video, no cookies
#   3. live-extended  real YouTube plus, for one test, a signed-in session you supply — either a
#                     browser store (tests/cookies/browser.spec + store/) or a Netscape jar
#                     (tests/cookies/youtube.txt), or VIDL_TEST_COOKIES pointing at either. That
#                     test skips-with-notice without one; see tests/cookies/README.md.
#                     Serialized and paced: it uses a real session sparingly.
#
# Live categories can fail for environmental reasons (offline, rate-limited, region blocks);
# every category runs regardless, and the summary at the end shows what passed where.
set -uo pipefail

command -v cargo >/dev/null 2>&1 || { printf 'ERROR: cargo not found in PATH.\n' >&2; exit 1; }
cd "$(dirname "$0")"

for tool in yt-dlp ffmpeg ffprobe; do
    command -v "$tool" >/dev/null 2>&1 || printf 'note: %s not on PATH — the live categories will fail without it\n' "$tool" >&2
done

declare -A RESULT

run_category() {
    local name="$1"; shift
    echo
    echo "=== ${name} ==="
    if "$@"; then
        RESULT[$name]="PASS"
    else
        RESULT[$name]="FAIL"
    fi
}

# --no-fail-fast: run every test binary even when one fails — a red target must not hide the rest.
# --nocapture: several tests are skip-with-notice (they self-skip, printing a "SKIPPED …" line,
# when ffmpeg or a cookie jar is unavailable). Without --nocapture libtest hides a passing test's
# output, so a silent skip would read as a clean pass — grep the run for "SKIPPED" to see exactly
# which, if any, opted out.
run_category "offline"       cargo test --no-fail-fast -- --nocapture
run_category "live-quick"    cargo test --test media_flags   -- --ignored --test-threads=1 --nocapture
run_category "live-extended" cargo test --test live_extended -- --ignored --test-threads=1 --nocapture

echo
echo "=== summary ==="
failed=0
for name in offline live-quick live-extended; do
    printf '  %-14s %s\n' "$name" "${RESULT[$name]}"
    [ "${RESULT[$name]}" = "FAIL" ] && failed=1
done
if [ -z "${VIDL_TEST_COOKIES:-}" ] && ! [ -s tests/cookies/browser.spec ] && ! [ -s tests/cookies/youtube.txt ]; then
    echo
    echo "note: no cookies supplied, so the age-restricted test self-skipped (SKIPPED line above)"
    echo "      — see tests/cookies/README.md for full coverage."
fi
exit "$failed"
