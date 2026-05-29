#!/usr/bin/env bash
# Run each bench under soltty and alacritty, print a side-by-side
# comparison. Each bench writes "<name> <secs> <iters> <cols> <rows>
# <bytes>" to a temp file inside the terminal session — we read it
# back after the terminal process exits.
#
# Usage:
#   ./bench/compare.sh                     # default iters per bench
#   ./bench/compare.sh --iters 1000        # override BENCH_ITERS for all
#   ./bench/compare.sh --runs 3            # average of 3 runs each
#   ./bench/compare.sh --terms soltty      # only soltty
#   ./bench/compare.sh --bench truecolor_grid     # one bench only
#   ./bench/compare.sh --cols 200 --rows 60       # force grid size
#   ./bench/compare.sh --gpu-sample        # sample nvidia-smi util.gpu
#                                          #   alongside each run; appends
#                                          #   mean/p95 % to result lines.

set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
bin_dir="$repo/target/release"

iters_override=""
runs=1
selected_bench=""
terms=(soltty alacritty ghostty)
cols_override=""
rows_override=""
gpu_sample=0

while [ $# -gt 0 ]; do
    case "$1" in
        --iters) iters_override="$2"; shift 2 ;;
        --runs) runs="$2"; shift 2 ;;
        --bench) selected_bench="$2"; shift 2 ;;
        --terms) IFS=',' read -ra terms <<< "$2"; shift 2 ;;
        --cols) cols_override="$2"; shift 2 ;;
        --rows) rows_override="$2"; shift 2 ;;
        --gpu-sample) gpu_sample=1; shift ;;
        -h|--help)
            sed -n '2,/^$/p' "$0" | sed 's/^# \?//'
            exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if [ "$gpu_sample" = "1" ]; then
    if ! command -v nvidia-smi >/dev/null; then
        echo "--gpu-sample requested but nvidia-smi not on PATH" >&2
        exit 1
    fi
fi

# Number of leading nvidia-smi samples to discard. The first ~1s of -lms
# 100 output is settling/warmup — the GPU clocks aren't ramped yet and
# the sampler itself has cold-cache overhead.
GPU_WARMUP_SAMPLES=10

all_benches=(raw_throughput truecolor_grid scroll_storm cursor_jumps glyph_churn)
# gpu_load is intentionally NOT in all_benches: it's wallclock-bounded
# rather than iteration-bounded and only meaningful with --gpu-sample,
# so it has to be requested explicitly via --bench gpu_load.
opt_in_benches=(gpu_load)
if [ -n "$selected_bench" ]; then
    # Validate against the union of always-run and opt-in benches.
    found=0
    for b in "${all_benches[@]}" "${opt_in_benches[@]}"; do
        [ "$b" = "$selected_bench" ] && found=1 && break
    done
    if [ "$found" = "0" ]; then
        echo "unknown bench: $selected_bench" >&2
        exit 2
    fi
    benches=("$selected_bench")
else
    benches=("${all_benches[@]}")
fi

# Verify each requested terminal exists. soltty is local; alacritty
# may or may not be installed; ghostty ships as a macOS .app bundle.
GHOSTTY_APP="/Applications/Ghostty.app"
for t in "${terms[@]}"; do
    case "$t" in
        soltty)    [ -x "$bin_dir/soltty" ] || { echo "missing $bin_dir/soltty — run cargo build --release" >&2; exit 1; } ;;
        alacritty) command -v alacritty >/dev/null || { echo "alacritty not on PATH; pass --terms soltty to skip it" >&2; exit 1; } ;;
        ghostty)   [ -d "$GHOSTTY_APP" ] || { echo "Ghostty.app not in /Applications" >&2; exit 1; } ;;
        *) echo "unknown terminal: $t" >&2; exit 2 ;;
    esac
done

echo "Building benches..."
( cd "$repo" && cargo build --release --quiet -p soltty-bench )

# One temp file per (bench, term, run) so concurrent runs (we don't do
# that yet) don't clobber each other.
#
# When --gpu-sample is set, a parallel nvidia-smi sampler writes
# utilization.gpu % every 100 ms to a sibling file. After the terminal
# subprocess returns we kill the sampler, aggregate the samples (drop
# warmup, compute mean+p95), and append "<gpu_mean> <gpu_p95>" to the
# bench result line. Existing 6-field parsers ignore the extras.
run_one() {
    local bench="$1" term="$2"
    local out; out=$(mktemp -t soltty_compare.XXXXXX)
    rm -f "$out"  # bench writes fresh; remove now to avoid stale read

    local gpu_log="" sampler_pid=""
    if [ "$gpu_sample" = "1" ]; then
        gpu_log=$(mktemp -t soltty_gpu.XXXXXX)
        # File mode (not pipe) so killing the sampler doesn't SIGPIPE.
        # 2>/dev/null swallows nvidia-smi's "terminated" message on TERM.
        nvidia-smi \
            --query-gpu=utilization.gpu \
            --format=csv,noheader,nounits \
            -lms 100 \
            > "$gpu_log" 2>/dev/null &
        sampler_pid=$!
    fi

    # Build the env-var prefix once.
    local env_prefix="BENCH_OUT=$out"
    [ -n "$iters_override" ] && env_prefix="BENCH_ITERS=$iters_override $env_prefix"
    [ -n "$cols_override"  ] && env_prefix="BENCH_COLS=$cols_override $env_prefix"
    [ -n "$rows_override"  ] && env_prefix="BENCH_ROWS=$rows_override $env_prefix"

    local inner="$env_prefix $bin_dir/$bench"
    case "$term" in
        soltty)
            "$bin_dir/soltty" -e bash -lc "$inner" >/dev/null 2>&1
            ;;
        alacritty)
            alacritty -e bash -lc "$inner" >/dev/null 2>&1
            ;;
        ghostty)
            # Ghostty's CLI launcher returns immediately on macOS — the
            # actual terminal lives in /Applications/Ghostty.app and is
            # spawned via `open`. `-n` forces a fresh instance and `-W`
            # blocks until it terminates, so the bench result file is
            # ready by the time we return. Cold-spawn cost is ~3s of
            # overhead per run, but the bench measures itself.
            open -nWa "$GHOSTTY_APP" --args -e bash -lc "$inner" >/dev/null 2>&1
            ;;
    esac

    if [ -n "$sampler_pid" ]; then
        kill -TERM "$sampler_pid" 2>/dev/null || true
        wait "$sampler_pid" 2>/dev/null || true
    fi

    if [ ! -s "$out" ]; then
        echo "  $term: FAILED — no result file produced" >&2
        rm -f "$gpu_log" 2>/dev/null
        return 1
    fi

    local bench_line
    bench_line=$(cat "$out")
    if [ -n "$gpu_log" ] && [ -s "$gpu_log" ]; then
        local gpu_stats
        gpu_stats=$(awk -v warmup="$GPU_WARMUP_SAMPLES" '
            { v = $1+0; raw[NR] = v }
            END {
                start = warmup + 1
                n = NR - warmup
                if (n <= 0) { printf "0.0 0.0"; exit }
                # Copy + selection sort into samples[]. NR rarely exceeds
                # a few hundred so the O(n^2) sort is fine.
                for (i = 1; i <= n; i++) samples[i] = raw[start + i - 1]
                for (i = 1; i <= n; i++) {
                    for (j = i+1; j <= n; j++) {
                        if (samples[j] < samples[i]) {
                            t = samples[i]; samples[i] = samples[j]; samples[j] = t
                        }
                    }
                    sum += samples[i]
                }
                mean = sum / n
                p95_idx = int(n * 0.95 + 0.5)
                if (p95_idx < 1) p95_idx = 1
                if (p95_idx > n) p95_idx = n
                printf "%.2f %.2f", mean, samples[p95_idx]
            }
        ' "$gpu_log")
        echo "$bench_line $gpu_stats"
    else
        echo "$bench_line"
    fi
    rm -f "$out" "$gpu_log" 2>/dev/null
}

# Per-bench primary metric, expressed as awk that consumes the result
# fields ($1=name $2=secs $3=iters $4=cols $5=rows $6=bytes).
primary_metric() {
    local bench="$1"
    case "$bench" in
        truecolor_grid|gpu_load)
            # gpu_load reports tick count in the iters column; same
            # per-cell-update formula as truecolor_grid.
            echo 'printf "%.2fM cells/s", $3*$4*$5/$2/1e6' ;;
        cursor_jumps)
            echo 'printf "%.0fk jumps/s", $3/$2/1000' ;;
        scroll_storm)
            echo 'printf "%.0fk lines/s", $3/$2/1000' ;;
        glyph_churn)
            echo 'printf "%.1f MB/s", $6/$2/1048576' ;;
        raw_throughput|*)
            echo 'printf "%.1f MB/s", $6/$2/1048576' ;;
    esac
}

# Average a column across N runs of the same (bench, term).
# Stdin: N lines, each "name secs iters cols rows bytes [gpu_mean gpu_p95]".
# Stdout: same shape with secs (and GPU columns if present) averaged.
# Other fields come from the first line.
average_runs() {
    awk '
        NR==1 { name=$1; iters=$3; cols=$4; rows=$5; bytes=$6; have_gpu = (NF >= 8) }
        { sec_sum += $2 }
        have_gpu { gpu_mean_sum += $7; gpu_p95_sum += $8 }
        END {
            if (have_gpu) {
                printf "%s %.6f %s %s %s %s %.2f %.2f\n",
                    name, sec_sum/NR, iters, cols, rows, bytes,
                    gpu_mean_sum/NR, gpu_p95_sum/NR
            } else {
                printf "%s %.6f %s %s %s %s\n", name, sec_sum/NR, iters, cols, rows, bytes
            }
        }
    '
}

for bench in "${benches[@]}"; do
    echo
    echo "=== $bench ==="
    # macOS ships bash 3.2 — no associative arrays. Stash each metric
    # in its own variable so we can compute soltty's ratio against
    # the others at the end.
    soltty_metric=""
    alacritty_metric=""
    ghostty_metric=""
    for term in "${terms[@]}"; do
        runs_data=""
        for ((r=1; r<=runs; r++)); do
            line=$(run_one "$bench" "$term") || continue
            runs_data+="$line"$'\n'
            # macOS throttles back-to-back GPU window creation — short
            # sleeps produce 2-3x slower numbers in subsequent runs.
            # 1.5s is empirically enough for the compositor to settle.
            sleep 1.5
        done
        if [ -z "$runs_data" ]; then
            echo "  $term: no successful runs" >&2
            continue
        fi
        avg_line=$(echo -n "$runs_data" | average_runs)
        # Trailing gpu_mean/gpu_p95 fields (when --gpu-sample is on)
        # are intentionally ignored by this read — they're consumed
        # separately below.
        read -r _name secs iters cols rows bytes _rest <<< "$avg_line"
        mbps=$(awk -v b="$bytes" -v s="$secs" 'BEGIN { printf "%.1f", b/s/1048576 }')
        primary=$(echo "$avg_line" | awk "{ $(primary_metric "$bench") }")
        if [ "$gpu_sample" = "1" ]; then
            gpu_mean=$(echo "$avg_line" | awk '{ printf "%.1f", $7 }')
            gpu_p95=$(echo "$avg_line"  | awk '{ printf "%.1f", $8 }')
            printf "  %-10s %s   (%ss, %s MB/s, %sx%s grid, %s iters, gpu mean %s%% p95 %s%%)\n" \
                "$term:" "$primary" "$secs" "$mbps" "$cols" "$rows" "$iters" "$gpu_mean" "$gpu_p95"
        else
            printf "  %-10s %s   (%ss, %s MB/s, %sx%s grid, %s iters)\n" \
                "$term:" "$primary" "$secs" "$mbps" "$cols" "$rows" "$iters"
        fi
        case "$term" in
            soltty)    soltty_metric="$primary" ;;
            alacritty) alacritty_metric="$primary" ;;
            ghostty)   ghostty_metric="$primary" ;;
        esac
    done

    if [ -n "$soltty_metric" ]; then
        s_val=$(echo "$soltty_metric" | awk '{ print $1+0 }')
        if [ -n "$alacritty_metric" ]; then
            a_val=$(echo "$alacritty_metric" | awk '{ print $1+0 }')
            ratio=$(awk -v s="$s_val" -v a="$a_val" 'BEGIN { if (a > 0) printf "%.2f", s/a; else print "-" }')
            echo "  -> soltty/alacritty: ${ratio}x"
        fi
        if [ -n "$ghostty_metric" ]; then
            g_val=$(echo "$ghostty_metric" | awk '{ print $1+0 }')
            ratio=$(awk -v s="$s_val" -v g="$g_val" 'BEGIN { if (g > 0) printf "%.2f", s/g; else print "-" }')
            echo "  -> soltty/ghostty:   ${ratio}x"
        fi
    fi
done
