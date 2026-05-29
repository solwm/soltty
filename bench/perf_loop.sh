#!/usr/bin/env bash
# Unified perf-loop runner.
#
# One command, multiple scenarios, structured TSV output, A/B-able
# against alacritty. The point is: every optimization claim is grounded
# in numbers from this tool. Don't trust eyeballs; trust this file.
#
# Each scenario produces a row with these columns (tab-separated):
#
#   scenario    terminal     hz_cap   width  height \
#   sol_sm     soltty_sm   util_mean util_p95 util_max util_min \
#   commits    drain_rate  prepare_mean swap_mean
#
# Usage:
#   bench/perf_loop.sh                            # default scenarios + soltty
#   bench/perf_loop.sh --runs 5                   # average of N runs
#   bench/perf_loop.sh --terms soltty,alacritty   # A/B
#   bench/perf_loop.sh --scenarios cmatrix,idle   # subset
#   bench/perf_loop.sh --out perf_2026_05_29.tsv  # custom output file
#
# Files written: --out (default ./bench/perf_loop_<git-sha>.tsv)
# Console: human-readable summary as scenarios complete.

set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
bin_dir="$repo/target/release"

# -------- arg parsing -----------------------------------------------------

runs=3
selected_terms=(soltty alacritty)
selected_scenarios=(cmatrix gpu_load idle scroll_burst)
out_file=""
sample_seconds=8
capture_commits=0

while [ $# -gt 0 ]; do
    case "$1" in
        --runs)       runs="$2"; shift 2 ;;
        --terms)      IFS=',' read -ra selected_terms <<< "$2"; shift 2 ;;
        --scenarios)  IFS=',' read -ra selected_scenarios <<< "$2"; shift 2 ;;
        --out)        out_file="$2"; shift 2 ;;
        --seconds)    sample_seconds="$2"; shift 2 ;;
        --commits)    capture_commits=1; shift ;;
        -h|--help)
            sed -n '2,/^$/p' "$0" | sed 's/^# \?//'
            exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if ! command -v nvidia-smi >/dev/null; then
    echo "nvidia-smi not on PATH — this runner is NVIDIA-only" >&2
    exit 1
fi

# Output path defaults to ./bench/perf_loop_<sha>.tsv so each commit
# has its own baseline file. Override with --out for ad-hoc runs.
sha=$(git -C "$repo" rev-parse --short HEAD 2>/dev/null || echo "no-git")
if [ -z "$out_file" ]; then
    out_file="$repo/bench/perf_loop_${sha}.tsv"
fi

# -------- build -----------------------------------------------------------

echo "Building release artifacts..." >&2
( cd "$repo" && cargo build --release --quiet )

# -------- term-launching helpers ------------------------------------------

# Spawn a maximized 4K terminal running the given command. Returns the
# job pid via stdout. The terminal closes after the command exits, or
# after `timeout` if we kill it.
spawn_term() {
    local term="$1" hard_kill_s="$2" cmd_str="$3"
    # If --commits is set, route stderr to the capture file. Otherwise
    # discard it; WAYLAND_DEBUG output is huge.
    local stderr_dst="/dev/null"
    if [ "$capture_commits" = "1" ] && [ -n "${trial_wl_log:-}" ]; then
        stderr_dst="$trial_wl_log"
    fi
    local wl_env=""
    [ "$capture_commits" = "1" ] && wl_env="WAYLAND_DEBUG=client"
    case "$term" in
        soltty)
            timeout --foreground --kill-after=2 "${hard_kill_s}s" \
                env $wl_env SOLTTY_START_MAXIMIZED=1 \
                "$bin_dir/soltty" -e bash -c "$cmd_str" >/dev/null 2>"$stderr_dst" &
            ;;
        alacritty)
            command -v alacritty >/dev/null || return 1
            timeout --foreground --kill-after=2 "${hard_kill_s}s" \
                env $wl_env alacritty -o 'window.startup_mode="Maximized"' \
                -e bash -c "$cmd_str" >/dev/null 2>"$stderr_dst" &
            ;;
        *) echo "unknown terminal: $term" >&2; return 1 ;;
    esac
    echo $!
}

# Run nvidia-smi pmon + util.gpu samplers in parallel for `secs` seconds.
# Outputs to provided log paths. Caller is responsible for cleanup.
start_samplers() {
    local pmon_log="$1" util_log="$2"
    nvidia-smi pmon -d 1 -c 60 -s u 2>/dev/null > "$pmon_log" &
    local pmon_pid=$!
    nvidia-smi --query-gpu=utilization.gpu --format=csv,noheader,nounits \
        -lms 100 > "$util_log" 2>/dev/null &
    local util_pid=$!
    echo "$pmon_pid $util_pid"
}

stop_samplers() {
    local pids="$1"
    set -- $pids
    local util_pid="$2"
    kill -TERM "$util_pid" 2>/dev/null || true
    wait $pids 2>/dev/null || true
}

# Aggregate pmon log: sol mean, soltty (or alacritty) per-process mean.
# Args: pmon_log_path term_process_name
agg_pmon() {
    awk -v target="$2" '
        /^#/ { next }
        $10 == "sol"    { sol_sum += $4; sol_n++ }
        $10 == target   { tg_sum  += $4; tg_n++; if (tg_max < $4+0) tg_max = $4+0 }
        END {
            printf "%.2f %.2f %.0f",
                (sol_n ? sol_sum/sol_n : 0),
                (tg_n  ? tg_sum/tg_n   : 0),
                (tg_max ? tg_max : 0)
        }
    ' "$1"
}

# Aggregate util.gpu sample log: mean, p95, max, min over the sample
# window. Skips the first 10 samples (~1s warmup).
agg_util() {
    awk '
        NR > 10 {
            v = $1+0; raw[++n] = v; sum += v
            if (max < v) max = v
            if (min == "" || min > v) min = v
        }
        END {
            if (n <= 0) { print "0 0 0 0"; exit }
            for (i = 1; i <= n; i++)
                for (j = i+1; j <= n; j++)
                    if (raw[j] < raw[i]) { t = raw[i]; raw[i] = raw[j]; raw[j] = t }
            p95_idx = int(n * 0.95 + 0.5)
            if (p95_idx < 1) p95_idx = 1
            if (p95_idx > n) p95_idx = n
            printf "%.1f %.1f %d %d", sum/n, raw[p95_idx], max, min
        }
    ' "$1"
}

# Run one scenario × one terminal × one trial, return one row.
# Columns: sol_sm tg_sm tg_sm_max util_mean util_p95 util_max util_min commits_per_s
run_trial() {
    local term="$1" scenario_cmd="$2" run_secs="$3"
    local pmon_log util_log
    pmon_log=$(mktemp -t pmon.XXXXXX)
    util_log=$(mktemp -t util.XXXXXX)
    trial_wl_log=""
    if [ "$capture_commits" = "1" ]; then
        trial_wl_log=$(mktemp -t wl.XXXXXX)
    fi

    local hard=$((run_secs + 5))
    local job_pid; job_pid=$(spawn_term "$term" "$hard" "$scenario_cmd")
    sleep 2  # let window come up and workload start

    local sampler_pids; sampler_pids=$(start_samplers "$pmon_log" "$util_log")
    sleep "$run_secs"
    stop_samplers "$sampler_pids"
    wait "$job_pid" 2>/dev/null || true

    local pmon_part util_part commits_part="0"
    pmon_part=$(agg_pmon "$pmon_log" "$term")
    util_part=$(agg_util "$util_log")
    if [ -n "$trial_wl_log" ] && [ -s "$trial_wl_log" ]; then
        local total; total=$(grep -c "wl_surface.*\.commit" "$trial_wl_log" || true)
        # The window covers settle+sample; commits/sec = total / (run_secs+2).
        commits_part=$(awk -v t="$total" -v s="$run_secs" 'BEGIN{printf "%.1f", t/(s+2)}')
    fi
    rm -f "$pmon_log" "$util_log" "$trial_wl_log"
    echo "$pmon_part $util_part $commits_part"
}

# -------- scenarios -------------------------------------------------------
#
# Each scenario is a (name, command-run-inside-terminal) pair. The
# command needs to keep the terminal alive for at least sample_seconds +
# 3s of settle time. Most should be self-terminating to avoid orphans.

scenario_cmd() {
    case "$1" in
        cmatrix)        echo "cmatrix -u 5" ;;
        gpu_load)       echo "BENCH_DURATION_MS=$((sample_seconds * 1000 + 4000)) \"$bin_dir/gpu_load\"" ;;
        idle)           echo "sleep $((sample_seconds + 4))" ;;
        scroll_burst)   echo "yes 'the quick brown fox jumps over the lazy dog 0123456789' | head -n 500000 | sed -n '1,500000p'; sleep 3" ;;
        typing_echo)    echo "for i in \$(seq 1 200); do echo \"line \$i echo me echo me\"; sleep 0.02; done; sleep 1" ;;
        *) return 1 ;;
    esac
}

# -------- main loop -------------------------------------------------------

{
    echo "# perf_loop run timestamp=$(date -u +%Y-%m-%dT%H:%M:%SZ) sha=${sha:-unknown}"
    echo "# runs=$runs sample_seconds=$sample_seconds capture_commits=$capture_commits"
    printf "scenario\tterminal\tsol_sm\tterm_sm\tterm_sm_max\tutil_mean\tutil_p95\tutil_max\tutil_min\tcommits_per_s\n"
} > "$out_file"

for scenario in "${selected_scenarios[@]}"; do
    cmd=$(scenario_cmd "$scenario") || { echo "unknown scenario: $scenario" >&2; continue; }
    echo
    echo "=== $scenario ==="
    for term in "${selected_terms[@]}"; do
        # Accumulate sums for the average; one line per (scenario, term).
        local_sum_sol=0
        local_sum_tg=0
        local_sum_tg_max=0
        local_sum_um=0
        local_sum_p95=0
        local_sum_max=0
        local_sum_min=0
        local_sum_commits=0
        ok_runs=0
        for ((r=1; r<=runs; r++)); do
            read -r sol tg tg_max um p95 mx mn commits < <(run_trial "$term" "$cmd" "$sample_seconds") || {
                echo "  $term run $r: failed" >&2
                continue
            }
            local_sum_sol=$(awk -v a="$local_sum_sol" -v b="$sol" 'BEGIN{print a+b}')
            local_sum_tg=$(awk  -v a="$local_sum_tg"  -v b="$tg"  'BEGIN{print a+b}')
            local_sum_tg_max=$((local_sum_tg_max + tg_max))
            local_sum_um=$(awk  -v a="$local_sum_um"  -v b="$um"  'BEGIN{print a+b}')
            local_sum_p95=$(awk -v a="$local_sum_p95" -v b="$p95" 'BEGIN{print a+b}')
            local_sum_max=$((local_sum_max + mx))
            local_sum_min=$((local_sum_min + mn))
            local_sum_commits=$(awk -v a="$local_sum_commits" -v b="$commits" 'BEGIN{print a+b}')
            ok_runs=$((ok_runs + 1))
            sleep 1.5
        done
        if [ "$ok_runs" -eq 0 ]; then
            printf "  %-12s SKIP\n" "$term"
            continue
        fi
        avg_sol=$(awk -v s="$local_sum_sol" -v n="$ok_runs" 'BEGIN{printf "%.2f", s/n}')
        avg_tg=$(awk  -v s="$local_sum_tg"  -v n="$ok_runs" 'BEGIN{printf "%.2f", s/n}')
        avg_tg_max=$((local_sum_tg_max / ok_runs))
        avg_um=$(awk  -v s="$local_sum_um"  -v n="$ok_runs" 'BEGIN{printf "%.1f", s/n}')
        avg_p95=$(awk -v s="$local_sum_p95" -v n="$ok_runs" 'BEGIN{printf "%.1f", s/n}')
        avg_max=$((local_sum_max / ok_runs))
        avg_min=$((local_sum_min / ok_runs))
        avg_commits=$(awk -v s="$local_sum_commits" -v n="$ok_runs" 'BEGIN{printf "%.1f", s/n}')
        commits_field=""
        [ "$capture_commits" = "1" ] && commits_field="  commits=$avg_commits/s"
        printf "  %-12s sol=%-5s  %s_sm=%-5s(max %d) util=%4s(min %d, max %d, p95 %s)%s\n" \
            "$term" "$avg_sol" "$term" "$avg_tg" "$avg_tg_max" "$avg_um" "$avg_min" "$avg_max" "$avg_p95" "$commits_field"
        printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" \
            "$scenario" "$term" "$avg_sol" "$avg_tg" "$avg_tg_max" "$avg_um" "$avg_p95" "$avg_max" "$avg_min" "$avg_commits" \
            >> "$out_file"
    done
done

echo
echo "Results written: $out_file"
