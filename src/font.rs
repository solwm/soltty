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

#[derive(Copy, Clone, Debug)]
pub struct CellMetrics {
    pub cell_w: u32,
    pub cell_h: u32,
    pub baseline: u32,
}

pub struct FontAtlas {
    font_data: Vec<u8>,
    font_offset: u32,
    px_size: f32,
    pub metrics: CellMetrics,
    pub atlas_w: u32,
    pub atlas_h: u32,
    pub atlas_data: Vec<u8>,
    /// Set when atlas_data has been mutated since the last GPU upload.
    pub atlas_dirty: bool,
    glyphs: HashMap<char, GlyphEntry>,
    allocator: AtlasAllocator,
    scale_ctx: ScaleContext,
}

impl FontAtlas {
    pub fn new(px_size: f32) -> std::io::Result<Self> {
        let path = discover_font().ok_or_else(|| io_err(
            "no monospace font found; set SOLTTY_FONT to a TTF/OTF path",
        ))?;
        log::info!("font: {}", path.display());
        let font_data = std::fs::read(&path)?;

        let font = FontRef::from_index(&font_data, 0)
            .ok_or_else(|| io_err("could not parse font"))?;
        let font_offset = font.offset;

        let metrics = compute_cell_metrics(&font, px_size);
        log::info!(
            "cell: {}x{} (baseline={})",
            metrics.cell_w, metrics.cell_h, metrics.baseline
        );

        let (atlas_w, atlas_h) = (1024u32, 1024u32);
        let atlas_data = vec![0u8; (atlas_w * atlas_h) as usize];
        let allocator = AtlasAllocator::new(size2(atlas_w as i32, atlas_h as i32));

        let mut atlas = Self {
            font_data,
            font_offset,
            px_size,
            metrics,
            atlas_w,
            atlas_h,
            atlas_data,
            atlas_dirty: true,
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
        // Build the FontRef inline to avoid borrowing self while we also need
        // &mut self.scale_ctx below.
        let Some(font) = FontRef::from_offset(&self.font_data, self.font_offset) else {
            return;
        };
        let glyph_id = font.charmap().map(ch);
        // glyph_id == 0 is .notdef. Cache it as empty so we don't try again.
        if glyph_id == 0 && ch != '\0' {
            self.glyphs.insert(ch, GlyphEntry::default());
            return;
        }

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

        self.atlas_dirty = true;
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

fn io_err(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotFound, msg)
}
