use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

use crate::term::Term;

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
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

/// Feed `bytes` into the shared terminal and hand back any reply bytes,
/// without ever poisoning the mutex.
///
/// `Term::feed` runs the (attacker-controlled) escape-sequence parser. If
/// it ever panics, a plain `lock().unwrap()` would poison the mutex, and
/// the main thread would then panic on its very next `lock().unwrap()` —
/// turning one parser bug into a whole-app crash with a confusing
/// secondary panic. We contain the panic here (caught before the guard
/// drops, so the lock is never marked poisoned) and recover from an
/// already-poisoned guard for good measure.
fn feed_locked(term: &Mutex<Term>, bytes: &[u8]) -> Vec<u8> {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    let mut guard = term.lock().unwrap_or_else(|p| p.into_inner());
    catch_unwind(AssertUnwindSafe(|| {
        guard.feed(bytes);
        guard.take_reply()
    }))
    .unwrap_or_else(|_| {
        log::error!("terminal parser panicked on input; dropping batch");
        Vec::new()
    })
}

impl Pty {
    #[allow(clippy::too_many_arguments)] // spawn config; grouping into a struct buys nothing
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

        // Drop the slave-side OPOST processing on the output direction
        // (the bytes the child writes to its stdout, which we read on
        // the master). OPOST runs the ldisc's per-byte output post-
        // processing for things like ONLCR (\n → \r\n), which we
        // don't want — we have our own VTE parser, and the per-byte
        // walk is real overhead under chatty output (gol-c, log
        // dumps).
        //
        // We deliberately do NOT call cfmakeraw, which would also
        // clear ICANON/ISIG/ECHO on the slave's input side and break
        // job control (Ctrl+C / Ctrl+Z), line editing, and shell
        // completion (zsh's readline / fzf-style tab completion both
        // expect ISIG and ICANON managed by the shell). Leaving the
        // input-side flags alone lets the shell set its own termios.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(raw_fd, &mut t) == 0 {
                t.c_oflag &= !libc::OPOST;
                let _ = libc::tcsetattr(raw_fd, libc::TCSANOW, &t);
            }
        }
        // Non-blocking master fd so the inner-batch loop can `read`
        // directly and use EAGAIN as the "kernel pipe is empty, send
        // what we have" signal — one syscall per inner iteration
        // instead of poll+read.
        unsafe {
            let flags = libc::fcntl(raw_fd, libc::F_GETFL);
            if flags >= 0 {
                let _ = libc::fcntl(raw_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }

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
                'outer: loop {
                    let mut total = 0usize;
                    // Non-blocking read loop: drain whatever the
                    // kernel has buffered into one batch. EAGAIN
                    // means the pipe is empty for now.
                    loop {
                        if total >= buf.len() {
                            break;
                        }
                        let n = unsafe {
                            libc::read(
                                raw_fd,
                                buf[total..].as_mut_ptr() as *mut _,
                                buf.len() - total,
                            )
                        };
                        if n > 0 {
                            total += n as usize;
                            continue;
                        }
                        if n == 0 {
                            // EOF — child has closed its end.
                            if total > 0 {
                                break;
                            }
                            break 'outer;
                        }
                        let err = std::io::Error::last_os_error();
                        let kind = err.kind();
                        if kind == std::io::ErrorKind::Interrupted {
                            continue;
                        }
                        if kind == std::io::ErrorKind::WouldBlock {
                            if total > 0 {
                                break;
                            }
                            // Nothing buffered — wait for data.
                            let mut pollfd = libc::pollfd {
                                fd: raw_fd,
                                events: libc::POLLIN,
                                revents: 0,
                            };
                            let _ = unsafe { libc::poll(&mut pollfd, 1, -1) };
                            continue;
                        }
                        // Other errors — bail.
                        break 'outer;
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
                    let reply = feed_locked(&term, &buf[..total]);
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
            child,
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

    /// The child's process id, while it's still tracked. Test-only.
    #[cfg(test)]
    fn child_pid(&self) -> Option<u32> {
        self.child.process_id()
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // Closing the window must take the shell — and any job-control
        // children in its process group — down with it. Otherwise they
        // orphan: the reader thread holds a dup'd master fd, so dropping
        // our `master` doesn't close the last PTY handle and the kernel
        // never delivers the usual SIGHUP. portable_pty starts the child
        // in its own session, so its pid is also its process-group id;
        // signal the whole group (SIGHUP for a clean exit, then SIGKILL
        // to be sure), then reap the leader so we don't leave a zombie.
        if let Some(pid) = self.child.process_id() {
            let pgid = pid as i32;
            unsafe {
                libc::kill(-pgid, libc::SIGHUP);
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
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
    std::io::Error::other(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    #[test]
    fn feed_locked_recovers_from_poisoned_mutex() {
        // A previous panic-while-holding poisons the mutex. `feed_locked`
        // must still feed (recovering the inner Term) rather than
        // propagating the poison — otherwise one parser panic cascades
        // into a crash on the next lock.
        let term = Arc::new(Mutex::new(Term::new(4, 10)));
        let poisoner = term.clone();
        let _ = std::thread::spawn(move || {
            let _g = poisoner.lock().unwrap();
            panic!("poison the lock");
        })
        .join();
        assert!(term.is_poisoned());

        let reply = feed_locked(&term, b"hi");
        assert!(reply.is_empty()); // plain text produces no reply bytes
        let g = term.lock().unwrap_or_else(|p| p.into_inner());
        assert_eq!(g.grid().lines[0].cells[0].ch, 'h');
        assert_eq!(g.grid().lines[0].cells[1].ch, 'i');
    }

    #[test]
    fn child_is_killed_on_drop() {
        // Regression: closing the window (dropping the Pty) must not leave
        // an orphaned shell/child running. Spawn a long sleep, confirm
        // it's alive, drop the Pty, and confirm the leader is gone.
        let term = Arc::new(Mutex::new(Term::new(24, 80)));
        let (tx, _rx) = channel();
        let burst = Arc::new(AtomicU32::new(0));
        let pty = Pty::spawn(
            24,
            80,
            "/bin/sh",
            &["-c".into(), "sleep 30".into()],
            term,
            tx,
            burst,
            0,
            usize::MAX,
            || {},
            || {},
        )
        .expect("spawn pty");

        let pid = pty.child_pid().expect("child pid") as i32;
        // Alive: signal 0 just checks existence/permission.
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "child should be alive");

        drop(pty);

        // Drop kills + reaps synchronously, but the group SIGKILL may take
        // a beat to settle. Poll briefly for the leader to disappear.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            if !alive {
                break;
            }
            assert!(Instant::now() < deadline, "child survived Pty drop");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
