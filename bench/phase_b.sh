#!/usr/bin/env bash
# Phase B: GPU-load baseline measurement matrix for soltty.
#
# Runs the gpu_load bench under several configurations, samples nvidia-smi
# utilization.gpu, and prints a table. Also runs a cmatrix witness in a
# maximized window so the user's actual reported workload is in the table
# too.
#
# Output goes both to stdout (human-friendly) and to a machine-readable
# file we can diff against after Phase C lands. The file format is a
# single TSV per run:
#
#   label	hz	cols	rows	secs	gpu_mean	gpu_p95
#
# Usage:
#   ./bench/phase_b.sh                  # full matrix
#   ./bench/phase_b.sh --skip-cmatrix   # gpu_load only (no GUI needed)
#   ./bench/phase_b.sh --out baseline.tsv

set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
bin_dir="$repo/target/release"

skip_cmatrix=0
out_file="$repo/bench/phase_b_baseline.tsv"
while [ $# -gt 0 ]; do
    case "$1" in
        --skip-cmatrix) skip_cmatrix=1; shift ;;
        --out) out_file="$2"; shift 2 ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if ! command -v nvidia-smi >/dev/null; then
    echo "nvidia-smi not on PATH — this script is NVIDIA-only" >&2
    exit 1
fi

# Build if needed.
( cd "$repo" && cargo build --release --quiet )

# Sample GPU util.gpu for N seconds, discard the first second of warmup,
# print mean and p95.
sample_gpu() {
    local seconds="$1"
    nvidia-smi \
        --query-gpu=utilization.gpu \
        --format=csv,noheader,nounits \
        -lms 100 -c $((seconds * 10)) 2>/dev/null \
    | awk '
        NR > 10 { v = $1+0; raw[++n] = v; sum += v }
        END {
            if (n <= 0) { print "0.0 0.0"; exit }
            for (i = 1; i <= n; i++) {
                for (j = i+1; j <= n; j++) {
                    if (raw[j] < raw[i]) { t = raw[i]; raw[i] = raw[j]; raw[j] = t }
                }
            }
            mean = sum / n
            p95_idx = int(n * 0.95 + 0.5)
            if (p95_idx < 1) p95_idx = 1
            if (p95_idx > n) p95_idx = n
            printf "%.2f %.2f", mean, raw[p95_idx]
        }
    '
}

# Run gpu_load under soltty with the given Hz cap and window-maximize
# flag. Returns "secs gpu_mean gpu_p95" on one line.
run_gpu_load() {
    local label="$1" hz="$2" maximized="$3" duration_ms="${4:-6000}"

    local out_path; out_path=$(mktemp -t phase_b.XXXXXX); rm -f "$out_path"
    local gpu_log; gpu_log=$(mktemp -t phase_b_gpu.XXXXXX)

    # Discard 1 s of warmup: bench runs duration_ms+1500 so sampler has
    # head time. Sampler runs the full duration; awk drops first 10
    # samples.
    nvidia-smi \
        --query-gpu=utilization.gpu \
        --format=csv,noheader,nounits \
        -lms 100 \
        > "$gpu_log" 2>/dev/null &
    local sampler_pid=$!

    local env_prefix="BENCH_OUT=$out_path BENCH_DURATION_MS=$duration_ms"
    [ -n "$hz" ] && env_prefix="SOLTTY_FRAME_HZ=$hz $env_prefix"
    [ "$maximized" = "1" ] && env_prefix="SOLTTY_START_MAXIMIZED=1 $env_prefix"

    env $env_prefix "$bin_dir/soltty" -e bash -lc "$bin_dir/gpu_load" \
        >/dev/null 2>&1

    kill -TERM "$sampler_pid" 2>/dev/null || true
    wait "$sampler_pid" 2>/dev/null || true

    local secs cols rows
    if [ -s "$out_path" ]; then
        read -r _name secs _ticks cols rows _bytes < "$out_path"
    else
        secs="0"; cols="0"; rows="0"
    fi

    local gpu_stats
    gpu_stats=$(awk '
        { v = $1+0; raw[++n] = v }
        END {
            start = 11
            window_n = n - 10
            if (window_n <= 0) { printf "0.00 0.00"; exit }
            for (i = 1; i <= window_n; i++) samples[i] = raw[start + i - 1]
            for (i = 1; i <= window_n; i++) {
                for (j = i+1; j <= window_n; j++) {
                    if (samples[j] < samples[i]) {
                        t = samples[i]; samples[i] = samples[j]; samples[j] = t
                    }
                }
                sum += samples[i]
            }
            mean = sum / window_n
            p95_idx = int(window_n * 0.95 + 0.5)
            if (p95_idx < 1) p95_idx = 1
            if (p95_idx > window_n) p95_idx = window_n
            printf "%.2f %.2f", mean, samples[p95_idx]
        }
    ' "$gpu_log")

    rm -f "$out_path" "$gpu_log" 2>/dev/null
    printf "%s\t%s\t%s\t%s\t%s\t%s\n" \
        "$label" "${hz:-monitor}" "$cols" "$rows" "$secs" "$gpu_stats"
}

# Cmatrix witness: launch soltty maximized running cmatrix under a hard
# `timeout`, sample nvidia-smi in the background for `duration` seconds,
# kill the sampler, let timeout reap soltty+cmatrix.
#
# Two correctness notes:
#   - nvidia-smi has no "count" option in query mode (-c means
#     --compute-mode, a setter that yields a bogus parse error). The
#     loop runs until SIGTERM.
#   - `timeout --foreground --kill-after=2` ensures the entire launched
#     process tree (timeout → env → soltty → bash → cmatrix) dies even
#     if soltty doesn't propagate SIGTERM cleanly through its PTY.
run_cmatrix() {
    local label="$1" hz="$2" duration="${3:-10}"

    local gpu_log; gpu_log=$(mktemp -t phase_b_gpu.XXXXXX)
    local env_prefix="SOLTTY_START_MAXIMIZED=1"
    [ -n "$hz" ] && env_prefix="SOLTTY_FRAME_HZ=$hz $env_prefix"

    # Settle (1s) + sample window + 2s tail.
    local hard_kill=$((duration + 3))

    timeout --foreground --kill-after=2 "${hard_kill}s" \
        env $env_prefix "$bin_dir/soltty" -e cmatrix -u 5 >/dev/null 2>&1 &
    local job_pid=$!

    # Brief settle so the window is up and cmatrix has started animating.
    sleep 1.0

    nvidia-smi \
        --query-gpu=utilization.gpu \
        --format=csv,noheader,nounits \
        -lms 100 \
        > "$gpu_log" 2>/dev/null &
    local sampler_pid=$!

    # Sample for `duration` seconds, then end the sampler.
    sleep "$duration"
    kill -TERM "$sampler_pid" 2>/dev/null || true
    wait "$sampler_pid" 2>/dev/null || true

    # `timeout` will SIGKILL soltty+children if they're still alive.
    wait "$job_pid" 2>/dev/null || true

    local gpu_stats
    gpu_stats=$(awk '
        { v = $1+0; raw[++n] = v }
        END {
            start = 11
            window_n = n - 10
            if (window_n <= 0) { printf "0.00 0.00"; exit }
            for (i = 1; i <= window_n; i++) samples[i] = raw[start + i - 1]
            for (i = 1; i <= window_n; i++) {
                for (j = i+1; j <= window_n; j++) {
                    if (samples[j] < samples[i]) {
                        t = samples[i]; samples[i] = samples[j]; samples[j] = t
                    }
                }
                sum += samples[i]
            }
            mean = sum / window_n
            p95_idx = int(window_n * 0.95 + 0.5)
            if (p95_idx < 1) p95_idx = 1
            if (p95_idx > window_n) p95_idx = window_n
            printf "%.2f %.2f", mean, samples[p95_idx]
        }
    ' "$gpu_log")
    rm -f "$gpu_log" 2>/dev/null
    # We don't have grid dims here; left blank. duration in seconds.
    printf "%s\t%s\t-\t-\t%s\t%s\n" "$label" "${hz:-monitor}" "$duration" "$gpu_stats"
}

# Output header.
{
    echo "# Phase B baseline. timestamp=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf "label\thz\tcols\trows\tsecs\tgpu_mean\tgpu_p95\n"
} | tee "$out_file"

# 1) gpu_load default window, monitor refresh (typically 120 Hz here).
{ printf "  running: gpu_load default 6s\n" >&2; run_gpu_load "gpu_load_default" "" 0; } | tee -a "$out_file"
sleep 1.5

# 2) gpu_load default window, 60 Hz cap.
{ printf "  running: gpu_load 60Hz 6s\n" >&2; run_gpu_load "gpu_load_60hz"   "60" 0; } | tee -a "$out_file"
sleep 1.5

# 3) gpu_load default window, 240 Hz cap.
{ printf "  running: gpu_load 240Hz 6s\n" >&2; run_gpu_load "gpu_load_240hz" "240" 0; } | tee -a "$out_file"
sleep 1.5

# 4) gpu_load maximized, monitor refresh — same window the user runs cmatrix in.
{ printf "  running: gpu_load maximized 6s\n" >&2; run_gpu_load "gpu_load_max"     "" 1; } | tee -a "$out_file"
sleep 1.5

# 5) gpu_load maximized, 60 Hz cap.
{ printf "  running: gpu_load maximized 60Hz 6s\n" >&2; run_gpu_load "gpu_load_max_60hz" "60" 1; } | tee -a "$out_file"
sleep 1.5

# 6) cmatrix witness, maximized, monitor refresh. The user's actual
#    reported workload — this is the number that has to drop.
if [ "$skip_cmatrix" = "0" ]; then
    { printf "  running: cmatrix maximized 10s\n" >&2; run_cmatrix "cmatrix_max"      "" 10; } | tee -a "$out_file"
    sleep 1.5
    { printf "  running: cmatrix maximized 60Hz 10s\n" >&2; run_cmatrix "cmatrix_max_60hz" "60" 10; } | tee -a "$out_file"
fi

echo
echo "baseline written: $out_file"
