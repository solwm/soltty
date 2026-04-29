//! System clipboard with a backend chain so we work across compositors.
//!
//! `arboard` handles X11, macOS, and Windows fine. On Wayland it requires
//! the optional `wlr-data-control` / `ext-data-control` protocol, which
//! Sway/Hyprland/KDE implement but Mutter and many minimal compositors
//! don't. As a fallback we shell out to `wl-copy` from the `wl-clipboard`
//! package, which uses the standard `wl_data_device` protocol and works
//! on every Wayland compositor.

use std::io::Write;
use std::process::{Command, Stdio};

pub struct Clipboard {
    arboard: Option<arboard::Clipboard>,
    has_wl_copy: bool,
}

impl Clipboard {
    pub fn new() -> Self {
        let arboard = match arboard::Clipboard::new() {
            Ok(c) => Some(c),
            Err(e) => {
                log::debug!("arboard init failed: {e}");
                None
            }
        };
        let has_wl_copy =
            std::env::var_os("WAYLAND_DISPLAY").is_some() && which("wl-copy").is_some();
        if arboard.is_none() && !has_wl_copy {
            log::warn!("clipboard: no working backend (arboard failed and wl-copy is missing)");
        }
        Self {
            arboard,
            has_wl_copy,
        }
    }

    /// Best-effort clipboard write. Logs and continues on failure.
    pub fn set_text(&mut self, text: String) {
        // Prefer wl-copy on Wayland — works on every compositor regardless
        // of which (if any) data-control protocol is implemented.
        if self.has_wl_copy {
            if let Err(e) = pipe_to_wl_copy(&text, false) {
                log::warn!("wl-copy failed: {e}");
            } else {
                return;
            }
        }
        if let Some(cb) = self.arboard.as_mut() {
            if let Err(e) = cb.set_text(text) {
                log::warn!("arboard set_text: {e}");
            }
        }
    }

    /// Write to the *primary* selection (filled by mouse selection,
    /// pasted by middle-click). wl-paste/wl-copy only — arboard has no
    /// primary support on Linux.
    pub fn set_primary(&mut self, text: String) {
        if self.has_wl_copy {
            if let Err(e) = pipe_to_wl_copy(&text, true) {
                log::warn!("wl-copy --primary failed: {e}");
            }
        }
    }

    /// Read regular clipboard contents. Returns empty string on failure.
    pub fn get_text(&mut self) -> String {
        if self.has_wl_copy {
            if let Some(s) = read_from_wl_paste(false) {
                return s;
            }
        }
        if let Some(cb) = self.arboard.as_mut() {
            if let Ok(s) = cb.get_text() {
                return s;
            }
        }
        String::new()
    }

    /// Read the X11/Wayland *primary* selection — the one populated by
    /// mouse selection and pasted with middle-click. arboard doesn't
    /// expose primary on Linux, so this is wl-paste-only.
    pub fn get_primary(&mut self) -> String {
        if self.has_wl_copy {
            if let Some(s) = read_from_wl_paste(true) {
                return s;
            }
        }
        String::new()
    }
}

fn read_from_wl_paste(primary: bool) -> Option<String> {
    let mut cmd = Command::new("wl-paste");
    cmd.arg("--no-newline").stderr(Stdio::null());
    if primary {
        cmd.arg("--primary");
    }
    match cmd.output() {
        Ok(out) if out.status.success() => String::from_utf8(out.stdout).ok(),
        Ok(_) => None, // empty selection exits 1 — treat as "no data"
        Err(e) => {
            log::warn!("wl-paste spawn: {e}");
            None
        }
    }
}

fn pipe_to_wl_copy(text: &str, primary: bool) -> std::io::Result<()> {
    let mut cmd = Command::new("wl-copy");
    if primary {
        cmd.arg("--primary");
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    let status = child.wait()?;
    if !status.success() {
        return Err(std::io::Error::other(format!("wl-copy exited {status}")));
    }
    Ok(())
}

fn which(cmd: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(cmd);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
