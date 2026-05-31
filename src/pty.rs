use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

use crate::term::Term;

/// Threshold (bytes accumulated in the current batch) above which the
/// worker switches from poll(timeout=0) to a short-blocking poll, so
/// one feed carries a render-frame's worth of producer output. Picked
/// above any plausible single-keystroke echo so interactive typing
/// never enters the slower-poll path.
const BURST_BATCH_BYTES: usize = 4 * 1024;

pub struct Pty {
    #[allow(dead_code)] // used by `resize`
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    /// Coalesced-wakeup flag. Worker sets to true after each batched
    /// read + feed; main clears it inside `user_event(PtyData)`. The
    /// worker only fires `on_data` when this transitions false→true,
    /// so back-to-back read/feed cycles between two main-thread
    /// frames collapse into a single winit wake.
    wakeup_pending: Arc<AtomicBool>,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl Pty {
    pub fn spawn(
        rows: u16,
        cols: u16,
        program: &str,
        args: &[String],
        // Parser state shared with main. Worker holds the lock during
        // feed; main holds it for `snapshot_term` and the brief
        // input-handler reads. The two are rarely contended thanks
        // to the snapshot pattern, which copies cells out of Term
        // and drops the guard before any GPU work runs.
        term: Arc<Mutex<Term>>,
        // Reply bytes the parser produced (DSR, OSC color queries) —
        // worker hands them to main here, main writes them through
        // `Pty::write` so all keystroke and reply traffic shares one
        // ordering on the writer.
        reply_tx: Sender<Vec<u8>>,
        // Burst-throttle counter the main thread reads to widen the
        // render interval. Worker stores `burst_holdoff_frames` when
        // a batch exceeds `burst_bytes_threshold`; main decays it by
        // 1 per render.
        burst_holdoff: Arc<AtomicU32>,
        burst_holdoff_frames: u32,
        burst_bytes_threshold: usize,
        on_data: impl Fn() + Send + 'static,
        on_exit: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io_other)?;

        let mut cmd = CommandBuilder::new(program);
        for a in args {
            cmd.arg(a);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        if let Ok(home) = std::env::var("HOME") {
            cmd.cwd(home);
        }

        let child = pair.slave.spawn_command(cmd).map_err(io_other)?;
        // Drop slave on the parent side so the child sees EOF when it exits.
        drop(pair.slave);

        // We read through the raw fd so we can use poll(timeout=0) to
        // batch contiguous reads into a single feed. The dyn-Read
        // clone is kept on the worker thread purely to own the dup'd
        // fd (drop closes it).
        let reader = pair.master.try_clone_reader().map_err(io_other)?;
        let raw_fd = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| io_other("master pty has no raw fd"))?;
        let writer = pair.master.take_writer().map_err(io_other)?;

        let wakeup_pending = Arc::new(AtomicBool::new(false));
        let wakeup_clone = wakeup_pending.clone();
        std::thread::Builder::new()
            .name("soltty-pty-reader".into())
            .spawn(move || {
                // Move-capture the cloned reader so its Drop runs at
                // thread exit; the raw_fd we read through stays valid
                // for the lifetime of this thread.
                let _reader_keepalive = reader;
                // 256 KB read buffer — bigger than the kernel's PTY pipe
                // so a single read() typically returns whatever was queued.
                let mut buf = [0u8; 262144];
                loop {
                    // First read of the batch: blocks until data is available.
                    let mut total = match unsafe {
                        libc::read(raw_fd, buf.as_mut_ptr() as *mut _, buf.len())
                    } {
                        0 => break,
                        n if n < 0 => {
                            let err = std::io::Error::last_os_error();
                            if err.kind() == std::io::ErrorKind::Interrupted {
                                continue;
                            }
                            break;
                        }
                        n => n as usize,
                    };
                    // Opportunistically extend the batch in two phases.
                    // Phase 1 (always): drain whatever's already queued
                    // with poll(timeout=0). Phase 2 (only past the
                    // burst threshold): wait up to a few ms for more —
                    // interactive single-byte echo never crosses the
                    // threshold so latency is unaffected.
                    const BURST_BATCH_MS: i32 = 2;
                    while total < buf.len() {
                        let timeout = if total >= BURST_BATCH_BYTES {
                            BURST_BATCH_MS
                        } else {
                            0
                        };
                        let mut pollfd = libc::pollfd {
                            fd: raw_fd,
                            events: libc::POLLIN,
                            revents: 0,
                        };
                        let r = unsafe { libc::poll(&mut pollfd, 1, timeout) };
                        if r <= 0 || (pollfd.revents & libc::POLLIN) == 0 {
                            break;
                        }
                        let n = unsafe {
                            libc::read(
                                raw_fd,
                                buf[total..].as_mut_ptr() as *mut _,
                                buf.len() - total,
                            )
                        };
                        if n <= 0 {
                            break;
                        }
                        total += n as usize;
                    }
                    if total >= burst_bytes_threshold {
                        burst_holdoff.store(burst_holdoff_frames, Ordering::Release);
                    }
                    // Feed under the shared lock. The lock is held only
                    // for the parse pass — main's snapshot takes the
                    // lock briefly (~150 µs for a 640×150 grid memcpy)
                    // then drops the guard before any GPU work, so
                    // worker contention is bounded by snapshot time,
                    // not by `swap_buffers`.
                    let reply = {
                        let mut term_guard = term.lock().unwrap();
                        term_guard.feed(&buf[..total]);
                        term_guard.take_reply()
                    };
                    if !reply.is_empty() {
                        // Drop on full / disconnected — main has gone
                        // away, the exit path is about to fire.
                        let _ = reply_tx.send(reply);
                    }
                    // Coalesced wakeup: only fire `on_data` if a
                    // previous wakeup hasn't been consumed yet. Under
                    // a sustained burst the atomic stays true across
                    // many feed cycles, collapsing N→1 winit
                    // dispatches per frame.
                    if !wakeup_clone.swap(true, Ordering::AcqRel) {
                        on_data();
                    }
                }
                drop(_reader_keepalive);
                // Reader saw EOF (or unrecoverable error) — child has closed
                // its end of the PTY. Tell the main thread to wind down.
                on_exit();
            })?;

        Ok(Self {
            master: pair.master,
            writer,
            wakeup_pending,
            _child: child,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if let Err(e) = self.writer.write_all(bytes) {
            log::warn!("pty write: {e}");
        }
    }

    /// Clear the worker→main wakeup flag at the start of
    /// `user_event(PtyData)`. Any worker batch that lands between
    /// here and the end of the handler re-signals naturally on the
    /// next swap of the atomic.
    pub fn ack_wakeup(&self) {
        self.wakeup_pending.store(false, Ordering::Release);
    }

    #[allow(dead_code)] // wired up in milestone 5
    pub fn resize(&self, rows: u16, cols: u16) {
        if let Err(e) = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            log::warn!("pty resize: {e}");
        }
    }
}

pub fn default_shell() -> String {
    if cfg!(windows) {
        std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".into())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    }
}

fn io_other<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
}
