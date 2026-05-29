use std::io::{Read, Write};
use std::sync::mpsc::{sync_channel, Receiver, TryRecvError};

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
        std::thread::Builder::new()
            .name("soltty-pty-reader".into())
            .spawn(move || {
                // 64 KB read buffer — matches Linux's default PTY pipe
                // buffer size, so a chatty producer that fills the kernel
                // pipe gets drained in a single syscall instead of 8.
                // Was 8 KB; profiling under gol-c (~460 fps, ~465 KB per
                // frame) showed the reader hitting ~27 k read() calls per
                // second. With 64 KB we're down to ~3.5 k.
                let mut buf = [0u8; 65536];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                            on_data();
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
            _child: child,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if let Err(e) = self.writer.write_all(bytes) {
            log::warn!("pty write: {e}");
        }
    }

    pub fn drain(&self) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            match self.output_rx.try_recv() {
                Ok(chunk) => out.extend_from_slice(&chunk),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        out
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
