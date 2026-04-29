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

The most common case — compare both terminals on every bench:

```
./bench/compare.sh
```

Useful flags:

```
--iters N           Override BENCH_ITERS for every bench
--runs N            Average N runs per (bench, terminal)
--bench NAME        Run just one of the benches
--terms a,b         Comma-separated list (default: soltty,alacritty)
--cols N --rows N   Force a fixed grid size (different terminals open
                    at different default sizes, so this is needed for
                    apples-to-apples comparison)
```

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

200×60 grid, default iters per bench:

```
raw_throughput     soltty 150 MB/s     alacritty 103 MB/s    1.45x
truecolor_grid     soltty 10.4M c/s    alacritty 8.3M c/s    1.26x
scroll_storm       soltty 6.0M l/s     alacritty 7.0M l/s    0.87x
cursor_jumps       soltty 17.4M j/s    alacritty 17.9M j/s   ~tied
glyph_churn        soltty  80 MB/s     alacritty  77 MB/s    ~tied
```
