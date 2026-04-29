//! Spam CUP sequences (`\x1b[<row>;<col>H`) with no content. Isolates
//! pure CSI parser cost from print/scroll/atlas work — every byte is
//! escape-machinery, nothing lands on screen.
//!
//! Default: 500,000 jumps. ~5 MB of pure CSI.

use std::io::Write;

use soltty_bench::Bench;

fn main() {
    let bench = Bench::start("cursor_jumps", 500_000);
    let mut out = bench.writer();

    // Hide the cursor so the user doesn't see a strobe-light effect
    // even at one redraw per 16ms.
    out.write_all(b"\x1b[?25l").unwrap();
    let mut bytes = 5;

    let cols = bench.cols.max(1);
    let rows = bench.rows.max(1);
    let mut buf = [0u8; 16];
    let mut rng: u32 = 0x9e37_79b9;
    for _ in 0..bench.iters {
        rng = rng.wrapping_mul(1103515245).wrapping_add(12345);
        let row = ((rng >> 16) as u16 % rows) + 1;
        let col = (rng as u16 % cols) + 1;
        let n = write_cup(&mut buf, row, col);
        out.write_all(&buf[..n]).unwrap();
        bytes += n as u64;
    }
    out.write_all(b"\x1b[?25h").unwrap();
    bytes += 5;
    out.flush().unwrap();
    drop(out);

    bench.finish(bytes);
}

fn write_cup(buf: &mut [u8; 16], row: u16, col: u16) -> usize {
    let mut i = 0;
    buf[i] = 0x1b;
    i += 1;
    buf[i] = b'[';
    i += 1;
    i += u16_to_decimal(&mut buf[i..], row);
    buf[i] = b';';
    i += 1;
    i += u16_to_decimal(&mut buf[i..], col);
    buf[i] = b'H';
    i + 1
}

fn u16_to_decimal(buf: &mut [u8], mut n: u16) -> usize {
    if n == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 6];
    let mut len = 0;
    while n > 0 {
        tmp[len] = b'0' + (n % 10) as u8;
        n /= 10;
        len += 1;
    }
    for i in 0..len {
        buf[i] = tmp[len - 1 - i];
    }
    len
}
