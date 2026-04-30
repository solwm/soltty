use std::collections::HashMap;
use std::path::PathBuf;

use etagere::{size2, AtlasAllocator};
use swash::scale::image::Image as SwashImage;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::FontRef;

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
    /// Loaded fonts, primary at [0] and fallbacks at [1..]. We try the
    /// primary first when rasterizing a glyph, then walk the fallbacks
    /// in order — first font that has the codepoint wins. JetBrainsMono
    /// (a common primary) lacks ballot boxes, geometric shapes, and
    /// many other symbol blocks, so the chain is what makes box-drawing
    /// task lists and similar UI render at all.
    fonts: Vec<LoadedFont>,
    px_size: f32,
    pub metrics: CellMetrics,
    pub atlas_w: u32,
    pub atlas_h: u32,
    pub atlas_data: Vec<u8>,
    /// Per-glyph rects added to the atlas since the last GPU upload.
    /// Empty == clean, no upload needed.
    pub dirty_rects: Vec<DirtyRect>,
    glyphs: HashMap<char, GlyphEntry>,
    allocator: AtlasAllocator,
    scale_ctx: ScaleContext,
}

impl FontAtlas {
    pub fn new(px_size: f32) -> std::io::Result<Self> {
        let primary_path = discover_font().ok_or_else(|| io_err(
            "no monospace font found; set SOLTTY_FONT to a TTF/OTF path",
        ))?;
        log::info!("font: {}", primary_path.display());
        let primary_data = std::fs::read(&primary_path)?;

        let primary_font = FontRef::from_index(&primary_data, 0)
            .ok_or_else(|| io_err("could not parse font"))?;
        let primary_offset = primary_font.offset;

        // Cell metrics come from the primary only. Fallback glyphs are
        // rendered at the same px_size and slotted into primary-sized
        // cells; their advance may not match exactly, but the alternative
        // (unrenderable symbol) is worse.
        let metrics = compute_cell_metrics(&primary_font, px_size);
        log::info!(
            "cell: {}x{} (baseline={})",
            metrics.cell_w, metrics.cell_h, metrics.baseline
        );

        let mut fonts: Vec<LoadedFont> = vec![LoadedFont {
            data: primary_data,
            offset: primary_offset,
        }];

        // Load broad-coverage fallbacks for symbols the primary lacks.
        // Skipped silently if the file doesn't exist or fails to parse —
        // worst case we end up with primary-only, which is what we had
        // before this commit.
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
            fonts.push(LoadedFont { data, offset });
        }

        let (atlas_w, atlas_h) = (1024u32, 1024u32);
        let atlas_data = vec![0u8; (atlas_w * atlas_h) as usize];
        let allocator = AtlasAllocator::new(size2(atlas_w as i32, atlas_h as i32));

        let mut atlas = Self {
            fonts,
            px_size,
            metrics,
            atlas_w,
            atlas_h,
            atlas_data,
            dirty_rects: Vec::new(),
            glyphs: HashMap::new(),
            allocator,
            scale_ctx: ScaleContext::new(),
        };

        // Pre-bake printable ASCII so the common case is hot from frame 0.
        for code in 0x20u8..=0x7Eu8 {
            atlas.rasterize(code as char);
        }

        Ok(atlas)
    }

    pub fn ensure(&mut self, ch: char) {
        if !self.glyphs.contains_key(&ch) {
            self.rasterize(ch);
        }
    }

    pub fn get(&self, ch: char) -> Option<GlyphEntry> {
        self.glyphs.get(&ch).copied()
    }

    fn rasterize(&mut self, ch: char) {
        // Walk the font chain to find one that has the codepoint. Primary
        // first, then fallbacks in load order. None match → cache empty so
        // we don't try again next frame; this is what kept JetBrainsMono's
        // ballot-box gap silent before we had a fallback chain at all.
        let mut found: Option<usize> = None;
        for (i, f) in self.fonts.iter().enumerate() {
            let Some(font) = f.font() else { continue };
            if font.charmap().map(ch) != 0 {
                found = Some(i);
                break;
            }
        }
        let font_idx = match found {
            Some(i) => i,
            None if ch == '\0' => 0, // render primary's .notdef for a literal NUL
            None => {
                self.glyphs.insert(ch, GlyphEntry::default());
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
        let ok = Render::new(&[Source::Outline, Source::Bitmap(StrikeWith::BestFit)])
            .render_into(&mut scaler, glyph_id, &mut image);
        if !ok {
            self.glyphs.insert(ch, GlyphEntry::default());
            return;
        }

        let placement = image.placement;
        let (w, h) = (placement.width, placement.height);
        if w == 0 || h == 0 {
            self.glyphs.insert(ch, GlyphEntry::default());
            return;
        }

        // 1px gutter between glyphs prevents bilinear bleed at atlas edges.
        let alloc = match self.allocator.allocate(size2(w as i32 + 1, h as i32 + 1)) {
            Some(a) => a,
            None => {
                log::warn!("glyph atlas full, dropping {ch:?}");
                self.glyphs.insert(ch, GlyphEntry::default());
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

        self.dirty_rects.push(DirtyRect {
            x: ax,
            y: ay,
            w,
            h,
        });
        self.glyphs.insert(
            ch,
            GlyphEntry {
                atlas_x: ax as u16,
                atlas_y: ay as u16,
                w: w as u16,
                h: h as u16,
                offset_x: placement.left as i16,
                // swash placement.top is "rows above baseline"; convert to
                // px-from-cell-top.
                offset_y: (self.metrics.baseline as i32 - placement.top as i32) as i16,
            },
        );
    }
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

fn discover_font() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SOLTTY_FONT") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
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
