# soltty-bench

Focused benchmark programs for comparing terminal emulators on
specific axes. Each program is a single Rust binary that writes a
known workload to stdout, asks the terminal to round-trip a DSR
cursor-position query, and records its wall time.

The DSR round-trip matters: without it, small workloads finish before
the kernel pipe even backpressures, and we'd just measure how fast we
can `write(2)` rather than how fast the terminal actually consumes.

## Programs

| Bench            | What it stresses                                     |
|------------------|------------------------------------------------------|
| `raw_throughput` | Plain ASCII, no escapes — parser printable fast path |
| `truecolor_grid` | Full-screen repaint with random `\e[48;2;R;G;Bm `    |
| `scroll_storm`   | Many short lines — `\n` past bottom triggers scroll  |
| `cursor_jumps`   | Random `\e[<r>;<c>H` only — pure CSI parser cost     |
| `glyph_churn`    | ~7000 distinct BMP code points — atlas allocator     |

## Running

The most common case — compare every terminal on every bench:

```
./bench/compare.sh
```

Useful flags:

```
--iters N           Override BENCH_ITERS for every bench
--runs N            Average N runs per (bench, terminal)
--bench NAME        Run just one of the benches
--terms a,b         Comma-separated list. Default is
                    soltty,alacritty,ghostty — drop names to skip.
--cols N --rows N   Force a fixed grid size (different terminals open
                    at different default sizes, so this is needed for
                    apples-to-apples comparison)
```

### Terminal launch quirks

- `soltty -e <cmd>` and `alacritty -e <cmd>` block until `<cmd>`
  exits, so the harness reads the result file right after they
  return.
- `ghostty -e ...` on macOS just signals an existing Ghostty.app
  process and returns immediately, so the harness uses
  `open -nWa Ghostty.app --args -e ...` instead. `-n` forces a
  fresh app instance and `-W` blocks until it terminates. The
  cold-spawn cost is ~3s of overhead per run that doesn't affect
  the bench measurement (the bench times itself), but does make
  multi-run comparisons slow.

Each bench writes a single result line to a temp file:

```
<name> <secs> <iters> <cols> <rows> <bytes>
```

## Running one bench by hand

```
BENCH_ITERS=1000 \
BENCH_OUT=/tmp/r.txt \
BENCH_COLS=200 BENCH_ROWS=60 \
./target/release/soltty -e bash -lc \
    "BENCH_ITERS=1000 BENCH_OUT=/tmp/r.txt ./target/release/raw_throughput"
cat /tmp/r.txt
```

The env vars need to be in the `bash -lc` invocation because that's
where the bench actually runs; the outer `BENCH_*` vars don't propagate
through soltty's `-e` flag.

## Sample numbers (M5 MacBook, soltty performance branch)

200×60 grid, --runs 5, one session. Numbers are noisy across
sessions; treat as indicative ranges, not gospel.

```
                soltty       alacritty   ghostty
raw_throughput   60 MB/s     103 MB/s   105 MB/s
truecolor_grid    9.0 M c/s    8.3 M c/s   4.6 M c/s   <- soltty wins
scroll_storm      3.8 M l/s    6.3 M l/s   8.4 M l/s
cursor_jumps      8.2 M j/s   18.0 M j/s  13.9 M j/s
glyph_churn       74 MB/s      89 MB/s   132 MB/s
```

For the headline gol-c number (real-world truecolor stress test in
a single long-lived window, low variance):

```
soltty:    ~5.40 M cells/s
alacritty: ~5.56 M cells/s   (+3% over soltty)
ghostty:   ~4.82 M cells/s   (-11% under soltty)
```
