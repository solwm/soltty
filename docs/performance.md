# Performance

This is the running record of what's been done to make soltty fast,
how to verify it, and what's still on the table. It assumes you've
read `docs/how-it-works.md` already.

## Where we stand vs alacritty

Measurement boils down to two questions: "is the user-facing workload
fixed?" and "what do the synthetic axes show?".

### gol-c (the original complaint, the gold-standard test)

A truecolor-cell-paint loop in C, ~22 bytes per cell, no fps cap. This
is what the user noticed soltty being slow on. With the fixes on this
branch, soltty is in the same range as alacritty and faster than
ghostty (5 runs each, 3s settle between runs):

```
soltty:    5.36 - 5.42 M cells/s   (mean ~5.40)
alacritty: 5.47 - 5.65 M cells/s   (mean ~5.56)   +3% over soltty
ghostty:   4.49 - 5.07 M cells/s   (mean ~4.82)   -11% under soltty
```

Run with the harness in `~/workspace/gol-c`:

```sh
GOL_FORCE_COLS=200 GOL_FORCE_ROWS=60 \
GOL_BENCH_ITERS=500 GOL_BENCH_OUT=/tmp/gol.txt \
./target/release/soltty -e bash -lc \
    "GOL_FORCE_COLS=200 GOL_FORCE_ROWS=60 \
     GOL_BENCH_ITERS=500 GOL_BENCH_OUT=/tmp/gol.txt \
     /Users/magniff/workspace/gol-c/gol"
cat /tmp/gol.txt   # secs iters cols rows
```

`gol.c` writes the result to `$GOL_BENCH_OUT` and exits cleanly with
the cursor restored. **Important:** sleep 2-3 seconds between repeated
runs or numbers degrade — see "harness gotchas" below.

### Synthetic suite (`bench/compare.sh`)

Indicative numbers from one 5-run-averaged session. **Trust gol-c
for absolute claims** — the synthetic suite is too noisy
(thermal/compositor variation) for cross-session comparisons.

| axis             | soltty | alacritty | ghostty | what it stresses              |
|------------------|--------|-----------|---------|-------------------------------|
| `truecolor_grid` | 9.0 M c/s | 8.3 M c/s | 4.6 M c/s | per-cell SGR repaint        |
| `glyph_churn`    |  74 MB/s |  89 MB/s | 132 MB/s | atlas allocation + upload   |
| `scroll_storm`   | 3.8 M l/s | 6.3 M l/s | 8.4 M l/s | scrollback ring eviction   |
| `raw_throughput` |  60 MB/s | 103 MB/s | 105 MB/s | parser printable fast path |
| `cursor_jumps`   | 8.2 M j/s | 18 M j/s | 14 M j/s | pure CSI parser cost       |

Reading the table:

- **`truecolor_grid` is the only synthetic axis we win clearly.**
  Mirrors gol-c's workload (truecolor SGR + space per cell), so it
  validates the throttle + SGR fast path doing real work for the
  one user-perceived complaint.
- **Ghostty's strengths are scrollback churn and atlas growth**, by
  big margins. Different architecture — they have an SIMD parser
  and a more aggressive atlas strategy.
- **Alacritty wins `cursor_jumps`** (pure CSI parser cost). Both
  soltty and alacritty use the `vte` crate, but alacritty's wrapper
  around it is leaner. Real workloads don't look like this.
- **`raw_throughput` (plain ASCII)** is the gap that surprises
  people: alacritty and ghostty are both ~70% faster than us at
  pumping plain bytes through the parser. Closing this would mean
  rewriting the parser-to-grid glue. Not currently a priority — see
  "what's still on the table" below.

## What this branch changed

In the order they landed (`git log performance ^main` for the
authoritative list).

### 1. Throttle redraws to display refresh rate (`195bb08`, `a812ced`)

The original 19% gol-c gap was almost entirely here. Profiling showed
that under a chatty PTY producer, winit fires `RedrawRequested` 2-3×
per "game frame" (each ~150 KB burst arrives as ~28 PTY chunks; winit
coalesces request_redraw within a single event-loop pass but not
across them). Each redraw calls `surface.swap_buffers`, which on
macOS Metal/GL is **~196 µs per call even with vsync off**. That cost
dominated the loop.

Fix: cap the actual paint rate with a `dirty` flag, `last_render`
timestamp, and `about_to_wait` arming `ControlFlow::WaitUntil` so
deferred frames still paint when the slot opens.

The whole machinery is in `src/main.rs` around `App::about_to_wait`,
`maybe_request_redraw`, and `detect_frame_interval`.

The cap is per-window, set in `resumed()`:

  - winit's `current_monitor().refresh_rate_millihertz()` first
  - `SOLTTY_FRAME_HZ` env var override (for debugging or compositors
    that don't expose a rate)
  - 120 Hz default if both fail
  - clamped to [60, 240] so a misconfigured monitor can't pin us
    at 5 Hz or 1 kHz

There's also an `IDLE_THRESHOLD = 100 ms` bypass: the first paint
after a long quiet period skips the cap regardless. Mostly defensive
— at typical caps it's logically redundant with `elapsed >=
frame_interval` — but it documents the intent and bounds echo
latency if the cap is ever configured high.

Side effect: human keystroke latency is bounded by `frame_interval`
in the worst case. At 4-8 ms (120-240 Hz panels) this is invisible;
at 16 ms (60 Hz fallback) it's right at the edge of human
perception, which is why `IDLE_THRESHOLD` exists.

### 2. SGR dispatch fast path (`195bb08`)

`csi_dispatch` was building a `Vec<u16>` of params for every CSI
including `m` (SGR), which `apply_sgr` doesn't even use — it walks
`params` directly. In the GoL workload one truecolor cell = one SGR,
so we were doing millions of throwaway heap allocations.

Fix: dispatch `'m'` before the collect:

```rust
if !private && action == 'm' {
    apply_sgr(self.grid(), params);
    return;
}
```

### 3. Recycle the popped scrollback row (`534ba8c`)

Steady-state scroll-up evicted the oldest scrollback row AND
allocated a fresh blank for the bottom of the live grid. Both were
the same size. Pop, clear, reuse: one alloc + one free saved per
scroll. ~15% on `scroll_storm`. Code in `Grid::scroll_up_in_region`
in `src/grid.rs`.

Also short-circuited the alt-screen case (`scrollback_limit == 0`):
it used to push a row into the VecDeque only to immediately pop it
back out.

### 4. Lazy CSI param walk (`9ef9629`)

`arg(&nums, idx, default)` was reading from a `Vec<u16>` collected
upfront. Replaced with `arg(params, idx, default)` that calls
`params.iter().nth(idx)` directly — `nth(0)` and `nth(1)` on a tiny
fixed-size iterator are essentially free. Removes the per-CSI
allocation entirely.

This didn't measurably move `cursor_jumps` because that bench is
parser-bound, not dispatch-bound. But it removes real allocation
waste from any mixed workload.

### 5. Atlas partial upload (`4fd162a`)

Each new glyph used to trigger a full 1024×1024 (1 MB) atlas
re-upload via `tex_sub_image_2d`. Now `FontAtlas` tracks per-glyph
`DirtyRect`s; the renderer uploads only those rects with
`UNPACK_ROW_LENGTH` set so it can read sub-regions of `atlas_data`.

Worst-case improvement: 7000 unique chars × 1 MB → 7000 × few KB
each. `glyph_churn` flipped from 0.83× of alacritty to ~1.19× in
clean runs.

`Renderer::new` and `reload_font` (font zoom) still do one full
upload — a fresh `etagere::AtlasAllocator` places glyphs at brand
new positions, so the texture really does need full replacement.
They both `dirty_rects.clear()` afterwards so the next `prepare()`
doesn't redundantly re-upload.

### Adjacent fixes that landed on this branch

These weren't pure perf but came out of the same investigation:

- `ae0a17b` — Respect `?25l` on the primary screen. Programs like
  gol-c hide the cursor legitimately; the previous "always-visible"
  workaround was a UX papercut.
- `14d5002` — DSR replies (`CSI 5 n`, `CSI 6 n`). Required for the
  bench harness to ping-and-wait correctly. See harness section.
- The `--trace` HUD (`195bb08`) is a small EWMA-smoothed FPS
  counter in the top-right corner. Useful for eyeballing render
  rate under load. Code in `src/renderer.rs::FpsCounter`.

## The bench harness

### Layout

`bench/` is a workspace member crate with one library and five
binaries:

```
bench/
├── Cargo.toml
├── README.md
├── compare.sh          # runner
└── src/
    ├── lib.rs          # Bench helper + DSR ping
    └── bin/
        ├── raw_throughput.rs
        ├── truecolor_grid.rs
        ├── scroll_storm.rs
        ├── cursor_jumps.rs
        └── glyph_churn.rs
```

Each bench reads its config from env vars:

| var               | meaning                                          |
|-------------------|--------------------------------------------------|
| `BENCH_ITERS`     | iteration count override                         |
| `BENCH_OUT`       | result-file path (default `/tmp/soltty_bench.txt`) |
| `BENCH_COLS/ROWS` | force a specific grid size                       |

Result format: `<name> <secs> <iters> <cols> <rows> <bytes>` — one
whitespace-separated line.

### The DSR-ping trick

This is the single most important piece of harness machinery, and
the most overlooked. **Without it, small benches measure how fast
we can `write(2)` into the kernel pipe, not how fast the terminal
actually consumes.**

After the workload, `Bench::finish` (`bench/src/lib.rs`) writes
`\x1b[6n` and reads stdin until it sees the `R` terminator of the
cursor-position response. The terminal only sends that response
*after* it has processed every preceding byte, so the round-trip
pins our clock to actual terminal completion.

stdin has to be in raw mode for this — otherwise the line discipline
cooks the response away. The lib does that via `tcsetattr`, runs the
ping, and restores. `libc` is the only non-rust dep needed.

soltty supports DSR as of `14d5002`. Before that commit the harness
hung waiting for a reply.

### Running

```sh
./bench/compare.sh                          # every bench, default iters
./bench/compare.sh --runs 5                 # average of 5 runs each
./bench/compare.sh --bench truecolor_grid   # one bench
./bench/compare.sh --terms soltty           # one terminal
./bench/compare.sh --cols 200 --rows 60     # force grid size
```

`--cols` / `--rows` matters because the two terminals open at
different default sizes — soltty defaults to ~147×48, alacritty to
~123×46. Without it you'd be comparing different workloads.

### Harness gotchas

1. **macOS throttles back-to-back GPU window creations.** Sleeping
   only 0.2s between runs gives 2-3× slower numbers in subsequent
   runs. The harness now sleeps 1.5s; even that isn't always enough.
   For low-noise comparisons, sleep 3+ seconds between runs by hand.

2. **Anything below ~30% improvement is below the noise floor.**
   We've watched the same bench bounce between 50 MB/s and 150 MB/s
   between sessions. Multi-run averaging helps but doesn't kill it.

3. **Spawning soltty/alacritty has a fixed cost.** For very small
   workloads the spawn dominates. Bump `BENCH_ITERS` until the bench
   itself takes >0.5s.

4. **Trust gol-c for absolute numbers.** It runs in a single
   long-lived terminal window, no per-run spawn. Variance is ~2%.

## What's still on the table

Things we considered but didn't ship, with current best guess at
impact.

### sRGB → linear LUT (predicted small win, never measured under throttle)

`Renderer::resolve_color` calls `srgb_to_linear` (powf-based) per
truecolor cell per frame. We expected this to be a top-3 cost; the
profile said it wasn't, mostly because the redraw throttle now caps
us at 60 frames per second. At ~10k cells × 3 channels × 60 fps =
1.8 M powf/s ≈ 30 ms/s of CPU. Real but not dominant.

Replacement: `srgb_to_linear: [f32; 256]` table, one lookup per
channel. ~10 lines. Probably 1-3% wall-time win on truecolor_grid.
Not currently a priority.

### Skip-unchanged-rows in `prepare`

Currently `Renderer::prepare` iterates every cell every frame, doing
an `atlas.ensure(ch)` and `atlas.get(ch)` HashMap lookup per cell.
Most frames have small changes. A "row dirty" bit on `Grid::lines`
that the parser marks when `put_char` / `clear_with` mutates a row,
and `prepare` clears, would let us skip the whole inner loop for
unchanged rows.

This would matter most for big grids with sparse updates. The bench
suite doesn't really exercise that pattern; vim/editing would.

### Cursor blink (and other rate-limited UI animation)

The cursor blinks at 1 Hz when the shape is Bar or Underline (insert /
replace mode). `App::about_to_wait` arms a `ControlFlow::WaitUntil` for
the next phase boundary, the event loop wakes, dirty gets set, and the
existing throttle path repaints. Two paints per second when nothing else
is happening — well below any noise floor.

Same idea would apply to any future rate-limited animation: don't run
your own timer thread, just compute the next deadline and feed it to
the existing dirty/throttle pipeline.

### vte parser

`cursor_jumps` says alacritty's parser path is leaner. Both use the
`vte` crate. The gap is in dispatch overhead — function calls,
match arms, `Performer` indirection. To close it would mean
rewriting the parser glue, not the parser itself. Not worth it
unless someone really needs CUP-heavy throughput.

### Atlas eviction

The atlas allocator never frees space. A long session with
thousands of unique glyphs (CJK + emoji + math + ...) eventually
fills the 1024×1024 atlas, and after that new glyphs draw as
`.notdef`. An LRU eviction policy on `glyphs` + a free-list in
the allocator would solve it. ~50 lines, low priority unless
someone hits it.

### Real frame-time histogram

The `--trace` HUD shows EWMA fps, which smooths over jitter. A
proper p50/p95/p99 frame-time histogram would surface stalls that
EWMA hides. ~20 lines if anyone cares.

## Where to look in the code

| concern                | file:lines (approx)                |
|------------------------|-----------------------------------|
| Throttle               | `src/main.rs::about_to_wait`, `maybe_request_redraw` |
| Frame-rate detection   | `src/main.rs::detect_frame_interval` (env, monitor, default, clamp) |
| Redraw flag flow       | `App::dirty`, `App::redraw_pending`, `App::last_render`, `App::frame_interval` |
| SGR fast path          | `src/term.rs::csi_dispatch` (top of fn) |
| Lazy CSI args          | `src/term.rs::arg`                |
| DSR reply              | `src/term.rs::Performer::dsr`     |
| Scroll row recycle     | `src/grid.rs::scroll_up_in_region` |
| Atlas dirty rects      | `src/font.rs::FontAtlas::rasterize`, `dirty_rects: Vec<DirtyRect>` |
| Atlas upload           | `src/renderer.rs::prepare` (the `atlas.dirty_rects` block) |
| FPS HUD                | `src/renderer.rs::FpsCounter`, `append_fps_overlay` |
| DSR-ping in benches    | `bench/src/lib.rs::ping_terminal` |

## Working on perf yourself

The shortest viable loop:

1. `cargo build --release` (everything; bench crate too)
2. `./bench/compare.sh --bench <name> --runs 5` for the axis you
   touched
3. `./target/release/soltty --trace` and watch the HUD on a
   real-world workload (try `find / 2>/dev/null` or a `cat` of a
   big colorful log)
4. For absolute claims, run gol-c with 3+ second sleeps between
   runs and report the median of 5

Don't trust a single `compare.sh` invocation to tell you anything
absolute. Trust it to tell you a *change* in your code (run it
before, change one thing, run it after).

When in doubt, profile. We don't have `cargo flamegraph` in the
toolchain by default — instrument by hand with `Instant::now()` if
you need to find a hot spot. Past experience: the answers are
almost always different from your prior.
