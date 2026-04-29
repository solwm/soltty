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

- **`themes/*.toml`** — vendored themes in iTerm2-Color-Schemes Alacritty
  TOML format. `build.rs` walks this directory at compile time, parses
  each file with a tiny TOML-subset state machine (~50 LOC, no `toml`
  crate dep), and emits a static `BUILTIN_THEMES` array into
  `OUT_DIR/themes_data.rs` that `src/theme.rs` includes.
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
rasterized lazily when the renderer encounters it for the first time:

```rust
// In Renderer::prepare:
for vrow in 0..rows {
    for cell in &term.viewport_row(vrow).cells {
        if !is_blank_glyph(cell.ch) { atlas.ensure(cell.ch); }
    }
}
if atlas.atlas_dirty {
    upload_atlas_full(queue, &self.atlas_texture, atlas);
    atlas.atlas_dirty = false;
}
```

The atlas re-upload is `1 MB` worst case (`R8 1024x1024`), but only
happens on frames where new glyphs were added — i.e. essentially never
in steady state.

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

Originally the cursor was "swap fg/bg at the cursor cell." That worked
in the trivial case (default colors) but broke quietly when a cell had
a custom background that happened to be similar to the page foreground.

The fix in `Renderer::prepare`:

```rust
if cursor == Some((vrow, col_idx)) {
    bg = self.cursor_bg;       // fixed near-white
    fg = self.cursor_fg;       // fixed = page bg
}
```

The cursor cell now has constant colors regardless of what's underneath.
The glyph (if any) is drawn in `cursor_fg`, which is the page background
color, so it looks like it's "punched out" of the cursor block. Standard
block-cursor look, predictable contrast.

There's also the policy choice that **`?25l` (DECTCEM hide) is ignored
on the primary screen.** The reason is concrete: a buggy program (gol-c
in our test corpus) hides the cursor and crashes without restoring it,
and you'd be left with a permanently invisible cursor at the shell
prompt. Vim/less/htop all use the *alt* screen, so they're unaffected —
they still get to manage their own cursor visibility there.

This is `Term::viewport_cursor`:

```rust
let visible = if self.on_alt { g.cursor.visible } else { true };
```

It's an opinionated divergence from spec, but it's the kind of opinion
soltty was built to hold.

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
- **DECSCUSR cursor styles.** Programs send `CSI <n> SP q` to pick block
  vs. underscore vs. bar (and steady vs. blinking). We always render a
  steady block.
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
- **Cursor invisible.** First check `viewport_cursor` is returning
  `Some` (it ignores `?25l` on primary, so this should rarely fail).
  Then check the cursor cell really is being inverted in
  `Renderer::prepare`.
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
