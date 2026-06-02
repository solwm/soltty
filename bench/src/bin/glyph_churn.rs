//! Print thousands of distinct Unicode characters in a row. Each
//! never-seen-before glyph forces the terminal to (a) miss the cache,
//! (b) rasterize via the font shaper, (c) allocate atlas space, and
//! (d) re-upload the atlas to the GPU. Repeating the same character
//! set every iter measures the steady-state cost; growing the set
//! measures atlas-allocation throughput.
//!
//! Default: 200 passes through ~7,000 BMP code points. The first pass
//! is the worst case (all-miss); subsequent passes hit the glyph cache.

use std::io::Write;

use soltty_bench::Bench;

fn main() {
    let bench = Bench::start("glyph_churn", 200);

    // Pre-build the line of glyphs once. Skip control-char ranges
    // (0..0x20, 0x7f..0xa0), surrogates, the BOM/zero-width joiners
    // we don't want to mess with combining behavior.
    let mut line = Vec::with_capacity(64 * 1024);
    let cols = bench.cols as usize;
    let mut tmp = [0u8; 4];
    let mut col = 0usize;
    let chars_per_row = cols.max(1);
    for cp in 0x0021u32..0x4000 {
        let Some(ch) = char::from_u32(cp) else {
            continue;
        };
        if ch.is_control() || (0xd800..=0xdfff).contains(&cp) {
            continue;
        }
        line.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
        col += 1;
        if col == chars_per_row {
            line.push(b'\n');
            col = 0;
        }
    }
    if col != 0 {
        line.push(b'\n');
    }

    let mut out = bench.writer();
    let mut bytes = 0u64;
    for _ in 0..bench.iters {
        out.write_all(&line).unwrap();
        bytes += line.len() as u64;
    }
    out.flush().unwrap();
    drop(out);

    bench.finish(bytes);
}
