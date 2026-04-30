//! Vi mode state and keybind parsing.
//!
//! Vi mode is a soltty UI feature, not a terminal-protocol feature: it
//! intercepts keystrokes locally to drive a separate "vi cursor" the user
//! navigates with vim-style keys. The PTY sees nothing while we're in vi
//! mode (modulo Esc, which exits us back to the normal flow).
//!
//! M1 scope (this file): state + the activation keybind parser. The actual
//! key handling and rendering glue live in `main.rs` and `renderer.rs`.

use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::selection::Selection;

/// What kind of visual selection the user is in. `None` means vi-mode
/// normal (motion only); `Char` and `Line` correspond to vim's `v` and
/// `V`. Block (`Ctrl-v`) is a future milestone.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum VisualMode {
    None,
    Char,
    Line,
}

pub struct ViMode {
    pub active: bool,
    /// Vi cursor position, in viewport coords (row, col). Independent of
    /// the terminal cursor — moving the vi cursor doesn't move whatever
    /// the shell thinks its cursor is.
    pub cursor: (usize, usize),
    pub visual: VisualMode,
    /// Where visual selection started. Set by `v` / `V`, used to build
    /// the Selection that gets rendered.
    pub visual_anchor: Option<(usize, usize)>,
}

impl ViMode {
    pub fn new() -> Self {
        Self {
            active: false,
            cursor: (0, 0),
            visual: VisualMode::None,
            visual_anchor: None,
        }
    }

    /// Enter vi mode at the given starting position (typically the
    /// terminal cursor's current viewport coords, so the vi cursor
    /// shows up where the user expects).
    pub fn enter(&mut self, start: (usize, usize)) {
        self.active = true;
        self.cursor = start;
        self.visual = VisualMode::None;
        self.visual_anchor = None;
    }

    pub fn exit(&mut self) {
        self.active = false;
        self.visual = VisualMode::None;
        self.visual_anchor = None;
    }

    pub fn exit_visual(&mut self) {
        self.visual = VisualMode::None;
        self.visual_anchor = None;
    }

    pub fn start_visual_char(&mut self) {
        self.visual = VisualMode::Char;
        self.visual_anchor = Some(self.cursor);
    }

    pub fn start_visual_line(&mut self) {
        self.visual = VisualMode::Line;
        self.visual_anchor = Some(self.cursor);
    }

    /// Build the Selection that corresponds to the current visual state,
    /// or `None` if there's nothing to show. `cols` is the grid width;
    /// line-wise mode uses it to extend selections to the right edge.
    pub fn selection(&self, cols: usize) -> Option<Selection> {
        let anchor = self.visual_anchor?;
        match self.visual {
            VisualMode::None => None,
            VisualMode::Char => {
                let mut sel = Selection::new(anchor);
                sel.end = self.cursor;
                sel.dragging = false;
                Some(sel)
            }
            VisualMode::Line => {
                // Cover full rows from min(anchor, cursor) to max.
                let (sr, er) = if anchor.0 <= self.cursor.0 {
                    (anchor.0, self.cursor.0)
                } else {
                    (self.cursor.0, anchor.0)
                };
                let mut sel = Selection::new((sr, 0));
                sel.end = (er, cols.saturating_sub(1));
                sel.dragging = false;
                Some(sel)
            }
        }
    }
}

/// One keybind: a key plus a required modifier mask. Used for the vi-mode
/// activation shortcut (default `Ctrl+Shift+Space`, overridable via the
/// `SOLTTY_VI_KEY` env var).
#[derive(Clone, Debug)]
pub struct ViKeyBind {
    key: KeyMatch,
    ctrl: bool,
    shift: bool,
    alt: bool,
}

#[derive(Clone, Debug)]
enum KeyMatch {
    /// A printable character key. Compared case-insensitively against
    /// the `Key::Character` variant winit delivers.
    Char(char),
    /// A non-printable named key (Space, Tab, Enter, …).
    Named(NamedKey),
}

impl ViKeyBind {
    /// Default activation: `Ctrl+Shift+Space`.
    pub fn default_activation() -> Self {
        Self {
            key: KeyMatch::Named(NamedKey::Space),
            ctrl: true,
            shift: true,
            alt: false,
        }
    }

    /// Read the keybind from the `SOLTTY_VI_KEY` env var, falling back
    /// to the default if unset or unparseable. Logs a warning on bad
    /// input rather than failing — a bad env var shouldn't keep the
    /// terminal from starting.
    pub fn from_env_or_default() -> Self {
        match std::env::var("SOLTTY_VI_KEY") {
            Ok(s) => match parse_vi_key(&s) {
                Some(k) => {
                    log::info!("vi-mode keybind: {s:?}");
                    k
                }
                None => {
                    log::warn!(
                        "SOLTTY_VI_KEY={s:?} did not parse, using default ctrl+shift+space"
                    );
                    Self::default_activation()
                }
            },
            Err(_) => Self::default_activation(),
        }
    }

    /// Match a winit key event against this binding. Modifiers must be
    /// exactly equal (no extras allowed) so `Ctrl+Shift+Space` doesn't
    /// also fire on `Ctrl+Shift+Alt+Space`.
    pub fn matches(&self, logical: &Key, mods: ModifiersState) -> bool {
        if mods.control_key() != self.ctrl
            || mods.shift_key() != self.shift
            || mods.alt_key() != self.alt
        {
            return false;
        }
        match (&self.key, logical) {
            (KeyMatch::Char(c), Key::Character(s)) => {
                let ch = c.to_ascii_lowercase();
                s.chars().next().map(|x| x.to_ascii_lowercase()) == Some(ch)
            }
            (KeyMatch::Named(n), Key::Named(k)) => n == k,
            _ => false,
        }
    }
}

/// Parse a `+`-separated key spec like `"ctrl+shift+space"` or `"alt+v"`.
/// Returns `None` if the spec is malformed or contains an unknown token.
/// Tokens are case-insensitive and whitespace around them is trimmed.
fn parse_vi_key(s: &str) -> Option<ViKeyBind> {
    let mut ctrl = false;
    let mut shift = false;
    let mut alt = false;
    let mut key: Option<KeyMatch> = None;

    for raw in s.split('+') {
        let part = raw.trim().to_lowercase();
        match part.as_str() {
            "ctrl" | "control" => ctrl = true,
            "shift" => shift = true,
            "alt" | "meta" | "option" => alt = true,
            "space" => key = Some(KeyMatch::Named(NamedKey::Space)),
            "tab" => key = Some(KeyMatch::Named(NamedKey::Tab)),
            "enter" | "return" => key = Some(KeyMatch::Named(NamedKey::Enter)),
            "backspace" => key = Some(KeyMatch::Named(NamedKey::Backspace)),
            other if other.chars().count() == 1 => {
                let c = other.chars().next()?;
                if c.is_ascii_alphanumeric() {
                    key = Some(KeyMatch::Char(c));
                } else {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(ViKeyBind {
        key: key?,
        ctrl,
        shift,
        alt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::SmolStr;

    #[test]
    fn parse_default_form() {
        let k = parse_vi_key("ctrl+shift+space").unwrap();
        assert!(k.ctrl && k.shift && !k.alt);
        assert!(matches!(k.key, KeyMatch::Named(NamedKey::Space)));
    }

    #[test]
    fn parse_letter() {
        let k = parse_vi_key("alt+v").unwrap();
        assert!(!k.ctrl && !k.shift && k.alt);
        assert!(matches!(k.key, KeyMatch::Char('v')));
    }

    #[test]
    fn parse_case_insensitive() {
        let k = parse_vi_key("CTRL+Shift+SPACE").unwrap();
        assert!(k.ctrl && k.shift);
        assert!(matches!(k.key, KeyMatch::Named(NamedKey::Space)));
    }

    #[test]
    fn parse_rejects_unknown() {
        assert!(parse_vi_key("ctrl+!").is_none());
        assert!(parse_vi_key("hyper+a").is_none());
    }

    #[test]
    fn matches_exact_modifiers() {
        let k = ViKeyBind::default_activation();
        let space = Key::Named(NamedKey::Space);
        assert!(k.matches(&space, ModifiersState::CONTROL | ModifiersState::SHIFT));
        // Extra alt → reject.
        assert!(!k.matches(
            &space,
            ModifiersState::CONTROL | ModifiersState::SHIFT | ModifiersState::ALT
        ));
        // Missing shift → reject.
        assert!(!k.matches(&space, ModifiersState::CONTROL));
    }

    #[test]
    fn matches_letter_case_insensitive() {
        let k = parse_vi_key("ctrl+v").unwrap();
        let lower = Key::Character(SmolStr::new("v"));
        let upper = Key::Character(SmolStr::new("V"));
        assert!(k.matches(&lower, ModifiersState::CONTROL));
        assert!(k.matches(&upper, ModifiersState::CONTROL));
    }

    #[test]
    fn vi_mode_starts_inactive() {
        let v = ViMode::new();
        assert!(!v.active);
        assert_eq!(v.visual, VisualMode::None);
    }

    #[test]
    fn enter_seeds_cursor() {
        let mut v = ViMode::new();
        v.enter((3, 7));
        assert!(v.active);
        assert_eq!(v.cursor, (3, 7));
    }

    #[test]
    fn visual_char_selection_follows_cursor() {
        let mut v = ViMode::new();
        v.enter((2, 4));
        v.start_visual_char();
        v.cursor = (2, 9);
        let sel = v.selection(80).unwrap();
        assert_eq!(sel.anchor, (2, 4));
        assert_eq!(sel.end, (2, 9));
    }

    #[test]
    fn visual_line_covers_full_rows() {
        let mut v = ViMode::new();
        v.enter((2, 4));
        v.start_visual_line();
        v.cursor = (5, 1);
        let sel = v.selection(80).unwrap();
        assert_eq!(sel.anchor, (2, 0));
        assert_eq!(sel.end, (5, 79));
    }
}
