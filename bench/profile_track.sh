#!/usr/bin/env bash
# Profile + bench tracker for the gol-c optimization loop.
#
# Runs the gol-c bench N times, records the median wallclock, then
# does one perf-record pass and categorizes soltty CPU samples by
# subsystem (parser, render, put_char, winit, channel). Appends a row
# to bench/profile_track.tsv keyed by git sha + short message.
#
# Goal: every optimization commit emits one row so the trend over time
# is auditable instead of remembered.
#
# Usage:
#   bench/profile_track.sh                    # 5 bench runs + 1 profile
#   bench/profile_track.sh --runs 10          # more bench runs
#   bench/profile_track.sh --label "try-X"    # custom label

set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
bin="$repo/target/release/soltty"
GOL="${GOL_BIN:-/home/magniff/workspace/gol-c/gol}"
OUT_TSV="$repo/bench/profile_track.tsv"

runs=5
label=""

while [ $# -gt 0 ]; do
    case "$1" in
        --runs) runs="$2"; shift 2 ;;
        --label) label="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,/^$/p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

[ -x "$GOL" ] || { echo "gol binary not at $GOL — set GOL_BIN" >&2; exit 1; }
[ -x "$bin" ] || { ( cd "$repo" && cargo build --release ) >&2; }

sha=$(git -C "$repo" rev-parse --short HEAD)
msg=$(git -C "$repo" log -1 --format=%s)
[ -z "$label" ] && label="$msg"

# Wallclock-only run, returns just the secs
run_bench() {
    local out; out=$(mktemp -t pt.XXXXXX); rm -f "$out"
    timeout --foreground --kill-after=2 25s \
        env SOLTTY_FONT_PX=10 SOLTTY_START_MAXIMIZED=1 "$bin" \
        -e bash -c "GOL_BENCH_ITERS=400 GOL_FORCE_COLS=640 GOL_FORCE_ROWS=150 \
                    GOL_BENCH_OUT=$out $GOL; sleep 0.3" \
        >/dev/null 2>&1
    cat "$out" 2>/dev/null | awk '{print $1}'
    rm -f "$out"
}

# Median of N runs
median_of() {
    sort -g | awk -v n="$1" 'NR==int((n+1)/2){print; exit}'
}

echo "Benching ($runs runs)..." >&2
samples=""
for r in $(seq 1 "$runs"); do
    s=$(run_bench)
    [ -z "$s" ] && { echo "  run $r FAILED" >&2; continue; }
    samples="$samples$s"$'\n'
    printf "  run %d: %ss\n" "$r" "$s" >&2
    sleep 0.8
done
median=$(echo -n "$samples" | sort -g | awk -v n="$runs" 'NR==int((n+1)/2){print; exit}')

# One profile run with category breakdown
echo "Profiling..." >&2
perf_data=$(mktemp -t pp.XXXXXX); rm -f "$perf_data"
timeout --foreground --kill-after=2 30s \
    perf record -F 4000 --call-graph dwarf -o "$perf_data" \
    -- env SOLTTY_FONT_PX=10 SOLTTY_START_MAXIMIZED=1 "$bin" \
    -e bash -c "GOL_BENCH_ITERS=400 GOL_FORCE_COLS=640 GOL_FORCE_ROWS=150 \
                GOL_BENCH_OUT=/tmp/.pt $GOL; sleep 0.3" \
    >/dev/null 2>&1

# Categorize all soltty samples by what subsystem they fall in.
# Buckets are matched on the symbol name; "other_soltty" catches the rest.
cats=$(perf report -i "$perf_data" --stdio --no-children -g none --percent-limit 0.0 2>/dev/null | \
    awk '
        /soltty *soltty/ {
            pct = $1 + 0
            fn = $NF
            total += pct
            if (fn ~ /put_char|put_ascii_fast/) put_char += pct
            else if (fn ~ /Term::feed|apply_sgr|inline_csi|parse_three_u8|vte::/) parser += pct
            else if (fn ~ /prepare|compute_damage|draw_arrays|append_/) render += pct
            else if (fn ~ /try_recv|Sender::send|Pty::drain|mpmc/) channel += pct
            else if (fn ~ /winit::|ApplicationHandler|user_event|window_event|run_app/) winit += pct
            else if (fn ~ /FnMut|call_mut|FnOnce/) closure += pct
            else other += pct
        }
        END {
            printf "%.2f\t%.2f\t%.2f\t%.2f\t%.2f\t%.2f\t%.2f\t%.2f",
                total, parser, put_char, render, channel, winit, closure, other
        }')
rm -f "$perf_data" /tmp/.pt

# Header if file is new
if [ ! -f "$OUT_TSV" ]; then
    printf "ts\tsha\tlabel\twallclock_median\truns\tsoltty_total\tparser\tput_char\trender\tchannel\twinit\tclosure\tother\n" > "$OUT_TSV"
fi
ts=$(date -u +%Y-%m-%dT%H:%M:%SZ)
printf "%s\t%s\t%s\t%s\t%d\t%s\n" "$ts" "$sha" "$label" "$median" "$runs" "$cats" >> "$OUT_TSV"

echo
echo "Recorded: $OUT_TSV"
echo
echo "Last 6 entries:"
column -t -s $'\t' "$OUT_TSV" | tail -7
