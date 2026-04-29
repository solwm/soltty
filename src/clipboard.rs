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
        let has_wl_copy = std::env::var_os("WAYLAND_DISPLAY").is_some()
            && which("wl-copy").is_some();
        if arboard.is_none() && !has_wl_copy {
            log::warn!(
                "clipboard: no working backend (arboard failed and wl-copy is missing)"
            );
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
            if let Err(e) = pipe_to_wl_copy(&text) {
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
}

fn pipe_to_wl_copy(text: &str) -> std::io::Result<()> {
    let mut child = Command::new("wl-copy")
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
