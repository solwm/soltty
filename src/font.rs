use std::collections::HashMap;
use std::path::{Path, PathBuf};

use etagere::{size2, AtlasAllocator};
use swash::scale::image::Image as SwashImage;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::FontRef;

use crate::config::Config;

/// Which face to use for a glyph. Derived from cell attrs (BOLD /
/// ITALIC bits) at render time. The atlas keys glyphs by `(char, style)`
/// so we can hold separate rasterizations of e.g. `A` Regular vs Bold.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum FontStyle {
    Regular = 0,
    Bold = 1,
    Italic = 2,
    BoldItalic = 3,
}

impl FontStyle {
    /// Map cell attribute bits to a style. Pure data → trivial test.
    pub fn from_attrs(bold: bool, italic: bool) -> Self {
        match (bold, italic) {
            (false, false) => FontStyle::Regular,
            (true, false) => FontStyle::Bold,
            (false, true) => FontStyle::Italic,
            (true, true) => FontStyle::BoldItalic,
        }
    }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct GlyphEntry {
    pub atlas_x: u16,
    pub atlas_y: u16,
    pub w: u16,
    pub h: u16,
    /// px from cell top-left to glyph top-left
    pub offset_x: i16,
    pub offset_y: i16,
}

/// One contiguous region of `atlas_data` that's been mutated since the
/// last GPU upload. The renderer drains this list every frame and
/// uploads only these rects via `tex_sub_image_2d` — saves ~1 MB of
/// PCIe traffic per new glyph compared to re-uploading the whole atlas.
#[derive(Copy, Clone, Debug)]
pub struct DirtyRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Copy, Clone, Debug)]
pub struct CellMetrics {
    pub cell_w: u32,
    pub cell_h: u32,
    pub baseline: u32,
}

/// One font loaded into memory. We hold the raw bytes + the offset into them
/// because `swash::FontRef` is a thin view that re-parses cheaply on each
/// `from_offset`. Keeping the bytes owned lets us hand out fresh refs without
/// lifetime games.
struct LoadedFont {
    data: Vec<u8>,
    offset: u32,
}

impl LoadedFont {
    fn font(&self) -> Option<FontRef<'_>> {
        FontRef::from_offset(&self.data, self.offset)
    }
}

pub struct FontAtlas {
    /// All loaded fonts (primary + variant primaries + fallbacks). We
    /// hold the bytes once each; chains below reference them by index.
    fonts: Vec<LoadedFont>,
    /// Search chains per FontStyle. Each chain is a list of indices
    /// into `fonts`; rasterize walks them and picks the first font
    /// that has the codepoint. Chain order: variant primary (if loaded)
    /// followed by the shared fallback list.
    chains_regular: Vec<usize>,
    chains_bold: Vec<usize>,
    chains_italic: Vec<usize>,
    chains_bold_italic: Vec<usize>,
    px_size: f32,
    pub metrics: CellMetrics,
    pub atlas_w: u32,
    pub atlas_h: u32,
    pub atlas_data: Vec<u8>,
    /// Per-glyph rects added to the atlas since the last GPU upload.
    /// Empty == clean, no upload needed.
    pub dirty_rects: Vec<DirtyRect>,
    /// ASCII (0..=0x7F) × 4 styles = 512 slots, direct-indexed. Avoids
    /// the HashMap hash for the 95%+ case (typical workloads are
    /// overwhelmingly ASCII). `None` = not yet rasterized; `Some(empty)`
    /// = no glyph in any chain font (cache the miss so we don't try
    /// again next frame).
    ascii_glyphs: [Option<GlyphEntry>; 128 * 4],
    /// Keyed by `(codepoint, style)`. Holds non-ASCII codepoints that
    /// fall through the ASCII array fast path.
    glyphs: HashMap<(char, FontStyle), GlyphEntry>,
    allocator: AtlasAllocator,
    scale_ctx: ScaleContext,
}

impl FontAtlas {
    pub fn new(px_size: f32, config: &Config) -> std::io::Result<Self> {
        let primary_path = discover_font(config).ok_or_else(|| {
            io_err("no monospace font found; set SOLTTY_FONT or `font` in soltty.conf")
        })?;
        log::info!("font: {}", primary_path.display());
        let primary_data = std::fs::read(&primary_path)?;

        let primary_font =
            FontRef::from_index(&primary_data, 0).ok_or_else(|| io_err("could not parse font"))?;
        let primary_offset = primary_font.offset;

        // Cell metrics come from the primary only. Fallback glyphs are
        // rendered at the same px_size and slotted into primary-sized
        // cells; their advance may not match exactly, but the alternative
        // (unrenderable symbol) is worse.
        let metrics = compute_cell_metrics(&primary_font, px_size);
        log::info!(
            "cell: {}x{} (baseline={})",
            metrics.cell_w,
            metrics.cell_h,
            metrics.baseline
        );

        let mut fonts: Vec<LoadedFont> = vec![LoadedFont {
            data: primary_data,
            offset: primary_offset,
        }];

        // Regular chain starts with the primary; all other chains are
        // built up below.
        let mut chains_regular: Vec<usize> = vec![0];
        let mut chains_bold: Vec<usize> = Vec::new();
        let mut chains_italic: Vec<usize> = Vec::new();
        let mut chains_bold_italic: Vec<usize> = Vec::new();

        // Load broad-coverage fallbacks for symbols the primary lacks.
        // Each fallback is appended to *every* style's chain — without
        // bold/italic fallback variants, a missing-glyph in bold mode
        // shows the regular DejaVu glyph, which is acceptable; the
        // alternative (empty cell) is not.
        let primary_canonical = primary_path.canonicalize().ok();
        for path in discover_fallback_fonts() {
            if path.canonicalize().ok() == primary_canonical {
                continue; // primary already loaded under a different alias
            }
            let data = match std::fs::read(&path) {
                Ok(d) => d,
                Err(e) => {
                    log::debug!("fallback {} unreadable: {e}", path.display());
                    continue;
                }
            };
            let Some(font) = FontRef::from_index(&data, 0) else {
                log::debug!("fallback {}: unparseable", path.display());
                continue;
            };
            let offset = font.offset;
            log::info!("font fallback: {}", path.display());
            let idx = fonts.len();
            fonts.push(LoadedFont { data, offset });
            chains_regular.push(idx);
            chains_bold.push(idx);
            chains_italic.push(idx);
            chains_bold_italic.push(idx);
        }

        // Variant primaries (Bold / Italic / BoldItalic). If a variant
        // exists, prepend it to its style's chain — it gets searched
        // before the shared fallbacks, so JetBrainsMono-Bold wins for
        // glyphs it has, and only missing chars fall through to
        // DejaVuSansMono (regular).
        for (style, label) in [
            (FontStyle::Bold, "bold"),
            (FontStyle::Italic, "italic"),
            (FontStyle::BoldItalic, "bold-italic"),
        ] {
            let Some(path) = discover_variant(&primary_path, style, config) else {
                continue;
            };
            let Ok(data) = std::fs::read(&path) else {
                log::debug!("variant {} unreadable: {}", label, path.display());
                continue;
            };
            let Some(font) = FontRef::from_index(&data, 0) else {
                log::debug!("variant {}: unparseable", path.display());
                continue;
            };
            let offset = font.offset;
            log::info!("font {label}: {}", path.display());
            let idx = fonts.len();
            fonts.push(LoadedFont { data, offset });
            let chain = match style {
                FontStyle::Bold => &mut chains_bold,
                FontStyle::Italic => &mut chains_italic,
                FontStyle::BoldItalic => &mut chains_bold_italic,
                FontStyle::Regular => unreachable!(),
            };
            chain.insert(0, idx);
        }

        let (atlas_w, atlas_h) = (1024u32, 1024u32);
        let atlas_data = vec![0u8; (atlas_w * atlas_h) as usize];
        let allocator = AtlasAllocator::new(size2(atlas_w as i32, atlas_h as i32));

        let mut atlas = Self {
            fonts,
            chains_regular,
            chains_bold,
            chains_italic,
            chains_bold_italic,
            px_size,
            metrics,
            atlas_w,
            atlas_h,
            atlas_data,
            dirty_rects: Vec::new(),
            ascii_glyphs: [None; 128 * 4],
            glyphs: HashMap::new(),
            allocator,
            scale_ctx: ScaleContext::new(),
        };

        // Pre-bake printable ASCII in Regular so the common case is hot
        // from frame 0. Bold/italic ASCII gets rasterized lazily.
        for code in 0x20u8..=0x7Eu8 {
            atlas.rasterize(code as char, FontStyle::Regular);
        }

        Ok(atlas)
    }

    pub fn ensure(&mut self, ch: char, style: FontStyle) {
        let c = ch as u32;
        if c < 128 {
            // Hot path: ASCII array. One bounds-checked load to see if
            // we already have it.
            let idx = (c as usize) * 4 + style as usize;
            if self.ascii_glyphs[idx].is_some() {
                return;
            }
        } else if self.glyphs.contains_key(&(ch, style)) {
            return;
        }
        self.rasterize(ch, style);
    }

    pub fn get(&self, ch: char, style: FontStyle) -> Option<GlyphEntry> {
        let c = ch as u32;
        if c < 128 {
            // Direct index, no hash. ~10ns vs ~30ns for the HashMap
            // path; matters because this is called per-cell per-frame.
            self.ascii_glyphs[(c as usize) * 4 + style as usize]
        } else {
            self.glyphs.get(&(ch, style)).copied()
        }
    }

    /// Internal: store a freshly-rasterized glyph. Picks ASCII array
    /// vs. HashMap by codepoint range.
    fn store_glyph(&mut self, ch: char, style: FontStyle, entry: GlyphEntry) {
        let c = ch as u32;
        if c < 128 {
            self.ascii_glyphs[(c as usize) * 4 + style as usize] = Some(entry);
        } else {
            self.glyphs.insert((ch, style), entry);
        }
    }

    fn rasterize(&mut self, ch: char, style: FontStyle) {
        // Walk the style's chain to find a font with the codepoint.
        // Variant primary first (when one exists), then the shared
        // fallbacks. None match → cache empty so we don't retry next
        // frame; same policy as before, just per-style now.
        let font_idx = {
            let chain: &[usize] = match style {
                FontStyle::Regular => &self.chains_regular,
                FontStyle::Bold => &self.chains_bold,
                FontStyle::Italic => &self.chains_italic,
                FontStyle::BoldItalic => &self.chains_bold_italic,
            };
            let mut found: Option<usize> = None;
            for &idx in chain {
                let Some(font) = self.fonts[idx].font() else {
                    continue;
                };
                if font.charmap().map(ch) != 0 {
                    found = Some(idx);
                    break;
                }
            }
            found
        };
        let font_idx = match font_idx {
            Some(i) => i,
            None if ch == '\0' => 0, // primary's .notdef for a literal NUL
            None => {
                self.store_glyph(ch, style, GlyphEntry::default());
                return;
            }
        };

        // Re-fetch the FontRef from the chosen slot. The borrow checker
        // wants this here (rather than caching from the search loop)
        // because `self.scale_ctx.builder(font)` below needs mutable access
        // to a different field of `self`, which requires the immutable
        // borrow we just made to live in this scope only.
        let Some(font) = self.fonts[font_idx].font() else {
            return;
        };
        let glyph_id = font.charmap().map(ch);

        let mut scaler = self
            .scale_ctx
            .builder(font)
            .size(self.px_size)
            .hint(true)
            .build();

        let mut image = SwashImage::new();
        let ok = Render::new(&[Source::Outline, Source::Bitmap(StrikeWith::BestFit)]).render_into(
            &mut scaler,
            glyph_id,
            &mut image,
        );
        if !ok {
            self.store_glyph(ch, style, GlyphEntry::default());
            return;
        }

        let placement = image.placement;
        let (w, h) = (placement.width, placement.height);
        if w == 0 || h == 0 {
            self.store_glyph(ch, style, GlyphEntry::default());
            return;
        }

        // The copy loops below index `image.data` by `placement` dimensions
        // (×bytes-per-pixel for the content kind). swash *should* return a
        // buffer that matches, but a corrupt/odd embedded-bitmap or color
        // strike could return fewer bytes — indexing it would panic. Bail
        // to a blank glyph rather than crash the whole terminal.
        if image.data.len() < expected_bitmap_len(image.content, w, h) {
            log::warn!("glyph {ch:?} bitmap shorter than placement; dropping");
            self.store_glyph(ch, style, GlyphEntry::default());
            return;
        }

        // 1px gutter between glyphs prevents bilinear bleed at atlas edges.
        let alloc = match self.allocator.allocate(size2(w as i32 + 1, h as i32 + 1)) {
            Some(a) => a,
            None => {
                log::warn!("glyph atlas full, dropping {ch:?}");
                self.store_glyph(ch, style, GlyphEntry::default());
                return;
            }
        };
        let ax = alloc.rectangle.min.x as u32;
        let ay = alloc.rectangle.min.y as u32;

        match image.content {
            swash::scale::image::Content::Mask => {
                for row in 0..h {
                    let src = (row * w) as usize;
                    let dst = ((ay + row) * self.atlas_w + ax) as usize;
                    self.atlas_data[dst..dst + w as usize]
                        .copy_from_slice(&image.data[src..src + w as usize]);
                }
            }
            swash::scale::image::Content::Color => {
                // Color emoji: collapse to luminance for now; proper RGBA path is
                // a future milestone.
                for row in 0..h {
                    for col in 0..w {
                        let i = ((row * w + col) * 4) as usize;
                        let r = image.data[i] as u32;
                        let g = image.data[i + 1] as u32;
                        let b = image.data[i + 2] as u32;
                        let a = image.data[i + 3] as u32;
                        let luma = ((r * 299 + g * 587 + b * 114) / 1000) * a / 255;
                        let dst = ((ay + row) * self.atlas_w + ax + col) as usize;
                        self.atlas_data[dst] = luma as u8;
                    }
                }
            }
            swash::scale::image::Content::SubpixelMask => {
                // We don't request subpixel rendering; treat as opaque grayscale fallback.
                for row in 0..h {
                    let src = (row * w * 3) as usize;
                    for col in 0..w {
                        let r = image.data[src + (col * 3) as usize] as u32;
                        let g = image.data[src + (col * 3 + 1) as usize] as u32;
                        let b = image.data[src + (col * 3 + 2) as usize] as u32;
                        let avg = ((r + g + b) / 3) as u8;
                        let dst = ((ay + row) * self.atlas_w + ax + col) as usize;
                        self.atlas_data[dst] = avg;
                    }
                }
            }
        }

        self.dirty_rects.push(DirtyRect { x: ax, y: ay, w, h });
        self.store_glyph(
            ch,
            style,
            GlyphEntry {
                atlas_x: ax as u16,
                atlas_y: ay as u16,
                w: w as u16,
                h: h as u16,
                offset_x: placement.left as i16,
                // swash placement.top is "rows above baseline"; convert to
                // px-from-cell-top.
                offset_y: (self.metrics.baseline as i32 - placement.top) as i16,
            },
        );
    }
}

/// Minimum `image.data` length the rasterizer's copy loops will index for
/// a `w`×`h` bitmap of the given content kind: 1 byte/px for `Mask`, 3 for
/// `SubpixelMask`, 4 for `Color`. Used to reject short buffers before they
/// can panic an out-of-bounds slice.
fn expected_bitmap_len(content: swash::scale::image::Content, w: u32, h: u32) -> usize {
    use swash::scale::image::Content;
    let bpp = match content {
        Content::Mask => 1,
        Content::SubpixelMask => 3,
        Content::Color => 4,
    };
    (w as usize) * (h as usize) * bpp
}

fn compute_cell_metrics(font: &FontRef<'_>, px: f32) -> CellMetrics {
    let m = font.metrics(&[]).scale(px);
    let charmap = font.charmap();
    // For monospace fonts, every glyph has the same advance; sample 'M'.
    // (Falls back to ascii '0' if 'M' isn't mapped.)
    let g = match charmap.map('M') {
        0 => charmap.map('0'),
        id => id,
    };
    let advance = font.glyph_metrics(&[]).scale(px).advance_width(g);
    let cell_w = advance.ceil().max(1.0) as u32;
    let line_height = m.ascent + m.descent + m.leading;
    let cell_h = line_height.ceil().max(1.0) as u32;
    let baseline = m.ascent.ceil().max(0.0) as u32;
    CellMetrics {
        cell_w,
        cell_h,
        baseline,
    }
}

fn discover_font(config: &Config) -> Option<PathBuf> {
    // Precedence: SOLTTY_FONT env > config file `font` > built-in
    // candidate list. First existing file wins.
    if let Ok(p) = std::env::var("SOLTTY_FONT") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(p) = config.font.as_deref() {
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    const CANDIDATES: &[&str] = &[
        // Linux
        "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
        "/usr/share/fonts/JetBrainsMono/JetBrainsMono-Regular.ttf",
        "/usr/share/fonts/jetbrains-mono/JetBrainsMono-Regular.ttf",
        "/usr/share/fonts/truetype/jetbrains-mono/JetBrainsMono-Regular.ttf",
        "/usr/share/fonts/TTF/CascadiaMono.ttf",
        "/usr/share/fonts/TTF/FiraCode-Regular.ttf",
        "/usr/share/fonts/truetype/firacode/FiraCode-Regular.ttf",
        "/usr/share/fonts/TTF/Hack-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        // macOS
        "/Library/Fonts/SF-Mono-Regular.otf",
        "/System/Library/Fonts/Menlo.ttc",
        "/Library/Fonts/Menlo.ttc",
        "/System/Library/Fonts/Monaco.ttf",
        // Windows
        r"C:\Windows\Fonts\CascadiaMono.ttf",
        r"C:\Windows\Fonts\consola.ttf",
    ];
    for c in CANDIDATES {
        let p = PathBuf::from(c);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// All paths we'd accept as a Bold/Italic/BoldItalic variant of `primary`.
/// Returned in priority order; `discover_variant` filters for the first
/// one that exists. Pure path math — no filesystem access — so unit
/// tests can assert on candidate construction without needing real
/// fonts on disk.
fn variant_candidates(primary: &Path, style: FontStyle) -> Vec<PathBuf> {
    if matches!(style, FontStyle::Regular) {
        return Vec::new();
    }
    let Some(stem) = primary.file_stem().and_then(|s| s.to_str()) else {
        return Vec::new();
    };
    let ext = primary
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("ttf");
    let parent = primary.parent().unwrap_or_else(|| Path::new(""));

    // Suffix variants in preference order. `Italic` is the canonical
    // name; `Oblique` is what DejaVu and a few others use. For
    // bold-italic we accept both forms plus `BoldOblique`.
    let suffixes: &[&str] = match style {
        FontStyle::Bold => &["Bold"],
        FontStyle::Italic => &["Italic", "Oblique"],
        FontStyle::BoldItalic => &["BoldItalic", "BoldOblique"],
        FontStyle::Regular => unreachable!(),
    };

    let mut out = Vec::with_capacity(suffixes.len() * 2);
    for s in suffixes {
        // 1) Replace "-Regular" suffix on the stem (e.g. JetBrainsMono-Regular).
        if let Some(rest) = stem.strip_suffix("-Regular") {
            out.push(parent.join(format!("{rest}-{s}.{ext}")));
        }
        // 2) Append "-<suffix>" to the bare stem (e.g. DejaVuSansMono → DejaVuSansMono-Bold).
        out.push(parent.join(format!("{stem}-{s}.{ext}")));
    }
    out
}

/// First existing variant for `style`, or `None` if no variant could be
/// found. Precedence: env var > config file > suffix-substitution
/// search on the primary path.
///   - `SOLTTY_FONT_BOLD` / `SOLTTY_FONT_ITALIC` /
///     `SOLTTY_FONT_BOLD_ITALIC` env vars
///   - `font_bold` / `font_italic` / `font_bold_italic` keys in
///     `~/.config/soltty/soltty.conf`
fn discover_variant(primary: &Path, style: FontStyle, config: &Config) -> Option<PathBuf> {
    let env_var = match style {
        FontStyle::Bold => "SOLTTY_FONT_BOLD",
        FontStyle::Italic => "SOLTTY_FONT_ITALIC",
        FontStyle::BoldItalic => "SOLTTY_FONT_BOLD_ITALIC",
        FontStyle::Regular => return None,
    };
    if let Ok(p) = std::env::var(env_var) {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let cfg_path = match style {
        FontStyle::Bold => config.font_bold.as_deref(),
        FontStyle::Italic => config.font_italic.as_deref(),
        FontStyle::BoldItalic => config.font_bold_italic.as_deref(),
        FontStyle::Regular => None,
    };
    if let Some(p) = cfg_path {
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    variant_candidates(primary, style)
        .into_iter()
        .find(|p| p.is_file())
}

/// Broad-coverage fonts loaded after the primary so we can render symbols
/// the primary lacks. JetBrainsMono is missing ballot boxes (U+2610-2612)
/// and the geometric-shape range (U+25A0-25CF), among others; DejaVuSansMono
/// covers all of those and is widely available on Linux. Color emoji fonts
/// are deliberately not here — our atlas is alpha-only, so we'd have to
/// build an RGBA path before they'd be useful.
///
/// Order matters: more specific / better-quality fonts should come first.
/// We don't cap how many we load, but in practice only a handful exist.
fn discover_fallback_fonts() -> Vec<PathBuf> {
    const CANDIDATES: &[&str] = &[
        // Linux — DejaVuSansMono is the workhorse; covers most non-emoji
        // symbol blocks. NotoSansMono fills a few additional gaps.
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
        // macOS — Menlo/Monaco have decent symbol coverage; SF Symbols and
        // Apple Symbols aren't accessible without CoreText, so we don't
        // try to use them.
        "/System/Library/Fonts/Menlo.ttc",
        "/System/Library/Fonts/Monaco.ttf",
        // Windows — Segoe UI Symbol is the broad-coverage symbol font.
        r"C:\Windows\Fonts\seguisym.ttf",
        r"C:\Windows\Fonts\consola.ttf",
    ];
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for c in CANDIDATES {
        let p = PathBuf::from(c);
        if !p.is_file() {
            continue;
        }
        // Two CANDIDATES entries can resolve to the same file via
        // distro-specific symlinks; canonicalize to dedupe.
        let key = p.canonicalize().unwrap_or_else(|_| p.clone());
        if seen.insert(key) {
            out.push(p);
        }
    }
    out
}

fn io_err(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotFound, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_style_from_attrs() {
        assert_eq!(FontStyle::from_attrs(false, false), FontStyle::Regular);
        assert_eq!(FontStyle::from_attrs(true, false), FontStyle::Bold);
        assert_eq!(FontStyle::from_attrs(false, true), FontStyle::Italic);
        assert_eq!(FontStyle::from_attrs(true, true), FontStyle::BoldItalic);
    }

    #[test]
    fn expected_bitmap_len_matches_bytes_per_pixel() {
        use swash::scale::image::Content;
        // The guard in `rasterize` rejects any `image.data` shorter than
        // this; the values must match the per-pixel strides the copy
        // loops use (1 / 3 / 4 bytes).
        assert_eq!(expected_bitmap_len(Content::Mask, 4, 5), 20);
        assert_eq!(expected_bitmap_len(Content::SubpixelMask, 4, 5), 60);
        assert_eq!(expected_bitmap_len(Content::Color, 4, 5), 80);
        // A short buffer (e.g. a 10×10 color glyph reporting only 100 of
        // the 400 bytes it implies) is detected as insufficient.
        let (w, h) = (10u32, 10u32);
        assert!(100 < expected_bitmap_len(Content::Color, w, h));
    }

    #[test]
    fn variant_replaces_regular_suffix() {
        // Most distros lay out fonts as `Foo-Regular.ttf` / `Foo-Bold.ttf`;
        // the candidate set should include the name swap.
        let primary = Path::new("/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf");
        let cands = variant_candidates(primary, FontStyle::Bold);
        assert!(
            cands.iter().any(|p| p.ends_with("JetBrainsMono-Bold.ttf")),
            "expected JetBrainsMono-Bold.ttf among {cands:?}"
        );
    }

    #[test]
    fn variant_appends_for_unsuffixed_stem() {
        // DejaVuSansMono.ttf has no `-Regular` so we just append.
        let primary = Path::new("/usr/share/fonts/TTF/DejaVuSansMono.ttf");
        let cands = variant_candidates(primary, FontStyle::Bold);
        assert!(cands.iter().any(|p| p.ends_with("DejaVuSansMono-Bold.ttf")));
    }

    #[test]
    fn variant_italic_accepts_oblique_alias() {
        // DejaVu uses `Oblique` rather than `Italic`; both should be in
        // the candidate set so the discovery succeeds.
        let primary = Path::new("/usr/share/fonts/TTF/DejaVuSansMono.ttf");
        let cands = variant_candidates(primary, FontStyle::Italic);
        let has_italic = cands
            .iter()
            .any(|p| p.ends_with("DejaVuSansMono-Italic.ttf"));
        let has_oblique = cands
            .iter()
            .any(|p| p.ends_with("DejaVuSansMono-Oblique.ttf"));
        assert!(has_italic && has_oblique, "candidates: {cands:?}");
    }

    #[test]
    fn variant_bold_italic_accepts_both_aliases() {
        // Both `BoldItalic` and `BoldOblique` should be candidates so
        // we work across font families.
        let primary = Path::new("/x/foo.ttf");
        let cands = variant_candidates(primary, FontStyle::BoldItalic);
        assert!(cands.iter().any(|p| p.ends_with("foo-BoldItalic.ttf")));
        assert!(cands.iter().any(|p| p.ends_with("foo-BoldOblique.ttf")));
    }

    #[test]
    fn regular_returns_no_variant_candidates() {
        // Regular is the *primary*, not a variant — there's no path
        // search for it.
        let primary = Path::new("/x/foo.ttf");
        assert!(variant_candidates(primary, FontStyle::Regular).is_empty());
    }

    /// Integration test: requires JetBrainsMono-Regular + -Bold to be
    /// installed at the standard Arch path. Skips silently otherwise so
    /// the CI-style minimal-image case still passes.
    #[test]
    fn atlas_distinguishes_bold_from_regular() {
        let regular = Path::new("/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf");
        let bold = Path::new("/usr/share/fonts/TTF/JetBrainsMono-Bold.ttf");
        if !regular.is_file() || !bold.is_file() {
            eprintln!("skip: required JetBrainsMono Regular + Bold not installed");
            return;
        }
        // Force discover_font to pick our known regular path so the
        // test isn't sensitive to which font happens to come first in
        // the candidate list.
        std::env::set_var("SOLTTY_FONT", regular.as_os_str());

        let mut atlas = FontAtlas::new(16.0, &Config::default()).unwrap();
        atlas.ensure('A', FontStyle::Regular);
        atlas.ensure('A', FontStyle::Bold);
        let r = atlas.get('A', FontStyle::Regular).unwrap();
        let b = atlas.get('A', FontStyle::Bold).unwrap();
        // Bold and regular get separate atlas slots — different positions.
        assert_ne!(
            (r.atlas_x, r.atlas_y),
            (b.atlas_x, b.atlas_y),
            "regular and bold ended up in the same atlas slot"
        );

        std::env::remove_var("SOLTTY_FONT");
    }
}
