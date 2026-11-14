#!/usr/bin/env bash
# P0-A GPUI spike measurements (ROADMAP P0-A, docs/spikes/p0-a-gpui.md).
#
# Builds the spike in release mode, runs its scripted benchmark (NINESQL_SPIKE_AUTO=1) a few
# times and prints a Markdown table of medians: cold start to first frame, RSS (first frame,
# idle with an empty table, 1 M rows loaded, after scrolling) and frame times while scrolling
# and jumping through 1 M rows.
#
# Usage (from the repository root):
#   crates/ninesql-ui/examples/spike-measure.sh                 # current session (Wayland or X11)
#   SPIKE_SESSION=x11 crates/ninesql-ui/examples/spike-measure.sh       # force X11 / XWayland
#   SPIKE_SESSION=wayland crates/ninesql-ui/examples/spike-measure.sh   # force Wayland
#   SPIKE_HEADLESS=x11 ...     # start a private Xvfb (software rendering, no real GPU)
#   SPIKE_HEADLESS=wayland ... # private Xvfb + nested weston (software rendering)
#
# Other knobs: SPIKE_RUNS (default 3), SPIKE_ROWS (default 1000000), SPIKE_SCROLL_FRAMES
# (default 600), SPIKE_OUT (directory for raw logs, default a fresh mktemp dir),
# SPIKE_SCREENSHOT=1 (X11 or SPIKE_HEADLESS only: `import -window root` while the benchmark
# is scrolling, needs ImageMagick),
# SPIKE_DROP_CACHES=1 (needs sudo: drop the page cache before each run for a true cold start).
#
# Frame times on a compositor with vsync are capped by the refresh rate (16.7 ms at 60 Hz):
# read p95 / p99 / max for hitches. Under Xvfb / headless weston rendering is software
# (Mesa llvmpipe / lavapipe) and the numbers are NOT representative of a real GPU.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
RUNS="${SPIKE_RUNS:-3}"
OUT="${SPIKE_OUT:-$(mktemp -d -t ninesql-spike-XXXXXX)}"
BIN="$ROOT/target/release/examples/spike"
mkdir -p "$OUT"

started_pids=()
cleanup() {
    for pid in "${started_pids[@]:-}"; do
        [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
    done
}
trap cleanup EXIT

echo "Building the spike (release)..." >&2
(cd "$ROOT" && cargo build --locked -p ninesql-ui --example spike --release >&2)

start_xvfb() {
    Xvfb "$1" -screen 0 1600x1000x24 -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
    started_pids+=("$!")
    sleep 2
}

case "${SPIKE_HEADLESS:-}" in
    x11)
        start_xvfb :142
        export DISPLAY=:142
        unset WAYLAND_DISPLAY
        ;;
    wayland)
        # weston's headless backend exposes no wl_seat and GPUI panics without one, so weston
        # runs nested (x11 backend, pixman renderer) inside a private Xvfb instead.
        start_xvfb :142
        export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-$(mktemp -d)}"
        DISPLAY=:142 weston --backend=x11 --width=1500 --height=950 --socket=ninesql-spike \
            --renderer=pixman >"$OUT/weston.log" 2>&1 &
        started_pids+=("$!")
        sleep 3
        export WAYLAND_DISPLAY=ninesql-spike
        unset DISPLAY
        ;;
    "") ;;
    *) echo "SPIKE_HEADLESS must be x11 or wayland" >&2; exit 2 ;;
esac

case "${SPIKE_SESSION:-}" in
    x11) unset WAYLAND_DISPLAY ;;
    wayland) unset DISPLAY ;;
    "") ;;
    *) echo "SPIKE_SESSION must be x11 or wayland" >&2; exit 2 ;;
esac

if [ -n "${WAYLAND_DISPLAY:-}" ]; then session="wayland ($WAYLAND_DISPLAY)"; else session="x11 (${DISPLAY:-none})"; fi
gpu="$(vulkaninfo --summary 2>/dev/null | awk -F'= ' '/deviceName/ {print $2; exit}' || true)"
echo "Session: $session; Vulkan device: ${gpu:-unknown}; runs: $RUNS; logs: $OUT" >&2

for i in $(seq 1 "$RUNS"); do
    if [ "${SPIKE_DROP_CACHES:-0}" = 1 ]; then
        sync && echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null
    fi
    log="$OUT/run-$i.log"
    t0=$(date +%s%N)
    NINESQL_SPIKE_AUTO=1 \
        NINESQL_SPIKE_ROWS="${SPIKE_ROWS:-1000000}" \
        NINESQL_SPIKE_SCROLL_FRAMES="${SPIKE_SCROLL_FRAMES:-600}" \
            timeout 600 "$BIN" >"$log" 2>&1 &
    spike_pid=$!
    shot_display="${DISPLAY:-}"
    [ "${SPIKE_HEADLESS:-}" = wayland ] && shot_display=:142
    if [ "${SPIKE_SCREENSHOT:-0}" = 1 ] && [ -n "$shot_display" ]; then
        sleep 6 && DISPLAY="$shot_display" import -window root "$OUT/screenshot-$i.png" 2>/dev/null || true
    fi
    wait "$spike_pid" || echo "run $i exited with $?" >&2
    t1=$(date +%s%N)
    echo "spike-metric wall_total_ms=$(((t1 - t0) / 1000000))" >>"$log"
done

# Median of a numeric metric across runs. $1 = key, $2 = optional field inside the value.
median() {
    local key="$1" field="${2:-}"
    grep -h "^spike-metric $key=" "$OUT"/run-*.log 2>/dev/null |
        sed "s/^spike-metric $key=//" |
        if [ -n "$field" ]; then grep -oE "(^| )$field=[0-9.]+" | cut -d= -f2; else sed 's/MiB$//'; fi |
        sort -n | awk '{a[NR]=$1} END {if (NR==0) print "n/a"; else print a[int((NR+1)/2)]}'
}

cat <<EOF

### GPUI spike measurements: $session, ${gpu:-unknown GPU}, median of $RUNS runs

| Metric | Value |
|---|---|
| App init (main to platform ready) | $(median app_init_ms) ms |
| Cold start to first frame | $(median first_frame_ms) ms |
| RSS at first frame | $(median rss_first_frame) MiB |
| RSS idle, empty table (2 s after first frame) | $(median rss_idle_empty_table) MiB |
| RSS with ${SPIKE_ROWS:-1000000} rows loaded | $(median rss_rows_loaded) MiB |
| RSS after scrolling | $(median rss_after_scroll) MiB |
| RSS after random jumps | $(median rss_after_jump) MiB |
| Scroll frame time mean / p50 / p95 / p99 / max | $(median frames_scroll mean) / $(median frames_scroll p50) / $(median frames_scroll p95) / $(median frames_scroll p99) / $(median frames_scroll max) ms |
| Scroll mean FPS | $(median frames_scroll fps_mean) |
| Random-jump frame time mean / p95 / max | $(median frames_jump mean) / $(median frames_jump p95) / $(median frames_jump max) ms |
| Clipboard round trip | $(grep -h '^spike-metric clipboard_roundtrip=' "$OUT"/run-*.log | sed 's/.*=//' | sort | uniq -c | tr '\n' ' ') |
EOF
