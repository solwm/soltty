//! Stream plain ASCII as fast as the terminal will accept it. No SGR,
//! no cursor moves — exercises the parser's printable-character fast
//! path and the per-cell write_char + scroll loop.
//!
//! Default workload: 50,000 lines of a 60-byte filler. ~3 MB total.

use std::io::Write;

use soltty_bench::Bench;

const LINE: &[u8] = b"the quick brown fox jumps over the lazy dog 0123456789abcd\n";

fn main() {
    let bench = Bench::start("raw_throughput", 50_000);
    let mut out = bench.writer();

    let mut bytes = 0u64;
    for _ in 0..bench.iters {
        out.write_all(LINE).expect("write");
        bytes += LINE.len() as u64;
    }
    out.flush().expect("flush");
    drop(out);

    bench.finish(bytes);
}
