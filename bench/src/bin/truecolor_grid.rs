//! Repaint the entire grid with random truecolor cells every frame.
//! This mirrors the gol-c stress test: each cell is one SGR
//! (`\x1b[48;2;R;G;Bm `, ~22 bytes) which is the densest mix of
//! parsing + state-update we can throw at a terminal.
//!
//! The pattern is a deterministic LCG so every run is reproducible.
//!
//! Default: 300 frames at the detected grid size.

use std::io::Write;

use soltty_bench::Bench;

fn main() {
    let bench = Bench::start("truecolor_grid", 300);
    let cells = bench.cols as usize * bench.rows as usize;
    let mut out = bench.writer();

    // Hide cursor + home so we paint over the same region every frame.
    out.write_all(b"\x1b[?25l\x1b[1;1H").unwrap();
    let mut bytes = 6;

    let mut rng: u64 = 0xdead_beef_cafe_babe;
    let mut cell = [0u8; 32];
    for _ in 0..bench.iters {
        for _ in 0..cells {
            // xorshift64* — fast, deterministic, good enough for "random
            // bytes that aren't repetitive".
            rng ^= rng >> 12;
            rng ^= rng << 25;
            rng ^= rng >> 27;
            let v = rng.wrapping_mul(2685821657736338717);
            let r = (v & 0xff) as u8;
            let g = ((v >> 8) & 0xff) as u8;
            let b = ((v >> 16) & 0xff) as u8;
            let n = write_sgr_bg_space(&mut cell, r, g, b);
            out.write_all(&cell[..n]).unwrap();
            bytes += n as u64;
        }
        // Move home for the next frame instead of clearing — same as gol-c.
        out.write_all(b"\x1b[1;1H").unwrap();
        bytes += 6;
    }
    out.flush().unwrap();
    drop(out);

    bench.finish(bytes);
}

/// Encode `\x1b[48;2;R;G;Bm ` into `buf` without going through `format!`,
/// which would heap-allocate a String per cell. Returns bytes written.
fn write_sgr_bg_space(buf: &mut [u8; 32], r: u8, g: u8, b: u8) -> usize {
    let mut i = 0;
    let prefix = b"\x1b[48;2;";
    buf[i..i + prefix.len()].copy_from_slice(prefix);
    i += prefix.len();
    i += write_u8(&mut buf[i..], r);
    buf[i] = b';';
    i += 1;
    i += write_u8(&mut buf[i..], g);
    buf[i] = b';';
    i += 1;
    i += write_u8(&mut buf[i..], b);
    buf[i] = b'm';
    i += 1;
    buf[i] = b' ';
    i + 1
}

fn write_u8(buf: &mut [u8], v: u8) -> usize {
    if v >= 100 {
        buf[0] = b'0' + v / 100;
        buf[1] = b'0' + (v / 10) % 10;
        buf[2] = b'0' + v % 10;
        3
    } else if v >= 10 {
        buf[0] = b'0' + v / 10;
        buf[1] = b'0' + v % 10;
        2
    } else {
        buf[0] = b'0' + v;
        1
    }
}
