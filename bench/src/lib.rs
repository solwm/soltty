//! Shared scaffolding for the bench programs.
//!
//! Each bench reads its config from env vars so the runner can launch
//! identical workloads under different terminals:
//!
//!   BENCH_ITERS=N        Override the bench's default iteration count.
//!   BENCH_OUT=path       Where to write the result line. Defaults to
//!                        /tmp/soltty_bench.txt. Output goes to a file
//!                        because stdout points at the PTY.
//!   BENCH_COLS, BENCH_ROWS
//!                        Override the detected terminal size. Useful
//!                        for forcing identical workloads when two
//!                        terminals open at different default sizes.
//!
//! The result line format is:
//!
//!   <name> <secs> <iters> <cols> <rows> <bytes>
//!
//! one whitespace-separated record. The runner script slices that.

use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::time::Instant;

pub struct Bench {
    pub name: &'static str,
    pub iters: u64,
    pub cols: u16,
    pub rows: u16,
    out_path: String,
    started: Instant,
}

impl Bench {
    pub fn start(name: &'static str, default_iters: u64) -> Self {
        let iters = std::env::var("BENCH_ITERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default_iters);
        let out_path =
            std::env::var("BENCH_OUT").unwrap_or_else(|_| "/tmp/soltty_bench.txt".into());
        let (cols, rows) = detect_size();
        Self {
            name,
            iters,
            cols,
            rows,
            out_path,
            started: Instant::now(),
        }
    }

    /// Lock stdout and wrap it in a 64 KB buffered writer. The kernel
    /// PTY pipe is ~16 KB on macOS / ~64 KB on Linux; a 64 KB userspace
    /// buffer means we make at most one syscall per pipe-full and the
    /// terminal's read rate is what backpressures us.
    pub fn writer(&self) -> std::io::BufWriter<std::io::StdoutLock<'static>> {
        std::io::BufWriter::with_capacity(64 * 1024, std::io::stdout().lock())
    }

    /// Wait for the terminal to drain before stopping the clock, then
    /// write the result line. The drain works via DSR (Device Status
    /// Report): we send `\x1b[6n`, which the terminal answers with
    /// `\x1b[<r>;<c>R` only after it has processed every preceding
    /// byte. Without this, small workloads finish before the kernel
    /// pipe even backpressures, and we just measure how fast we can
    /// `write(2)` — not how fast the terminal actually consumes.
    pub fn finish(&self, bytes: u64) {
        self.emit(self.iters, bytes);
    }

    /// Like `finish`, but for wallclock-bounded benches: the caller
    /// counts how many ticks it executed inside its time budget, and
    /// that count lands in the result file's iters column. Used by
    /// `gpu_load`, which can't be iteration-bounded because nvidia-smi
    /// sampling needs a known minimum duration to gather a
    /// statistically meaningful average.
    pub fn finish_after(&self, ticks: u64, bytes: u64) {
        self.emit(ticks, bytes);
    }

    fn emit(&self, iters: u64, bytes: u64) {
        let drain = ping_terminal(&mut std::io::stdout().lock());
        let secs = self.started.elapsed().as_secs_f64();
        if let Err(e) = drain {
            eprintln!("bench: DSR ping failed: {e} — wall time may be too low");
        }
        let line = format!(
            "{} {:.6} {} {} {} {}\n",
            self.name, secs, iters, self.cols, self.rows, bytes
        );
        if let Err(e) = std::fs::write(&self.out_path, &line) {
            eprintln!("bench: write {}: {e}", self.out_path);
        }
        // Restore the terminal so the host shell isn't left with a
        // hidden cursor or active SGR.
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(b"\x1b[?25h\x1b[0m\n");
        let _ = out.flush();
    }
}

/// Round-trip a Device Status Report. Returns once we've read the `R`
/// terminator from stdin. Puts stdin into raw mode for the duration so
/// the response bytes don't get cooked away by the line discipline.
fn ping_terminal<W: Write>(out: &mut W) -> std::io::Result<()> {
    let stdin_fd = std::io::stdin().as_raw_fd();
    let mut orig: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(stdin_fd, &mut orig) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut raw = orig;
    unsafe {
        libc::cfmakeraw(&mut raw);
    }
    if unsafe { libc::tcsetattr(stdin_fd, libc::TCSANOW, &raw) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    let result = (|| -> std::io::Result<()> {
        out.write_all(b"\x1b[6n")?;
        out.flush()?;
        let mut stdin = std::io::stdin();
        let mut byte = [0u8; 1];
        // The response is `ESC [ <row> ; <col> R`. We don't care about
        // the values — just that the `R` terminator arrived.
        loop {
            stdin.read_exact(&mut byte)?;
            if byte[0] == b'R' {
                break;
            }
        }
        Ok(())
    })();

    unsafe {
        libc::tcsetattr(stdin_fd, libc::TCSANOW, &orig);
    }
    result
}

/// Best-effort terminal size. Honors BENCH_COLS / BENCH_ROWS env
/// overrides, falls back to TIOCGWINSZ via the `terminal_size` crate,
/// then to a sane 80x24 default if neither yields a sensible value.
fn detect_size() -> (u16, u16) {
    let env_cols = std::env::var("BENCH_COLS")
        .ok()
        .and_then(|s| s.parse().ok());
    let env_rows = std::env::var("BENCH_ROWS")
        .ok()
        .and_then(|s| s.parse().ok());
    if let (Some(c), Some(r)) = (env_cols, env_rows) {
        return (c, r);
    }
    if let Some((terminal_size::Width(w), terminal_size::Height(h))) =
        terminal_size::terminal_size()
    {
        return (env_cols.unwrap_or(w), env_rows.unwrap_or(h));
    }
    (env_cols.unwrap_or(80), env_rows.unwrap_or(24))
}
