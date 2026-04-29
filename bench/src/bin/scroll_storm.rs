//! Print many short lines back-to-back. Each `\n` past the bottom of
//! the live grid pushes a row into scrollback, so this exercises the
//! VecDeque eviction path and the per-row blank-fill in `scroll_up`.
//!
//! Default: 200,000 lines (~3 MB total). At 10,000-line scrollback
//! that's ~20x ring-buffer turnover.

use std::io::Write;

use soltty_bench::Bench;

fn main() {
    let bench = Bench::start("scroll_storm", 200_000);
    let mut out = bench.writer();

    let mut bytes = 0u64;
    let mut buf = [0u8; 16];
    for i in 0..bench.iters {
        // Short numeric-ish payload so each line is small and the
        // scroll path dominates over the print path.
        let n = write_line(&mut buf, i);
        out.write_all(&buf[..n]).unwrap();
        bytes += n as u64;
    }
    out.flush().unwrap();
    drop(out);

    bench.finish(bytes);
}

fn write_line(buf: &mut [u8; 16], n: u64) -> usize {
    let mut i = 0;
    let prefix = b"line ";
    buf[..prefix.len()].copy_from_slice(prefix);
    i += prefix.len();
    let written = u64_to_decimal(&mut buf[i..], n);
    i += written;
    buf[i] = b'\n';
    i + 1
}

fn u64_to_decimal(buf: &mut [u8], mut n: u64) -> usize {
    if n == 0 {
        buf[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 20];
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
