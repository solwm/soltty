# Performance

This is the running record of what's been done to make soltty fast,
how to verify it, and what's still on the table. It assumes you've
read `docs/how-it-works.md` already.

## Where we stand vs alacritty

The synthetic suite (`bench/compare.sh`) and the user-facing gol-c
benchmark, both 10-run trimmed means on a 240 Hz panel:

| bench               | ratio  | what it stresses                                |
|---------------------|-------:|-------------------------------------------------|
| `truecolor_grid`    | 2.05×  | per-cell SGR repaint                            |
| `raw_throughput`    | 1.65×  | parser printable fast path                      |
| `cursor_jumps`      | 1.18×  | pure CSI parser cost (CUP)                      |
| `scroll_storm`      | 0.93×  | scrollback ring eviction + LF execute           |
| `gol-c` (10 px)     | 0.91×  | the user-facing gol-of-life paint loop          |
| `glyph_churn`       | 0.86×  | atlas allocation + upload                       |

Three convincing wins, three within the parity-noise band. The
remaining sub-1.0× numbers swing into 1.04-1.13× territory in
individual runs — variance on this Wayland+NVIDIA host is wide
enough that single runs lie. Trust A/B comparisons (apples-to-apples
on the same hardware in the same minute) for measuring our own
changes; trust the trimmed-mean numbers for cross-emulator claims.

### Running the benches

```sh
./bench/compare.sh                          # every bench, default iters
./bench/compare.sh --runs 10                # average of 10 runs each
./bench/compare.sh --bench truecolor_grid   # one bench
./bench/compare.sh --terms soltty           # one terminal
./bench/compare.sh --cols 200 --rows 60     # force grid size
```

`--cols` / `--rows` matters because the two terminals open at
different default sizes — without it you're comparing different
workloads.

### gol-c

A truecolor-cell-paint loop in C, ~22 bytes per cell, runs at the
panel's refresh rate. `~/workspace/gol-c/gol` accepts these env
vars when `GOL_BENCH_ITERS > 0`:

| var                | meaning                                          |
|--------------------|--------------------------------------------------|
| `GOL_BENCH_ITERS`  | iteration count (required to enable bench mode)  |
| `GOL_FORCE_COLS`   | grid width override                              |
| `GOL_FORCE_ROWS`   | grid height override                             |
| `GOL_BENCH_OUT`    | result-file path (default `/tmp/gol.txt`)        |

```sh
SOLTTY_FONT_PX=10 ./target/release/soltty -e bash -lc \
    "GOL_BENCH_ITERS=500 GOL_FORCE_COLS=200 GOL_FORCE_ROWS=50 \
     GOL_BENCH_OUT=/tmp/gol.txt /home/you/workspace/gol-c/gol"
cat /tmp/gol.txt   # secs iters cols rows
```

In bench mode `gol.c` seeds `srand` with a fixed value and round-
trips a DSR ping at the end so the clock pins to actual terminal
consumption, not `write(2)` queue depth.

## What's been done

The optimizations land in two buckets: parser/dispatch (avoid vte's
per-byte trait dispatch where we can do better inline) and frame
budget (don't render more often than the eye can see, especially
during heavy bursts).

### Parser fast paths

Each path detects a hot byte/sequence in the parser-Ground state
and dispatches straight to a `Grid` method, skipping
`vte::Parser::advance` + `Performer::*` callbacks entirely. They
all live in `Term::feed` (`src/term.rs`); anything that doesn't
match falls through to the original byte-by-byte vte path
unchanged.

| commit     | path                       | what it skips                                     |
|------------|----------------------------|---------------------------------------------------|
| `372c3bb`  | printable ASCII run        | vte state machine for runs of `0x20..=0x7E`       |
| `7575c95`  | inline SGR (`ESC[…m`)      | per-byte dispatch + `Params` allocation           |
| `e9b692c`  | inline CUP (`ESC[…H`/`f`)  | same dispatch chain for cursor positioning        |
| `e00c888`  | C0 controls (BS/HT/LF/CR)  | `Performer::execute` round trip                   |

After the four fast paths, the remaining vte traffic during gol-c
is initialization (`?25l`/`?25h`) and a per-frame `\x1B[1;1H`. The
profile's `vte::Parser::perform_action` bucket dropped from ~30%
of CPU to ~7% across this work.

The four paths share a single `if performer.in_ground { … }` guard
so the branch predictor sees one predicate, not three. The earlier
attempt with separate `&& in_ground` guards was a wash — the
predictor missed the duplicate.

`apply_sgr_simple` (used by the inline SGR path) mirrors `apply_sgr`'s
arms but reads from a flat `&[u16]` instead of vte's `Params`
iterator. Subparam form (`38:2:R:G:B`) and any unknown final byte
fall back to vte and the existing `apply_sgr` path so we don't
regress correctness for the long tail.

### Render rate

Render is the wallclock bottleneck once parsing is fast. Hz scaling
on gol-c (10 px font, 200×50 grid):

| frame cap | gol-c time |
|-----------|-----------:|
| 60 Hz     | 0.62 s     |
| 120 Hz    | 0.73 s     |
| 240 Hz    | 1.02 s     |

Wallclock scales with frame rate because each `swap_buffers` does
real driver work — ~13% of CPU lives in `libnvidia-eglcore` per
swap. Pumping frames faster than the panel can present them is
pure waste; the compositor drops them.

#### Burst throttle (`0e4a5af`, `de93a5a`)

A single PTY drain `>= 4 KB` is taken as evidence of a sustained
burst (gol-c, `cat` of a large file, build-tool log dump). It arms
a 6-frame holdoff that triples the redraw deadline, decaying one
step per render. Single-keypress echoes sit well below 4 KB and
never trip it. `KeyboardInput` clears the holdoff so typing during
a burst always renders at full refresh.

Effect on 240 Hz: during bursts we render at ~80 Hz instead of
240 Hz, which is invisible for animation-style output but cuts
GPU work by 3×. Closed the gol-c gap from 0.71× to 0.91×.

#### `about_to_wait` early-outs (`debea4b`)

`about_to_wait` fires after every winit event poll. In Ground +
not-blinking + redraw-already-pending state, the body would
compute `Instant::now()` and walk to the same `return`. Bail up
front. Saved ~1.5% on gol-c, more under heavy event traffic.

### Renderer micro-opts

| commit     | change                                                   |
|------------|----------------------------------------------------------|
| `861ab5d`  | sRGB→linear LUT replaces per-call powf in `resolve_color`|
| `b36e099`  | `Vec<Row>` → `VecDeque<Row>` for grid lines              |
| `b36e099`  | ASCII glyph cache (`[Option<GlyphEntry>; 128*4]` array)  |
| `b36e099`  | Renderer skips atlas lookup for blank cells              |
| `b36e099`  | `char_width` short-circuits ASCII + Latin Ext + Cyrillic |

The grid `VecDeque` makes full-screen scroll O(1) (pop_front +
push_back). Region scrolls inside a custom scroll region stay O(n)
via hand-rolled swap pairs since `VecDeque` has no slice rotate.

The LUT is a 256-entry `LazyLock<[f32; 256]>` populated once at
process start. Bitwise-tested against the original formula in
`renderer::tests::srgb_lut_matches_formula`.

### Atlas partial upload (pre-this-branch)

Each new glyph used to trigger a full 1024×1024 (1 MB) atlas
re-upload via `tex_sub_image_2d`. `FontAtlas` now tracks per-glyph
`DirtyRect`s and the renderer uploads only those rects with
`UNPACK_ROW_LENGTH` set so it can read sub-regions of `atlas_data`.

`Renderer::new` and `reload_font` (font zoom) still do one full
upload — a fresh `etagere::AtlasAllocator` places glyphs at brand
new positions, so the texture really does need full replacement.
They both `dirty_rects.clear()` afterwards so the next `prepare()`
doesn't redundantly re-upload.

## The bench harness

`bench/` is a workspace member crate with a library and five binaries:

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

### Harness gotchas

1. **GPU/compositor settling between runs.** Sleeping <0.3s between
   spawn-then-spawn runs gives 2-3× slower numbers on Wayland+NVIDIA.
   `compare.sh` sleeps 1.5s; for low-noise comparisons sleep 3+
   seconds between runs by hand or use the alternating-runner
   pattern (`/tmp/ab2.sh` in past sessions) which interleaves the
   two binaries to wash out drift.

2. **Single-run noise is ~10-20%.** We've watched the same bench
   bounce 30% between sessions. Trust 10-run trimmed means for
   cross-emulator claims; single runs are for "did I break it?"

3. **Spawning soltty/alacritty has a fixed cost.** For very small
   workloads the spawn dominates. Bump `BENCH_ITERS` until the bench
   itself takes >0.5 s.

4. **gol-c's variance is lower** than the spawn-per-run synthetic
   suite — it runs in a single long-lived terminal window. ~5%
   between runs is typical, so ratios are more meaningful.

## What's still on the table

Things we considered but didn't ship.

### Damage tracking in the renderer

`Renderer::prepare` iterates every cell every frame, doing
`atlas.ensure(ch)` (already cheap for ASCII via the array cache)
and packing one `CellInstance` per cell. Most frames have small
changes. A "row dirty" bit on `Grid::lines` that the parser marks
when `put_char`/`clear_with` mutates a row, and `prepare` clears,
would let us skip the whole inner loop for unchanged rows.

This would matter most for big grids with sparse updates (vim,
editor sessions). The synthetic suite and gol-c don't really
exercise that pattern — every cell changes per iteration. Lower
priority than it looks.

### Persistent-mapped buffer

We orphan the instance VBO each frame (`buffer_data_size` with no
data, then `buffer_sub_data` to fill). The driver may or may not
allocate a new buffer internally to avoid a stall. A persistent-
mapped + coherent buffer (or a manual triple-buffer ring) would
remove that question. Not currently the bottleneck per profile —
the GPU driver bucket is ~13% and most of that is `swap_buffers`,
not upload.

### CellInstance packing

Currently 64 bytes per cell — two `[f32; 4]` colors plus four
`[u32; 2]` integer fields. Could pack to ~24 bytes by storing
colors as `u8 RGBA` and the geometry fields as `u16`. Halves
upload bandwidth and CPU memory pressure during prepare. Trade:
the vertex shader has to do the sRGB→linear conversion, which is
fine on GPU.

Not measured to be the bottleneck; included for completeness.

### Atlas eviction

The atlas allocator never frees space. A long session with
thousands of unique glyphs (CJK + emoji + math + …) eventually
fills the 1024×1024 atlas, and after that new glyphs draw as
`.notdef`. An LRU eviction policy on `glyphs` + a free-list in the
allocator would solve it. ~50 lines, low priority unless someone
hits it.

## Where to look in the code

| concern                | file:lines (approx)                                          |
|------------------------|--------------------------------------------------------------|
| Parser fast paths      | `src/term.rs::Term::feed` (the `if performer.in_ground` block) |
| `apply_sgr_simple`     | `src/term.rs::apply_sgr_simple`                              |
| Frame throttle         | `src/main.rs::about_to_wait`, `maybe_request_redraw`         |
| Frame-rate detection   | `src/main.rs::detect_frame_interval` (env, monitor, default, clamp) |
| Burst throttle         | `src/main.rs` constants `BURST_*`, `App::burst_holdoff`      |
| sRGB LUT               | `src/renderer.rs::SRGB_LUT`, `srgb_to_linear`                |
| ASCII glyph cache      | `src/font.rs::FontAtlas::ascii_glyphs`                       |
| Grid scroll (VecDeque) | `src/grid.rs::scroll_up_in_region`                           |
| DSR reply              | `src/term.rs::Performer::dsr`                                |
| Atlas dirty rects      | `src/font.rs::FontAtlas::rasterize`, `dirty_rects: Vec<DirtyRect>` |
| Atlas upload           | `src/renderer.rs::prepare` (the `atlas.dirty_rects` block)   |
| DSR-ping in benches    | `bench/src/lib.rs::ping_terminal`                            |

## Working on perf yourself

The shortest viable loop:

1. `cargo build --release` (everything; bench crate too)
2. A/B against the previous build:
   - `cp target/release/soltty target/release/soltty.now`
   - `git stash && cargo build --release && cp target/release/soltty target/release/soltty.prev && git stash pop`
   - run a script that alternates the two and reports trimmed means
3. For absolute-vs-alacritty claims, run `compare.sh --runs 10`

Don't trust a single `compare.sh` invocation to tell you anything
absolute. Trust it to tell you a *change* in your code (run it
before, change one thing, run it after — better yet, alternate).

When in doubt, profile:

```sh
SOLTTY_FONT_PX=10 perf record -F 1500 --call-graph dwarf -o /tmp/perf.data \
    -- ./target/release/soltty -e bash -lc \
       "GOL_BENCH_ITERS=400 GOL_FORCE_COLS=200 GOL_FORCE_ROWS=50 \
        GOL_BENCH_OUT=/tmp/gol.txt /path/to/gol"
perf report -i /tmp/perf.data --stdio --no-children -g none --percent-limit 0.8
```

The release profile keeps line-table debuginfo (`debug = "line-tables-only"`
in `Cargo.toml`), so dwarf unwinding works without a separate symbols build.
