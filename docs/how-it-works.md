# How soltty works

This is a walkthrough of the codebase, written as if you're reading the source
top-down for the first time. It assumes you're comfortable with Rust and have
at least bumped into a terminal emulator or a GPU pipeline before, but it
doesn't assume you've shipped one.

The goal isn't a feature list — `git log --oneline` and `cargo test` give you
that. The goal is to show *why the pieces fit together the way they do*, with
enough precision that you can change them without flying blind.

## A four-actor play

Almost everything soltty does is some combination of four crates:

| Role          | Crate          | What it actually owns                                    |
|---------------|----------------|----------------------------------------------------------|
| Window + input| `winit`        | The OS window, keyboard/mouse/scale events               |
| GL context    | `glutin`       | OpenGL context + surface creation per platform           |
| GL bindings   | `glow`         | Thin Rust binding to the OpenGL function pointers        |
| ANSI parser   | `vte`          | The byte-by-byte CSI/OSC/ESC state machine               |
| Rasterizer    | `swash`        | Outline → 8-bit alpha bitmap for one glyph at one size   |
| CLI           | `clap`         | `--theme`, `--list-themes`, `-e`, `--help`, `--version`  |

Plus `portable-pty` for the OS-specific PTY dance (`forkpty` on Unix,
ConPTY on Windows) and `etagere` for shelf-packing glyph rectangles into
the atlas.

## Mouse selection and paste

Three pieces:

- **`src/selection.rs`** — `Selection` struct with `anchor` and `end`
  cells in viewport coords, plus a `dragging` flag. `normalized()` puts
  them in row-major order; `contains(row, col)` is the per-cell hit
  test the renderer uses. Also `word_bounds` and `line_bounds` for
  multi-click expansion.
- **Mouse handlers in `App::window_event`**:
  - `CursorMoved` updates `App::mouse_pos` and, if dragging, advances
    `selection.end`. If the cursor is dragged outside the window the
    viewport auto-scrolls so the user can extend selection past the
    visible region.
  - Left press: starts a new selection. Within 400 ms of the previous
    press at the same cell, escalates: 2 = word, 3 = line. Shift+left
    extends the existing selection's end instead of starting fresh.
  - Left release: freezes the selection and fills *both* the regular
    clipboard and the X11/Wayland *primary* selection so middle-click
    pastes the most recent selection without a Ctrl+Shift+V cycle.
  - Middle press: pastes the primary selection (falls back to clipboard
    if primary is empty).
- **Ctrl+Shift+V**: pastes from the regular clipboard.
- **Bracketed paste**: when DECSET 2004 is on (`Term::bracketed_paste`),
  pasted text is wrapped in `ESC [ 200~ ... ESC [ 201~` so editors can
  tell pasted bytes from typed input.
- **Renderer integration** — `Renderer::prepare` accepts an
  `Option<&Selection>` and applies an inverse-video swap on selected
  cells. Composes after SGR-inverse but before the cursor, so the
  cursor still reads through correctly.

`pixel_to_cell` is integer division of physical pixels by the cell size
(returned by `Renderer::cell_size`). Off-window drags clamp to the
grid's last row/column.

Typing into the PTY also clears the selection — matches xterm
behavior. `extract_text` in `selection.rs` walks the viewport rows in
order, trims trailing whitespace per line, and joins with `\n`. Works
whether you're scrolled into history or at the live grid.

### Clipboard backend chain

Wayland clipboard is messy in 2026. There are *three* possible
protocols:

1. `wl_data_device` — the standard, every compositor supports it,
   but apps need to plumb it through their wl_seat.
2. `wlr-data-control` — wlroots extension. Sway, Hyprland, KDE
   support it.
3. `ext-data-control` — newer extension working its way through
   wayland-protocols.

`arboard` (the canonical Rust clipboard crate) on Linux uses (2) or
(3). Compositors that only implement (1) — Mutter, several minimal
WMs — fail to connect, after which arboard tries to fall back to X11
and times out on Wayland-only setups.

`src/clipboard.rs` implements a fallback chain:

1. **`wl-copy`** (from the `wl-clipboard` package) — preferred when
   `WAYLAND_DISPLAY` is set and the binary is on `$PATH`. Uses
   protocol (1), works on every Wayland compositor.
2. **`arboard`** — used otherwise (X11, macOS, Windows, or as a last
   resort if `wl-copy` is missing).

We don't currently bind to wl_data_device through winit's Wayland
connection. That's the architecturally cleanest answer (would let us
drop the `wl-clipboard` runtime dep) and it's the right next step if
copy/paste UX needs to be more polished.

## VSync and ANSI throughput

This deserves a section because it's a 35%-throughput trap that's easy
to walk into. We use `glutin::SwapInterval::DontWait` rather than
`Wait(1)`. With Wait, every `swap_buffers` blocks the main thread for
up to one vsync frame (~16 ms at 60 Hz). And because PTY drain, parse,
grid mutation, and rendering all run on that same thread, a blocked
swap stalls byte processing.

Measured under a synthetic ANSI-flood benchmark (full-screen truecolor
SGR repaints, gol-c-style):

| | Throughput | vs alacritty |
|---|---:|---:|
| `Wait(1)` (vsync on)  | ~110 MB/s | ~55% |
| `DontWait` (vsync off)| ~178 MB/s | ~85% |

Won't this tear? On Wayland, no — the compositor enforces tearing
prevention regardless of our app's swap interval. On X11, full-screen
animations with `DontWait` can tear; future work to address that
properly is moving the GL context to a worker thread so the main
thread never blocks on swap. For now the X11 case is the lesser
concern.

## Color themes

Two pieces:

- **`themes/*.toml`** — vendored snapshot of every theme in
  `mbadolato/iTerm2-Color-Schemes`'s Alacritty TOML directory (504
  themes as of this writing). `build.rs` walks the directory at
  compile time, parses each file with a tiny TOML-subset state
  machine (~50 LOC, no `toml` crate dep — but it does handle both
  basic `"..."` and literal `'...'` strings, plus `#` inside strings,
  because the upstream collection uses both), and emits a static
  `BUILTIN_THEMES` array into `OUT_DIR/themes_data.rs` that
  `src/theme.rs` includes.
- **`~/.config/soltty/themes/*.toml`** — runtime overrides in the same
  format. Loaded once at startup by `ThemeLib::load`. A user theme with
  the same name as a builtin replaces it.

The `Theme` struct is the 16 ANSI colors plus default fg/bg and cursor
fg/bg — 84 bytes plus the name. Indices 16..=255 of the 256-color palette
are derived at theme-set time from the standard xterm formula (6×6×6
cube + 24-step grayscale ramp), so themes don't need to specify them.

Live switching is a `Renderer::set_theme(&Theme)` call that rebuilds the
linearized palette and the cursor/default colors. The next frame's
instance pack picks up the new colors automatically — no GPU reset, no
shader reload.

### The picker overlay

`Ctrl+Shift+T` opens a centered modal listing every available theme.
While open, Up/Down move the selection cursor and *immediately* call
`set_theme`, so you see what the theme actually looks like before
committing. Enter keeps the new selection; Esc reverts to whatever was
active when the picker opened.

The overlay is rendered by appending extra `CellInstance`s to the
instance buffer **after** the grid instances. Our shader doesn't blend,
so later instances at the same screen pixel just overwrite earlier ones —
that's all "modal on top" needs. State lives in `src/picker.rs`, geometry
is computed once per frame in `Picker::layout`, and the renderer's
`append_picker_overlay` walks the layout and emits cells for the box,
header, and theme list.

### Why GL and not wgpu

Earlier soltty used `wgpu` for portability and the safe API. We switched
because of a measurable performance gap: `hyperfine -e true` showed
soltty at ~520 ms vs alacritty at ~250 ms (1.9× slower), with almost
the entire gap in *system CPU time* — i.e. kernel/driver work during
Vulkan loader init, adapter selection, and device creation.

After switching to `glow + glutin`, the same benchmark gives soltty
~155 ms vs alacritty ~245 ms (1.58× **faster**). The Vulkan setup cost
just doesn't exist on the GL path: one EGL context creation instead of
loader → instance → adapter enumeration → device. We're now closer to
the floor of "what does it cost to open a window and a GL context on
Linux."

Trade we accepted: no Metal/DX12/WebGPU portability and a `Cell`-deep
unsafe block around every GL call. For a Linux/macOS terminal that
emits one instanced draw call per frame, neither is a real loss.

There's no `tokio`, no `async-std`, no message-passing actor framework.
There is one main thread driving winit, plus exactly one helper thread that
blocks on `read()` of the PTY master. That's it.

The central insight that justifies a custom GPU pipeline at all is:

> **A terminal's layout is trivial. It's a fixed grid of fixed-width cells.**

You don't need shaping for ASCII (which is 95% of terminal traffic). You don't
need text wrapping logic — wrapping is `if col == cols { col = 0; row += 1 }`.
You don't need glyph positioning beyond integer pixel offsets. So we get to
collapse what would normally be a multi-stage text pipeline into one instanced
draw call, and we get to be smug about it.

## The four files you should read first

If you read these in order, you'll have a complete mental model:

1. `src/main.rs` — entry point, event loop, key encoding, CLI parsing
2. `src/pty.rs` — spawn shell, reader thread, bounded channel
3. `src/term.rs` — `vte::Perform` impl, viewport into scrollback
4. `src/renderer.rs` + `src/shader.wgsl` — the GPU side

The rest (`grid.rs`, `font.rs`, `gpu.rs`) are exactly what their names
suggest, and you can read them as needed.

## Following a keystroke

Let's trace what happens when you press **Ctrl+C** while a runaway program
is spewing output. This single example exercises the event loop, the
modifier-tracking quirk, the key-to-bytes encoding, the PTY writer, and
indirectly the line discipline in the kernel — so it's the most
information-dense path to walk.

### winit hands us a key event

winit 0.30 uses the `ApplicationHandler` trait. soltty's `App` struct (in
`src/main.rs`) implements it. Two relevant callbacks:

```rust
fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
    match event {
        WindowEvent::ModifiersChanged(mods) => self.modifiers = mods.state(),
        WindowEvent::KeyboardInput { event: KeyEvent { logical_key, text, state: Pressed, .. }, .. } => {
            // ...
        }
        // ...
    }
}
```

The mildly surprising thing is that **winit doesn't put modifier state on
the `KeyEvent` itself.** You have to track it yourself by observing
`ModifiersChanged` events, which fire when the modifier mask changes. So we
keep a `modifiers: ModifiersState` field on `App` and read it inside
`KeyboardInput`.

This is a place I'd grep first if "Ctrl+C doesn't work" ever returned —
forgetting to update `self.modifiers` would silently turn every shortcut
into a plain character.

### Translating the press to bytes

The actual translation lives in `encode_key` (`src/main.rs`), and the rules
it implements are *terminal* rules, not winit rules:

- **Ctrl + a-z** → C0 control byte 0x01..=0x1A. So Ctrl+C → `0x03` (ETX).
- **Ctrl + [/\\/]/^/_/?/Space/@** → other C0 codes (0x1B, 0x1C, 0x1D, 0x1E, 0x1F, 0x7F, 0x00, 0x00).
- **Alt + x** → `ESC` (0x1B) followed by x's bytes. This is the readline
  meta-* convention; it's how Alt+B = "back word" works in bash.
- **Shift+Tab** → `CSI Z`.
- **Modifier-encoded arrows / Home / End** → `CSI 1;<m><letter>` where
  `<m> = 1 + shift + 2*alt + 4*ctrl`. So Shift+Up = `CSI 1;2A`,
  Ctrl+Right = `CSI 1;5C`. This is xterm's standard form, parsed by
  every readline-aware shell, vim, tmux, etc.
- **Modifier-encoded function keys / PageUp/PageDown/Insert/Delete** → the
  tilde-terminated form `CSI <n>;<m>~`.

Why so many cases? Because terminal apps have, for forty years, agreed on
exactly which byte sequences mean which keypresses, and **deviating means
breaking everything from `less` to `vim` to `htop`**. The encoding here is
not creative; it's just looking up the right answer.

A small twist worth noting: when winit gives us `text` (the OS's idea of
what was typed), we use it for plain characters. But when Ctrl is held,
the OS typically gives us `text = None`, so the C0 path runs first and we
ignore `text` entirely.

### The bytes go to the PTY

```rust
pty.write(&bytes);
```

That `pty.write` (`src/pty.rs`) is just `self.writer.write_all(bytes)` on
the master end of the pseudo-terminal. The kernel then does the real work:

1. Bytes appear in the master → slave pipe
2. The slave's *line discipline* (the kernel's tty driver) processes them
3. If `ISIG` is set on the slave's termios (it is, by default — the shell
   doesn't disable it for normal command-line work), the kernel sees `0x03`
   and translates it into a `SIGINT` delivered to the foreground process
   group of the pty session.

So strictly speaking, **soltty does not "send a SIGINT".** It writes a
byte. The kernel's tty layer is what converts that byte into a signal.
This is also why bypassing cooked mode (e.g. running `stty raw`) makes
Ctrl+C deliver `0x03` as a literal byte to the program — the line
discipline isn't translating anymore.

### Why this isn't responsive when the program is spammy

Pre-bounded-channel, this whole pipeline could fall apart if a child
program was busy-looping `printf`. The reader thread would push 8 KB
chunks into an unbounded `mpsc::channel` faster than the main thread
could drain. Each PTY read fires a `UserEvent::PtyData` wakeup; many
hundreds of those could queue ahead of the next `KeyboardInput`.

The fix is in `src/pty.rs`:

```rust
const PTY_CHANNEL_CAP: usize = 64;  // ~512 KB max in flight
let (tx, rx) = sync_channel::<Vec<u8>>(PTY_CHANNEL_CAP);
```

`sync_channel(64)` makes `tx.send` *block* when the channel is full. The
reader thread freezes. The kernel PTY buffer fills. The child's
`write()` blocks. Backpressure all the way down. Each `PtyData` event
now has a bounded amount of work, so keyboard events don't starve.

There's also the question of whether the per-frame parser work (vte +
grid mutation + renderer prepare) can be slow under huge backlogs even
with the cap, and the answer is "yes, but bounded" — the worst case is
512 KB of escape sequences per event, which the vte state machine chews
through in a couple of milliseconds.

## Following a byte of output

Now the other direction: a single character emitted by the shell, all
the way to a fragment shader.

### The reader thread

```rust
std::thread::Builder::new().name("soltty-pty-reader".into()).spawn(move || {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() { break; }
                on_data();
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    on_exit();
});
```

Two important things this thread does that aren't obvious:

1. **`on_data()` after every successful read** — this calls
   `EventLoopProxy::send_event(UserEvent::PtyData)`, the *only* thread-safe
   way to wake winit's event loop out of `ControlFlow::Wait`.

2. **`on_exit()` after the loop terminates** — fires `UserEvent::PtyExited`,
   which causes the main thread to call `event_loop.exit()`. This is what
   makes `Ctrl+D` actually close the window: shell sees EOF, terminates,
   slave fd closes, master `read()` returns 0, reader breaks the loop,
   `on_exit` fires, window closes. Without this, Ctrl+D would print "exit"
   and leave you with a dead window.

### Through `vte` and into the grid

Back on the main thread, when `UserEvent::PtyData` fires:

```rust
let bytes = pty.drain();
self.term.feed(&bytes);
window.request_redraw();
```

`Term::feed` (`src/term.rs`) is where the byte enters the parser:

```rust
pub fn feed(&mut self, bytes: &[u8]) {
    let sb_before = self.primary.scrollback.len();
    let mut parser = std::mem::take(&mut self.parser);
    let mut performer = Performer { term: self };
    for &b in bytes {
        parser.advance(&mut performer, b);
    }
    self.parser = parser;
    // ...viewport anchoring, see below
}
```

There's a small Rust borrow-checker dance here: `vte::Parser::advance`
needs `&mut Performer`, and `Performer` needs `&mut Term` to mutate the
grid. But the parser *also* lives inside `Term`. So we `mem::take` the
parser out for the duration of the call, which leaves a default
`Parser::default()` sitting in `self.parser` temporarily. After
`advance` returns, we put the real parser back. This is cleaner than
splitting `Term` into separate "parser" and "state" structs.

### What `Performer` actually does

`Performer` (also in `src/term.rs`) implements `vte::Perform`, which
gives it callbacks for each kind of escape sequence the parser
recognizes:

```rust
fn print(&mut self, c: char)                                       // a printable character
fn execute(&mut self, byte: u8)                                    // C0 control: \r, \n, \b, \t, BEL
fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8],
                _ignore: bool, action: char)                       // CSI sequences (cursor moves, SGR)
fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8)  // ESC sequences (RI, RIS)
fn osc_dispatch(&mut self, params: &[&[u8]], _bell: bool)          // OSC (window title)
```

Each of these mutates a `Grid` (`src/grid.rs`). `Grid` owns a
`Vec<Row>` (visible lines) and a `VecDeque<Row>` (scrollback, capped at
10,000 lines). When the cursor LFs past `scroll_bot` and the scroll
region is the full screen, the top row gets pushed into scrollback —
that's the only path by which lines enter scrollback.

The cursor itself has a quirk worth knowing about: **xterm-style sticky
wrap.** Writing the last column does *not* immediately wrap; we set
`cursor.wrap_next = true` and only wrap on the *next* print. Without
this, every line ending exactly at the right edge would scroll, which
would be wrong for things like the bash prompt.

### Primary screen vs. alt screen

`Term` actually has two grids: `primary` and an `Option<alt>`. Programs
like vim, less, and htop send `CSI ?1049h` to switch to alt screen — a
fresh blank grid with no scrollback — do their thing, and send
`CSI ?1049l` to switch back. The implementation:

```rust
fn enter_alt(&mut self) {
    if self.on_alt { return; }
    let (rows, cols) = (self.primary.rows, self.primary.cols);
    self.primary.save_cursor();
    self.alt = Some(Grid::new(rows, cols, 0));  // 0 scrollback on alt
    self.on_alt = true;
}
```

The alt grid is allocated lazily on first switch and dropped on exit,
which saves about 1 MB when not in vim.

### Viewport: the user scrolls back

Now, the renderer doesn't draw `grid.lines` directly. It draws *the
viewport*, which is a window into the virtual concatenation of
`scrollback ++ live_grid`. There's a `viewport_offset: usize` on `Term`
that says "how many lines above the bottom of the live grid we're
looking."

```rust
pub fn viewport_row(&self, vrow: usize) -> &Row {
    let g = self.grid();
    let sb_len = g.scrollback.len();
    let top = sb_len.saturating_sub(self.viewport_offset);
    let abs = top + vrow;
    if abs < sb_len { &g.scrollback[abs] } else { &g.lines[abs - sb_len] }
}
```

The mouse wheel adjusts `viewport_offset`. Pressing any key that
produces output snaps it back to 0 (the canonical "scroll-to-bottom on
typing" behavior every other terminal does).

There's one subtle invariant here. If you scroll up to look at history
and the shell prints new lines, those new lines push old scrollback
entries up — and your viewport, anchored to absolute scrollback
indices, would *slide toward the live grid* as scrollback grows. To
prevent that, `feed()` snapshots `scrollback.len()` before parsing and
bumps `viewport_offset` by however many lines got pushed:

```rust
if self.viewport_offset > 0 {
    let sb_after = self.primary.scrollback.len();
    if sb_after > sb_before {
        let added = sb_after - sb_before;
        self.viewport_offset = (self.viewport_offset + added).min(sb_after);
    }
}
```

So a user scrolled into history stays anchored to whatever they were
reading. This is the kind of detail that's easy to get wrong on the
first try and impossible to get right without testing it — see
`viewport_anchors_when_output_arrives_during_scrollback` in the test
module.

## The renderer

This is the part that justifies the project. The design goal was
*uncompromised performance from day 0*, which ruled out high-level text
crates and led to a custom path. Let's see what that path actually is.

### One pipeline, one instance per cell, one draw call

Open `src/renderer.rs` and look at `CellInstance`:

```rust
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable, Default)]
struct CellInstance {
    cell_xy: [u32; 2],         // grid coords (col, row)
    glyph_origin: [u32; 2],    // glyph position in the atlas, in pixels
    glyph_size: [u32; 2],      // glyph extent in atlas, in pixels
    glyph_offset: [i32; 2],    // glyph top-left relative to cell top-left
    fg: [f32; 4],              // linear-light RGBA
    bg: [f32; 4],
}
```

That's 64 bytes per cell. For an 80×24 grid, the entire instance buffer
is 120 KB; for a maximized 200×60 it's 768 KB. We upload the whole thing
every frame. That sounds wasteful and is, in fact, the wrong choice in
the long run, but at this scale `Queue::write_buffer` is a sub-millisecond
operation on any modern GPU and there's no point optimizing it yet.

There's no vertex buffer. The shader uses `vertex_index` (0..6) to pick
which corner of a triangle pair to emit:

```wgsl
var corners = array<vec2<f32>, 6>(
    vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
    vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
);
```

So one instanced draw call (`pass.draw(0..6, 0..instance_count)`) draws
the entire screen.

### The folded shader

The trick that lets us render the whole grid in a single pass is in
`src/shader.wgsl`. Each cell quad covers the full cell rectangle. The
fragment shader figures out, *for each pixel*, whether that pixel is
inside the glyph's bounding box. If yes, it samples the alpha mask. If
no, it returns zero alpha:

```wgsl
let glyph_local = in.cell_local - in.glyph_offset_px;
var alpha: f32 = 0.0;
if (in.glyph_size_px.x > 0.0
    && glyph_local.x >= 0.0 && glyph_local.x < in.glyph_size_px.x
    && glyph_local.y >= 0.0 && glyph_local.y < in.glyph_size_px.y) {
    let uv = (in.glyph_origin_px + glyph_local) / u.atlas_size;
    alpha = textureSample(atlas_tex, atlas_samp, uv).r;
}
return in.bg * (1.0 - alpha) + in.fg * alpha;
```

The output is a manual mix — no GPU blend state. Cells don't overlap, so
nothing in the pipeline needs alpha blending; we composite explicitly in
the fragment.

This is the key benefit over a `quad-per-glyph` design (which is what
`glyphon` and most "easy" wgpu text crates do): we draw `N` instances
where `N = rows * cols`, and we get backgrounds, foregrounds, and the
"empty space around glyphs" all in one pass. With one quad per glyph
you need a separate pass (or a separate set of instances) for cell
backgrounds.

### The atlas

`src/font.rs` builds a single 1024×1024 R8 (alpha-only) texture. Pre-bake
happens in `FontAtlas::new`:

```rust
for code in 0x20u8..=0x7Eu8 {
    atlas.rasterize(code as char);
}
```

So all 95 printable ASCII characters are in the atlas before we render
the first frame. Anything else (Unicode, box-drawing, etc.) gets
rasterized lazily on first encounter, via `atlas.ensure(ch)` from the
renderer's per-cell loop. New glyphs append to `atlas.dirty_rects`; the
renderer drains that list each frame and uploads only the per-glyph
sub-rectangles via `tex_sub_image_2d`. Steady state is no upload at all.
See `docs/performance.md` for the per-rect upload mechanics.

#### Font fallback chain

JetBrainsMono — our default primary — lacks ballot boxes (U+2610..2612),
the geometric-shape range (U+25A0..25CF), and several other symbol
blocks. Without a fallback those characters silently render as empty
cells. So `FontAtlas::new` loads a small chain: the primary first, then
broad-coverage fallbacks (DejaVuSansMono on Linux; Menlo/Monaco on
macOS; Segoe UI Symbol on Windows) discovered by hand-probing fixed
paths.

`rasterize` walks the chain on each new glyph, picks the first font
whose `charmap.map(ch) != 0`, and rasterizes from that one. Cell
metrics still come from the primary, so a fallback glyph wider than the
primary's advance gets clipped to the cell — acceptable since the
alternative is "doesn't render at all".

Color emoji (U+2705 ✅, U+274C ❌, etc.) deliberately stays outside the
chain. Our atlas is alpha-only; doing color glyphs needs an RGBA path
and a separate texture-or-array, which is its own milestone.

Glyph rasterization itself is one `swash::scale::Render` call that
produces an alpha mask. We pack it into the atlas using `etagere`, a
shelf packer:

```rust
let alloc = self.allocator.allocate(size2(w as i32 + 1, h as i32 + 1))?;
```

The `+ 1` on each dimension is a one-pixel gutter to prevent any
imagined bilinear bleed from neighboring glyphs. We actually use
`FilterMode::Nearest` sampling — glyph positions are integer-aligned by
construction, so linear filtering would only soften them — but the
gutter is cheap defense in depth.

### sRGB and gamma

The wgpu surface format we pick is `Bgra8UnormSrgb`. This means the
swap chain texture is in sRGB color space, and wgpu *automatically*
gamma-encodes our linear-light shader output when writing to it. So
the shader does its mixing in linear space and the screen sees correct
sRGB.

We linearize the palette and any truecolor RGB values *once on the CPU*
when packing instances:

```rust
fn srgb_to_linear(c: u8) -> f32 {
    let s = c as f32 / 255.0;
    if s <= 0.04045 { s / 12.92 }
    else { ((s + 0.055) / 1.055).powf(2.4) }
}
```

The shader never knows about gamma; it just blends.

### The cursor

Cursor rendering is entirely on the GPU side. The fragment shader takes
the cursor cell, shape, and two colors as uniforms, then overlays the
chosen shape on top of whatever the cell would otherwise paint:

| Shape       | Code | What the shader does                                                |
|-------------|------|---------------------------------------------------------------------|
| Block       | 1    | Replace the cell's bg/fg with `cursor_bg`/`cursor_fg` so the glyph appears "punched out" of a solid block. |
| Underline   | 2    | Leave the cell as-is, paint a band ~10% of cell height (min 2 px) at the bottom in `cursor_bg`. |
| Bar         | 3    | Leave the cell as-is, paint a column ~15% of cell width (min 2 px) at the left in `cursor_bg`. |
| HollowBlock | 4    | Outline only (~6% of min cell dim) along all four cell edges in `cursor_bg`. Used by vi-mode; apps can't request it. |
| None        | 0    | No overlay — used when the cursor is hidden, scrolled out of view, or unfocused. |

Apps select the shape with DECSCUSR (`CSI Ps SP q`). We don't honor
the spec's blink-vs-steady distinction byte-for-byte: the codes
`1/3/5` (blinking) and `2/4/6` (steady) collapse to shape only.
Mapping is in `term::Performer::set_cursor_shape`. Each grid (primary
and alt) carries its own shape, so vim setting bar-on-insert in alt
screen doesn't leak back to the shell on exit.

#### Blinking

Whether the cursor blinks is decided by *shape*, not by the DECSCUSR
blink bit:

| Shape     | Behavior                                |
|-----------|-----------------------------------------|
| Block     | Steady — never blinks.                  |
| Bar       | Blinks at 1 Hz (500 ms on, 500 ms off). |
| Underline | Blinks at 1 Hz.                         |

The reasoning is UX: Block almost always means a "reading" cursor
(vim normal/visual, zsh-vi-mode normal) where flicker is just
distraction. Bar and Underline mean an "editing" cursor (insert/replace)
where the blink helps you find where you are. This deviates from the
DECSCUSR spec — apps that want a blinking block don't get one — but
the alternative (let apps wreck your reading flow) is worse.

`App::cursor_visible_now` computes the on/off phase from
`(now - blink_epoch) / 500ms`. The renderer takes a `cursor_visible_now`
flag and suppresses the cursor uniform when it's false (same path the
picker uses). `App::about_to_wait` schedules a `WaitUntil` for the
next phase boundary so the event loop doesn't sleep through the
toggle. Typing resets `blink_epoch` so the cursor always lights up
solid right after a keystroke instead of catching mid-off-phase.

Why the shader and not CPU-side color overrides? Selection and SGR
inversion compose into the cell's bg/fg in `Renderer::prepare`; the
cursor needs to win over both. Doing the cursor in the fragment stage
runs after that compositing without needing per-cell instance changes.

The `cursor_bg`/`cursor_fg` colors come from the active theme — the
same pair the theme picker overlay uses for its highlight, which means
they're guaranteed readable against any palette.

`?25l` (DECTCEM hide) is honored on both primary and alt screens. A
program that hides the cursor and crashes without restoring it leaves
the shell prompt cursor-less until something sends `?25h` — typically
`tput cnorm` or a fresh shell. Same behavior as every other terminal.

## Vi mode

A modal cursor for grabbing text without the mouse. Press the activation
key (default `Ctrl+N`, override via `SOLTTY_VI_KEY`) to enter, `Esc` to
exit. While active, the PTY sees nothing — keystrokes drive a separate
vi cursor over the visible viewport (and into scrollback). The default
shadows readline's "next history" binding at the shell prompt; pick a
different combo via `SOLTTY_VI_KEY="ctrl+shift+space"` or similar if
that bothers you.

Indicator: a faint green wash over the whole window so it's obvious
when you're "in" vi mode. Drawn as a separate fullscreen-quad pass
(`src/tint.{vert,frag}`) with alpha blending enabled briefly around the
draw, off otherwise. Color and strength are constants in `src/gpu.rs`
(`VI_TINT_COLOR`).

State lives in `src/vi.rs::ViMode`:

  - `cursor: (row, col)` — vi cursor in viewport coords, independent of
    the terminal cursor. While vi-mode is active the terminal cursor is
    suppressed and only the vi cursor renders, as a hollow block.
  - `visual: VisualMode::{None, Char, Line}` and `visual_anchor` —
    drives the existing mouse `Selection` so all the rendering machinery
    (inverse highlight in `Renderer::prepare`, `extract_text` for yank)
    just works.

Keys:

| Key                       | Action                                                       |
|---------------------------|--------------------------------------------------------------|
| `h` `j` `k` `l`           | Move one cell. Past row 0 scrolls into history; past the bottom scrolls toward live. |
| `0` `^` `$`               | Column 0 / first non-blank / last non-blank of the row.      |
| `gg` `G`                  | Top of scrollback / bottom of live grid.                     |
| `H` `M` `L`               | Top / middle / bottom of the visible viewport.               |
| `Ctrl-u` `Ctrl-d`         | Scroll half page up / down.                                  |
| `Ctrl-b` `Ctrl-f`         | Scroll full page up / down.                                  |
| `w` `e` `b`               | Word next-start / current-or-next-end / previous-start.      |
| `W` `E` `B`               | Same but on WORDs (non-whitespace runs, punctuation glued).  |
| `v`                       | Char-wise visual; further motions extend the selection.      |
| `V`                       | Line-wise visual; selection covers full rows from anchor.    |
| `Ctrl-v`                  | Block-wise visual; selection is a rectangle from anchor to cursor. |
| `y` (in visual)           | Yank selection to clipboard + primary, exit vi-mode.         |
| `y<motion>`               | Yank cursor → motion target (e.g. `yw`, `y$`, `yG`).         |
| `yy` (or `Y`)             | Yank current line; counts work (`3yy`).                      |
| `yiw` `yiW`               | Yank inner word / WORD (run of class around cursor).         |
| `yaw` `yaW`               | Yank around word / WORD (object + adjacent whitespace).      |
| `<count>` (e.g. `5j`)     | Count prefix — repeats the next motion N times.              |
| `/<query>` `?<query>`     | Forward / backward live search. Matches highlight as you type, cursor jumps live to the best one. |
| `Enter` (in search)       | Commit search; matches stay highlighted, `n`/`N` navigates.  |
| `n` `N`                   | Next / previous match relative to original search direction. |
| `Esc`                     | Cancel search → cancel pending count/op → exit visual → exit vi (cascade). |

Motion dispatch lives in `vi::apply_motion` (a free function, not a
`ViMode` method, so the caller in `App::window_event` can pass disjoint
borrows of `vi` and `term` while still holding `&mut self.gpu` from the
destructure at the top of that function). Counts are taken from
`ViMode::pending_count` and the motion is run that many times.

State machine:

  - `pending_count: Option<u32>` — accumulates digit keys (`5j` parses
    `5` here before applying `j`). Bare leading `0` is the LineStart
    motion rather than a digit; once `pending_count` is non-None, `0`
    becomes a digit too.
  - `pending_op: Option<PendingOp>` — set after `g`, cleared by the
    next key. Only `g` completes (to `DocStart`); anything else cancels.
    Future two-key sequences extend this enum.

Word semantics match vim: `word` = `[A-Za-z0-9_]+` runs, `WORD` = any
non-whitespace run. The classifier (`classify` in `vi.rs`) folds
characters into Word / Punct / Whitespace; in `WORD` mode, Punct
collapses into Word. Word motions cross row boundaries — at the bottom
or top of the visible viewport they stop without scrolling, matching
"don't aggressively page through history just because the user typed
`w`."

The activation keybind is parsed by `vi::parse_vi_key` from a string
like `"ctrl+shift+space"` or `"alt+v"`. Comparison against winit
events requires *exact* modifier match — `Ctrl+Shift+Space` doesn't
fire on `Ctrl+Shift+Alt+Space`. Bad env var values log a warning and
fall back to the default.

#### Search

`/query` and `?query` open a live-search modal at the bottom of the
viewport. State lives in `vi::Search`: query buffer, direction, the
match list (`Vec<Match>` in absolute scrollback ++ live coords), the
"current" match index, and the origin cursor + viewport_offset for
Esc-cancel restore.

`vi::find_matches` is a dumb O(N) substring scan — collect each row
into a `String`, walk it with `str::find`. Cells are 1 char each so
column index == char index. Non-overlapping matches: a hit at col `c`
advances the search cursor by `query.len()` so `aaaa`/`aa` returns
two hits, not three.

The renderer takes a `SearchOverlay` from App each frame and:

  - Highlights any cell inside a match. Current match gets a bright
    gold bg with black fg; others get a dim amber bg, fg unchanged.
    Match override happens *after* SGR-inverse and selection-inverse
    in the cell loop, so it always wins — the user can spot matches
    even on a busy background.
  - Renders a search bar at the bottom row showing `/query` or
    `?query` plus a `_` caret while typing.

`n`/`N` navigate matches in the original search direction (vim
semantics — `N` reverses, never just "next"). `Esc` while typing
restores the origin cursor + viewport (so live-preview feels
reversible); `Esc` post-commit dismisses highlights but keeps you
where you are.

#### Block visual

`Ctrl-v` enters block-rect selection. The selection is the axis-aligned
rectangle whose opposite corners are the anchor (where Ctrl-v fired)
and the current vi cursor — no row-major sloshing through the middle.

`selection::Selection` gained a `mode: SelectionMode` field (default
`Char`), with `Block` toggling rect semantics in `contains()` and
`extract_text()`. Block extract preserves trailing whitespace inside
the rect so column-aligned data round-trips through paste — `ls -la`
size-column → clipboard → another terminal stays a clean column of
numbers.

Mouse selection always uses `Char`. Vi `v` / `V` use `Char` (line is
char with full-row columns). Vi `Ctrl-v` is the only producer of
`Block`.

## Window resize

Three things have to happen in lockstep when the user resizes:

1. The wgpu surface is reconfigured to the new dimensions.
2. The `Term`'s internal grid resizes (rows/cols change). Lines are
   added or truncated; the cursor is clamped.
3. The PTY's winsize is updated via `ioctl(TIOCSWINSZ)` — wrapped by
   `MasterPty::resize` in portable-pty. Without this, programs like
   `vim` and `tmux` see the old dimensions and render incorrectly.

The orchestration is in `WindowEvent::Resized` in `src/main.rs`:

```rust
gpu.resize(size.width, size.height);
let (rows, cols) = grid_dims_for_window(gpu.cell_size(), size.width, size.height);
self.term.resize(rows as usize, cols as usize);
if let Some(pty) = self.pty.as_ref() { pty.resize(rows, cols); }
window.request_redraw();
```

`grid_dims_for_window` is just integer division: `cols = width / cell_w`,
`rows = height / cell_h`. We don't reflow scrollback when columns
change — that's a hard problem and most terminals get it subtly wrong.
For now, scrollback rows keep their original column counts; new content
uses the new width.

## Font zoom

Ctrl+= / Ctrl++ / Ctrl+- / Ctrl+0 are intercepted *before* the key
encoder runs:

```rust
if let Some(target) = font_zoom_target(&logical_key, self.modifiers, gpu.font_size()) {
    let cell = gpu.set_font_size(target);
    let (rows, cols) = grid_dims_for_window(cell, inner.width, inner.height);
    self.term.resize(rows as usize, cols as usize);
    if let Some(pty) = self.pty.as_ref() { pty.resize(rows, cols); }
    window.request_redraw();
    return;
}
```

`Gpu::set_font_size` rebuilds the entire `FontAtlas` at the new pixel
size and calls `Renderer::reload_font` (which re-uploads the texture
data and updates the cell-size uniform). The atlas dimensions stay at
1024×1024, so the wgpu texture, view, and bind group don't need to be
recreated — only the contents change. After that we recompute grid
dims, `Term::resize`, `pty.resize`, and trigger a redraw.

Each step is on the order of a millisecond. There's no visible hitch.

## What's deliberately not here

Roughly in order of "you might miss it":

- **Bold/italic font variants.** SGR bold currently changes the cell's
  attribute bits but the renderer ignores them — every glyph uses the
  regular face. Adding bold/italic means loading bold/italic TTF files
  and indexing the atlas by `(face, glyph_id)` instead of `glyph_id`.
- **Application cursor keys (DECCKM).** In some modes vim and tmux
  expect arrow keys to send `SS3 A/B/C/D` instead of `CSI A/B/C/D`. We
  always send `CSI`. Most things still work; specific edge cases in vim
  insert mode might not.
- **Bracketed paste (`?2004h`).** When set, pasted content should be
  wrapped in `ESC [ 200~ ... ESC [ 201~`. We ignore the mode toggle.
- **Mouse reporting.** Wheel scrolls scrollback locally; we don't
  forward clicks/drags as VT mouse sequences.
- **Selection and clipboard.** Drag-to-select isn't implemented.
- **OSC 52 / OSC 8.** Programmatic clipboard writes and hyperlinks.
- **Color emoji.** swash gives us color glyphs (COLR/CBDT) when the font
  has them, but we collapse them to luminance for the alpha-mask atlas.
  A real emoji path needs an RGBA atlas (separate texture or array
  layer) and a shader that knows which one to sample.
- **Ligatures.** Would need `rustybuzz` shaping for runs that contain
  ligature-eligible sequences, and a corresponding break in the
  one-cell-one-instance invariant.
- **Frame pacing.** We redraw on every PTY data event. For chatty
  programs we already coalesce via `request_redraw`, but a smarter
  budget (e.g. cap at 120 Hz) would cut GPU work.

If you want a feature in that list, the right place to start is
usually `src/term.rs` (parser side) or `src/renderer.rs` (visual side).
Most of them are small additions — the architecture has room for them.

## Why each crate, briefly

A few choices that aren't obvious from looking at `Cargo.toml`:

- **`vte` (Alacritty's parser)** instead of writing our own. The state
  machine is small but pedantically correct; reimplementing it is a
  classic "looks easy, isn't" exercise.
- **`swash`** instead of `fontdue`/`ab_glyph`. Fontdue is fine but
  swash is what cosmic-text uses internally — it handles color emoji
  (COLR/CBDT), is fast, and is actively maintained. We don't use most
  of its features yet, but it's the right long-term choice.
- **`portable-pty`** because writing the openpty/forkpty/ConPTY dance
  three times for three OSes is exactly the kind of yak-shaving that
  has nothing to do with the interesting problem.
- **`etagere`** because shelf packing is a small, well-solved problem
  and doing it inline would be 100 lines of distraction.
- **`bytemuck`** for the `Pod`/`Zeroable` derives on instance/uniform
  structs — it's the standard way to safely cast a `&[CellInstance]`
  into the `&[u8]` that `wgpu::Queue::write_buffer` wants.
- **No font discovery crate** (e.g. `font-kit`). We hand-probe a list of
  common system paths and accept a `SOLTTY_FONT` env override. That's
  uglier but adds zero dependencies and is enough.

## Where to look when something breaks

A small triage cheat sheet:

- **Window opens black, no text.** Shader compile failure. Look for
  `validating fragment stage` in stderr.
- **Text renders but at the wrong size or with wrong baseline.** Check
  `cell:` log line at startup (printed by `FontAtlas::new`). The
  numbers come from `compute_cell_metrics` in `font.rs`.
- **Colors look washed out or weirdly saturated.** Gamma. Check that
  the surface is sRGB (`format=Bgra8UnormSrgb` in startup log). If we
  pick a non-sRGB format, our linear-light shader output would display
  raw, looking too dark.
- **Cursor invisible.** Check `Term::viewport_cursor` first — it
  returns `None` if `?25l` (DECTCEM hide) is in effect on either
  screen, or if the cursor scrolled out of view. If a program hid the
  cursor and crashed without restoring, send `tput cnorm`. Past that,
  verify `Renderer::prepare` is setting `cursor_cell` to something
  other than `(-1, -1)` and that `cursor_shape_id` is non-zero.
- **Cursor draws as the wrong shape.** Apps drive shape via DECSCUSR
  (`CSI Ps SP q`); zsh and vim re-set on every prompt / mode change.
  Confirm `Term::cursor_shape()` reflects what the app sent, then
  check the fragment shader's shape branches.
- **Ctrl+C doesn't work.** Either modifiers aren't being tracked
  (verify `WindowEvent::ModifiersChanged` is being processed), or
  the channel is so backed up that `KeyboardInput` is queued behind a
  flood of `PtyData` events. The bounded `sync_channel(64)` should
  prevent the latter.
- **Window won't close on Ctrl+D.** `UserEvent::PtyExited` isn't being
  fired or processed. Reader thread should call `on_exit()` after its
  loop exits.

---

That's the whole picture. ~2,600 lines of Rust + 72 lines of WGSL,
backed by 25 unit tests. The architecture is small enough that the
explanation fits in one document — which is, in a way, the entire
point of building it.
