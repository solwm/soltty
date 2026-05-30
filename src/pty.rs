use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, TryRecvError};
use std::sync::Arc;

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

/// Bound on how many 8 KB chunks the reader thread can buffer before blocking.
/// 64 chunks = 512 KB in flight. When full, the reader's `tx.send` blocks,
/// the kernel PTY buffer fills, and a chatty child (e.g. a busy-loop printf)
/// is forced to slow down to match our drain rate. Without this, the channel
/// grows unboundedly, the main thread can't keep up, and keyboard events
/// (including Ctrl+C) queue behind a wall of PTY data.
const PTY_CHANNEL_CAP: usize = 64;

pub struct Pty {
    #[allow(dead_code)] // used by `resize`, which is wired up in milestone 5
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    output_rx: Receiver<Vec<u8>>,
    /// Reusable scratch buffer for `drain` so we don't allocate a fresh
    /// `Vec<u8>` per call. Under gol-c bench we drain ~1000+ times/sec;
    /// the per-call malloc/free shows up in profiles.
    drain_buf: Vec<u8>,
    /// Coalesced-wakeup flag. Reader sets to true after a read; main
    /// clears it at the start of every drain. `on_data` fires winit's
    /// user event only when the flag was previously false — i.e., when
    /// the main thread might be idle. Saves redundant winit dispatches
    /// during chatty bursts where multiple reads queue between drains.
    wakeup_pending: Arc<AtomicBool>,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl Pty {
    pub fn spawn(
        rows: u16,
        cols: u16,
        program: &str,
        args: &[String],
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

        let mut reader = pair.master.try_clone_reader().map_err(io_other)?;
        let writer = pair.master.take_writer().map_err(io_other)?;

        let (tx, rx) = sync_channel::<Vec<u8>>(PTY_CHANNEL_CAP);
        let wakeup_pending = Arc::new(AtomicBool::new(false));
        let wakeup_clone = wakeup_pending.clone();
        std::thread::Builder::new()
            .name("soltty-pty-reader".into())
            .spawn(move || {
                // 256 KB read buffer — bigger than the kernel's PTY pipe
                // so a single read() returns whatever the kernel had
                // queued in one shot.
                let mut buf = [0u8; 262144];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                            // Coalesced wakeup: only fire `on_data` if a
                            // previous wakeup hasn't already been signaled
                            // but not yet consumed by the main thread.
                            // Under heavy bursts (gol-c writes ~7 chunks
                            // per frame) this collapses 7 winit dispatches
                            // into 1.
                            if !wakeup_clone.swap(true, Ordering::AcqRel) {
                                on_data();
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                // Reader saw EOF (or unrecoverable error) — child has closed
                // its end of the PTY. Tell the main thread to wind down.
                on_exit();
            })?;

        Ok(Self {
            master: pair.master,
            writer,
            output_rx: rx,
            drain_buf: Vec::with_capacity(256 * 1024),
            wakeup_pending,
            _child: child,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if let Err(e) = self.writer.write_all(bytes) {
            log::warn!("pty write: {e}");
        }
    }

    /// Drain everything currently pending on the channel. Returns a
    /// reusable slice borrowed from `self.drain_buf` — caller must
    /// copy/parse before the next call to `drain` (which truncates the
    /// buffer back to empty).
    pub fn drain(&mut self) -> &[u8] {
        // Reset the coalesce flag *first*. Any read that lands between
        // here and the next drain will set it again and re-wake us.
        // Resetting last would race: a read that landed mid-drain could
        // set the flag, then we'd clear it, and a subsequent read
        // wouldn't re-wake because the flag was already true. Reset
        // first → at most one missed wake per drain, and the channel
        // try_recv loop below catches anything that arrived since.
        self.wakeup_pending.store(false, Ordering::Release);
        self.drain_buf.clear();
        loop {
            match self.output_rx.try_recv() {
                Ok(chunk) => self.drain_buf.extend_from_slice(&chunk),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        &self.drain_buf
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
