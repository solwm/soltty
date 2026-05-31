_default:
    @just --list

# Release build with the project's native-CPU flags.
build:
    cargo build --release

# Run the full test suite.
test:
    cargo test --release

# Cargo check without producing a binary — fastest "did this compile" loop.
check:
    cargo check --release

# Run soltty interactively at the standard font size.
run *ARGS:
    cargo run --release -- {{ARGS}}

# 7-run gol-c bench (640×150, 400 iters) — appends one row to bench/profile_track.tsv
bench label="":
    bench/profile_track.sh --runs 7 {{ if label != "" { "--label '" + label + "'" } else { "" } }}

# Same bench with a custom run count: `just bench-runs 12 "experiment-X"`
bench-runs runs label="":
    bench/profile_track.sh --runs {{runs}} {{ if label != "" { "--label '" + label + "'" } else { "" } }}

# Show the last 10 bench rows column-aligned.
bench-tail:
    #!/usr/bin/env bash
    column -t -s $'\t' bench/profile_track.tsv | tail -11

# perf-record one gol-c run and print the top soltty symbols (>0.5%).
perf:
    #!/usr/bin/env bash
    set -euo pipefail
    pd=$(mktemp -t soltty-perf.XXXXXX); rm -f "$pd"
    out=$(mktemp -t soltty-bench.XXXXXX); rm -f "$out"
    timeout --foreground --kill-after=2 20s \
        perf record -F 2000 --call-graph dwarf -o "$pd" \
        -- env SOLTTY_FONT_PX=10 SOLTTY_START_MAXIMIZED=1 ./target/release/soltty \
        -e bash -c "GOL_BENCH_ITERS=400 GOL_FORCE_COLS=640 GOL_FORCE_ROWS=150 \
                    GOL_BENCH_OUT=$out /home/magniff/workspace/gol-c/gol; sleep 0.3" \
        > /dev/null 2>&1
    echo "wallclock: $(cat "$out" 2>&1)"
    echo "=== top soltty symbols ==="
    perf report -i "$pd" --stdio --no-children -g none --percent-limit 0.5 2>/dev/null \
        | grep -E "^ +[0-9]" | head -25
    rm -f "$pd" "$out"

# Clean target/.
clean:
    cargo clean
