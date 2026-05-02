# soltty

A GPU-accelerated terminal emulator for Linux and macOS, written in Rust.
Built on `winit` + `glutin` + `glow` (OpenGL), with a custom inline
ANSI parser and a single-instanced-draw-call rendering pipeline.

## Why

A terminal's layout is trivial — a fixed grid of fixed-width cells —
which makes it a good fit for a tightly-scoped GPU pipeline. soltty
treats it that way: one instanced draw call per frame, alpha-only
glyph atlas, manual sRGB linearization, no shaping for ASCII (which
is 95% of terminal traffic).

## Performance

Latest benchmarks against alacritty 0.17 on the same hardware
(NVIDIA + Wayland, 240 Hz panel, 10-run trimmed means):

| bench               | ratio  |
|---------------------|-------:|
| `truecolor_grid`    | **2.05×** |
| `raw_throughput`    | **1.65×** |
| `cursor_jumps`      | **1.18×** |
| `scroll_storm`      | 0.93× |
| gol-c (10 px)       | 0.91× |
| `glyph_churn`       | 0.86× |

Three convincing wins, three within parity-noise. See
`docs/performance.md` for what got us there and what's still on the
table.

## Features

- **Themes** — 504 vendored from
  [iTerm2-Color-Schemes](https://github.com/mbadolato/iTerm2-Color-Schemes),
  picker overlay on `Ctrl+Shift+T`, drop additional `.toml` files into
  `~/.config/soltty/themes/` to extend.
- **Vi mode** — modal cursor with motions (`hjkl`, `wWbBeE`, `0^$`,
  `gg`/`G`, `H`/`M`/`L`, `Ctrl-u`/`d`, `Ctrl-b`/`f`), visual modes
  (`v`/`V`/`Ctrl-v`), text objects (`yiw`/`yaw`/`yiW`/`yaW`), counts,
  forward/backward search (`/`/`?`), yank to clipboard. Default
  activation: `Ctrl+N` (configurable).
- **Mouse selection** — drag to select, double/triple click for word /
  line, Shift+click extends. Block-rect selection from vi mode
  (`Ctrl-v`). Middle-click pastes primary, `Ctrl+Shift+V` pastes
  clipboard. Bracketed paste (`?2004h`) honored.
- **Mouse reporting** — DECSET 1000 / 1002 / 1003 / 1006 forward
  events to the PTY for TUIs.
- **Wide chars** — CJK, fullwidth, and emoji (luminance-collapsed) lay
  out at width 2; combining marks at width 0.
- **Bold / italic / underline / strikethrough / inverse** — separate
  font variants for bold and italic; the rest in shader.
- **Cursor shapes** — block / underline / bar / hollow-block via
  DECSCUSR; bar and underline blink, block stays steady.
- **OSC 10 / 11 / 12** — apps can query the active theme colors.
- **DECCKM** — application cursor keys for vim/tmux.
- **Native Wayland clipboard** via `wl_data_device` (no shelling out
  to `wl-copy` for the read path; segfault-prone on some compositors).
- **Per-user config** at `~/.config/soltty/soltty.conf` — auto-seeded
  on first run with a commented template (TOML subset).
- **Font fallback chain** — primary (JetBrainsMono by default) →
  DejaVuSansMono on Linux / Menlo on macOS. Bold/italic variants
  discovered by suffix substitution; per-style fallback chains.

## Usage

```sh
soltty                              # spawn $SHELL
soltty -e bash -lc 'echo hello'     # run a command
soltty --theme dracula              # one-shot theme override
soltty --list-themes                # dump the bundled list
```

Common keybinds:

| keys              | action                                |
|-------------------|---------------------------------------|
| `Ctrl+Shift+T`    | open theme picker                     |
| `Ctrl+Shift+V`    | paste clipboard                       |
| middle-click      | paste primary selection               |
| `Ctrl+=` / `Ctrl++` / `Ctrl+-` / `Ctrl+0` | font zoom in / out / reset |
| `Ctrl+N` (default)| enter vi mode (`Esc` to exit)         |
| mouse wheel       | scroll history                        |

Config keys (see `~/.config/soltty/soltty.conf` after first run for
the full template):

```toml
[appearance]
theme     = "Brogrammer"
# font_size = 21.0
# font     = "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf"

[behavior]
# frame_hz = 0           # 0 = monitor refresh; clamped to [60, 240]

[keybinds]
# vi_key   = "ctrl+n"
```

Env-var overrides take precedence over the config file (see
`soltty.conf` for the full list).

## Building

```sh
cargo build --release
./target/release/soltty
```

Linux Wayland needs `wayland-client` system libs; macOS needs nothing
beyond Xcode CLT. We don't currently ship a Windows build.

The release profile keeps line-table debuginfo so `perf record
--call-graph dwarf` works without a separate symbols build.

## Layout

```
src/
├── main.rs              # entry point, event loop, key encoding, throttle
├── pty.rs               # spawn shell, reader thread, bounded channel
├── term.rs              # Term::feed: parser fast paths + vte fallback
├── grid.rs              # cell/row/grid model, scrollback, cursor rules
├── renderer.rs          # GPU pipeline, instance pack, atlas upload
├── shader.{vert,frag}   # GLSL 330 cell shader
├── tint.{vert,frag}     # vi-mode wash overlay
├── font.rs              # FontAtlas, swash rasterization, fallback chain
├── gpu.rs               # glutin context + Renderer wrapper
├── theme.rs             # theme loader (built-in + user)
├── picker.rs            # theme picker modal
├── selection.rs         # mouse selection geometry
├── vi.rs                # vi-mode state machine, motions, search
├── clipboard.rs         # cross-platform clipboard chain
├── clipboard_wayland.rs # native wl_data_device read path
└── config.rs            # TOML-subset parser for soltty.conf

bench/                   # workspace member: synthetic benchmarks
docs/
├── how-it-works.md      # architecture walkthrough
├── performance.md       # benches, optimizations, bench harness
└── soltty.conf.example  # template seeded into ~/.config on first run
themes/                  # 504 .toml themes vendored at build time
```

110 unit tests; run with `cargo test --release`.

## Reading order

For the full mental model:

1. `docs/how-it-works.md` — architecture top-down
2. `docs/performance.md` — what's been optimized and how to measure
3. `src/main.rs` → `src/term.rs` → `src/renderer.rs` in that order

## License

MIT.
