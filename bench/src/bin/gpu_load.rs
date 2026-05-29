//! Steady-state GPU-load bench. Fixed wallclock window (default 5 s,
//! `BENCH_DURATION_MS` override) instead of an iteration count, so
//! nvidia-smi sampling at -lms 100 has time to gather a representative
//! average past the ~1 s warmup the runner discards.
//!
//! Workload pattern: hide cursor, home, then repeatedly fill the grid
//! via natural wrap with rotating printable ASCII bytes. Each tick
//! emits one 256-color SGR (to keep fg colors visually rotating, so
//! per-cell `CellInstance.fg` differs across ticks and the renderer
//! can't elide work based on identical bytes) plus `rows × cols`
//! printable bytes. That hits the parser's printable-ASCII fast path
//! (`src/term.rs::feed`) — the cheapest parser cost per cell change
//! we have — and forces a full visible rewrite of the grid every tick.
//!
//! The point isn't bytes-per-second; it's "how busy can the GPU be
//! while the rendered surface keeps changing." `truecolor_grid`
//! already covers per-cell SGR parsing cost; this is its GPU-bound
//! sibling.

use std::io::Write;
use std::time::{Duration, Instant};

use soltty_bench::Bench;

const PATTERN_LEN: u64 = 95; // printable ASCII: 0x20..=0x7E inclusive.

fn main() {
    // The iters argument is unused — finish_after() supplies the tick
    // count. We pass 0 so the field is harmlessly initialized.
    let bench = Bench::start("gpu_load", 0);
    let cols = bench.cols as usize;
    let rows = bench.rows as usize;

    let duration_ms = std::env::var("BENCH_DURATION_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(5_000);
    let deadline = Instant::now() + Duration::from_millis(duration_ms);

    let mut out = bench.writer();

    // Hide cursor + home; the wallclock budget starts now so any
    // setup-write cost is on us, not the terminal.
    out.write_all(b"\x1b[?25l\x1b[H").expect("setup write");
    let mut bytes: u64 = b"\x1b[?25l\x1b[H".len() as u64;

    // Pre-allocate the per-row scratch so the hot loop has no allocs.
    let mut row_buf = vec![0u8; cols];

    let mut tick: u64 = 0;
    let mut sgr_buf = [0u8; 16];

    while Instant::now() < deadline {
        // Rotate a 256-color SGR fg per tick. 16..=231 is the 6×6×6
        // cube, so we get visually-distinct ticks without truecolor
        // parsing cost (truecolor would be 3 extra params per SGR).
        let color = 16 + (tick % 216) as u8;
        let sgr_len = write_sgr_fg256(&mut sgr_buf, color);
        out.write_all(&sgr_buf[..sgr_len]).expect("sgr write");
        bytes += sgr_len as u64;

        // Home, then fill via natural wrap. The renderer cares about
        // cell-level updates; we never need explicit CUP between rows.
        out.write_all(b"\x1b[H").expect("home");
        bytes += 3;

        for r in 0..rows {
            let base = tick.wrapping_add(r as u64 * 7);
            for c in 0..cols {
                let n = base.wrapping_add(c as u64) % PATTERN_LEN;
                row_buf[c] = b' ' + (n as u8);
            }
            out.write_all(&row_buf).expect("row write");
            bytes += cols as u64;
        }
        // Flush every tick: keeps PTY drain timing close to the
        // logical-frame boundary so the burst throttle in the
        // terminal sees coherent chunks instead of one giant pipe.
        out.flush().expect("flush");
        tick += 1;
    }

    drop(out);
    bench.finish_after(tick, bytes);
}

/// Encode `\x1b[38;5;Nm` into `buf`. Avoids the heap-alloc cost of
/// `format!` inside the per-tick hot loop, matching the pattern from
/// `truecolor_grid`'s `write_sgr_bg_space`.
fn write_sgr_fg256(buf: &mut [u8; 16], n: u8) -> usize {
    let prefix = b"\x1b[38;5;";
    buf[..prefix.len()].copy_from_slice(prefix);
    let mut i = prefix.len();
    if n >= 100 {
        buf[i] = b'0' + n / 100;
        buf[i + 1] = b'0' + (n / 10) % 10;
        buf[i + 2] = b'0' + n % 10;
        i += 3;
    } else if n >= 10 {
        buf[i] = b'0' + n / 10;
        buf[i + 1] = b'0' + n % 10;
        i += 2;
    } else {
        buf[i] = b'0' + n;
        i += 1;
    }
    buf[i] = b'm';
    i + 1
}
