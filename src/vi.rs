//! Vi mode state, motions, and keybind parsing.
//!
//! Vi mode is a soltty UI feature, not a terminal-protocol feature: it
//! intercepts keystrokes locally to drive a separate "vi cursor" the user
//! navigates with vim-style keys. The PTY sees nothing while we're in vi
//! mode (modulo Esc, which exits us back to the normal flow).
//!
//! Key handling lives in `main.rs::window_event`; this module owns the
//! state machine (cursor, visual mode, count prefix, op-pending) and the
//! motion implementations that mutate `ViMode` + scroll the `Term`.

use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::selection::Selection;
use crate::term::Term;

/// What kind of visual selection the user is in. `None` means vi-mode
/// normal (motion only); `Char` and `Line` correspond to vim's `v` and
/// `V`. Block (`Ctrl-v`) is a future milestone.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum VisualMode {
    None,
    Char,
    Line,
}

/// Two-key sequences in vim wait for the second key — `gg` is the only
/// one we use today, so this is a single-variant enum we'll grow later.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PendingOp {
    Goto, // saw 'g', waiting for the second 'g' to mean DocStart
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
    /// Accumulated count prefix (e.g. `5j` parses 5 here before applying
    /// motion j five times). `None` means no count typed; `Some(0)` is
    /// only reachable when `0` is itself a count digit (i.e. typed after
    /// `1`-`9`), since a leading `0` means "line start" motion instead.
    pub pending_count: Option<u32>,
    /// Set when we're mid-multi-key sequence (e.g. saw `g`, waiting for
    /// the next key to complete `gg`). Cleared once the sequence
    /// completes or is cancelled by Esc / a non-matching follow-up.
    pub pending_op: Option<PendingOp>,
}

impl ViMode {
    pub fn new() -> Self {
        Self {
            active: false,
            cursor: (0, 0),
            visual: VisualMode::None,
            visual_anchor: None,
            pending_count: None,
            pending_op: None,
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
        self.pending_count = None;
        self.pending_op = None;
    }

    pub fn exit(&mut self) {
        self.active = false;
        self.visual = VisualMode::None;
        self.visual_anchor = None;
        self.pending_count = None;
        self.pending_op = None;
    }

    pub fn exit_visual(&mut self) {
        self.visual = VisualMode::None;
        self.visual_anchor = None;
    }

    /// Clear half-typed count and op-pending state. Esc calls this before
    /// deciding whether to also exit visual or vi-mode.
    pub fn cancel_pending(&mut self) {
        self.pending_count = None;
        self.pending_op = None;
    }

    /// Consume the typed count and reset. Returns 1 when no count was
    /// pending, matching vim's "implicit ×1" for an unprefixed motion.
    pub fn take_count(&mut self) -> u32 {
        self.pending_count.take().unwrap_or(1).max(1)
    }

    /// Append a digit (0..=9) to the pending count. Saturates at 10_000
    /// to keep one stuck key from spinning forever.
    pub fn push_digit(&mut self, d: u32) {
        let cur = self.pending_count.unwrap_or(0);
        self.pending_count = Some((cur.saturating_mul(10).saturating_add(d)).min(10_000));
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

/// A vi-mode motion. Counts (`5j`, `3w`) are applied by re-running the
/// motion N times in `apply_motion`; idempotent ones (LineStart) repeat
/// trivially, others (Char, WordNext) accumulate the way vim users
/// expect.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Motion {
    /// (drow, dcol) per repeat. Used for h/j/k/l. Scrolls into history /
    /// toward live when a step would push past the viewport edge.
    Char(isize, isize),
    LineStart,         // 0
    LineFirstNonBlank, // ^
    LineEnd,           // $
    DocStart,          // gg — top of scrollback
    DocEnd,            // G  — bottom of live grid
    ScreenTop,         // H
    ScreenMiddle,      // M
    ScreenBottom,      // L
    HalfPageUp,        // Ctrl-u
    HalfPageDown,      // Ctrl-d
    FullPageUp,        // Ctrl-b
    FullPageDown,      // Ctrl-f
    /// `w`/`W`. `big` toggles WORD (non-whitespace runs) vs word
    /// (alphanumeric + `_` runs).
    WordNext { big: bool },
    /// `e`/`E` — end of current word, or end of next word if already at
    /// the end.
    WordEnd { big: bool },
    /// `b`/`B`.
    WordPrev { big: bool },
}

/// Apply a motion `count` times, scrolling the viewport when motions push
/// past the edge. Free function rather than a `ViMode` method so the
/// caller in `App::window_event` can pass disjoint borrows of `vi` and
/// `term` while still holding the unrelated `&mut self.gpu` from the
/// destructure at the top of that fn.
pub fn apply_motion(vi: &mut ViMode, term: &mut Term, motion: Motion, count: u32) {
    for _ in 0..count {
        let (rows, cols) = {
            let g = term.grid();
            (g.rows, g.cols)
        };
        match motion {
            Motion::Char(drow, dcol) => char_move(vi, term, drow, dcol, rows, cols),
            Motion::LineStart => vi.cursor.1 = 0,
            Motion::LineFirstNonBlank => {
                let row = vi.cursor.0;
                vi.cursor.1 = first_non_blank(term, row, cols);
            }
            Motion::LineEnd => {
                let row = vi.cursor.0;
                vi.cursor.1 = last_non_blank(term, row, cols);
            }
            Motion::DocStart => {
                term.scroll_view(isize::MAX);
                vi.cursor.0 = 0;
            }
            Motion::DocEnd => {
                term.reset_view();
                vi.cursor.0 = rows.saturating_sub(1);
            }
            Motion::ScreenTop => vi.cursor.0 = 0,
            Motion::ScreenMiddle => vi.cursor.0 = rows / 2,
            Motion::ScreenBottom => vi.cursor.0 = rows.saturating_sub(1),
            Motion::HalfPageUp => term.scroll_view((rows / 2) as isize),
            Motion::HalfPageDown => term.scroll_view(-((rows / 2) as isize)),
            Motion::FullPageUp => term.scroll_view(rows.saturating_sub(1) as isize),
            Motion::FullPageDown => term.scroll_view(-(rows.saturating_sub(1) as isize)),
            Motion::WordNext { big } => word_next(vi, term, big, rows, cols),
            Motion::WordEnd { big } => word_end(vi, term, big, rows, cols),
            Motion::WordPrev { big } => word_prev(vi, term, big, cols),
        }
        // Final clamp — most arms set one coord and trust the other to
        // already be in range, but motion logic that strays outside (e.g.
        // a row count larger than the viewport) gets caught here.
        vi.cursor.0 = vi.cursor.0.min(rows.saturating_sub(1));
        vi.cursor.1 = vi.cursor.1.min(cols.saturating_sub(1));
    }
}

fn char_move(
    vi: &mut ViMode,
    term: &mut Term,
    drow: isize,
    dcol: isize,
    rows: usize,
    cols: usize,
) {
    let r = vi.cursor.0 as isize + drow;
    let c =
        (vi.cursor.1 as isize + dcol).clamp(0, cols.saturating_sub(1) as isize) as usize;
    if r < 0 {
        // Scroll into history; pin cursor to the new top row.
        term.scroll_view(-r);
        vi.cursor = (0, c);
    } else if r >= rows as isize {
        let overshoot = r - (rows - 1) as isize;
        term.scroll_view(-overshoot);
        vi.cursor = (rows - 1, c);
    } else {
        vi.cursor = (r as usize, c);
    }
}

fn first_non_blank(term: &Term, row: usize, cols: usize) -> usize {
    term.viewport_row(row)
        .cells
        .iter()
        .take(cols)
        .position(|c| !c.ch.is_whitespace() && c.ch != '\0')
        .unwrap_or(0)
}

fn last_non_blank(term: &Term, row: usize, cols: usize) -> usize {
    term.viewport_row(row)
        .cells
        .iter()
        .take(cols)
        .rposition(|c| !c.ch.is_whitespace() && c.ch != '\0')
        .unwrap_or(0)
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum WordClass {
    Whitespace,
    Word,
    Punct,
}

fn classify(ch: char, big: bool) -> WordClass {
    if ch.is_whitespace() || ch == '\0' {
        WordClass::Whitespace
    } else if big || ch.is_alphanumeric() || ch == '_' {
        // In WORD mode (`big`), every non-whitespace char is "Word"; in
        // word mode, only alphanumerics + underscore.
        WordClass::Word
    } else {
        WordClass::Punct
    }
}

fn cell_char(term: &Term, row: usize, col: usize) -> char {
    term.viewport_row(row)
        .cells
        .get(col)
        .map(|c| c.ch)
        .unwrap_or('\0')
}

/// Move (row, col) to the next cell in row-major order across the
/// viewport. Returns false at the very last cell.
fn advance(row: &mut usize, col: &mut usize, rows: usize, cols: usize) -> bool {
    if *col + 1 < cols {
        *col += 1;
        true
    } else if *row + 1 < rows {
        *col = 0;
        *row += 1;
        true
    } else {
        false
    }
}

/// Move (row, col) to the previous cell. Returns false at (0, 0).
fn retreat(row: &mut usize, col: &mut usize, cols: usize) -> bool {
    if *col > 0 {
        *col -= 1;
        true
    } else if *row > 0 {
        *col = cols.saturating_sub(1);
        *row -= 1;
        true
    } else {
        false
    }
}

/// Jump to the start of the next word (or WORD).
fn word_next(vi: &mut ViMode, term: &Term, big: bool, rows: usize, cols: usize) {
    let mut r = vi.cursor.0;
    let mut c = vi.cursor.1;
    let start = classify(cell_char(term, r, c), big);
    // Skip the current run if it's not whitespace (whitespace already
    // bleeds into the second loop).
    if start != WordClass::Whitespace {
        while classify(cell_char(term, r, c), big) == start {
            if !advance(&mut r, &mut c, rows, cols) {
                vi.cursor = (r, c);
                return;
            }
        }
    }
    while classify(cell_char(term, r, c), big) == WordClass::Whitespace {
        if !advance(&mut r, &mut c, rows, cols) {
            vi.cursor = (r, c);
            return;
        }
    }
    vi.cursor = (r, c);
}

/// Jump to the end of the current word — or, if already at the end, to
/// the end of the next word.
fn word_end(vi: &mut ViMode, term: &Term, big: bool, rows: usize, cols: usize) {
    let (mut r, mut c) = vi.cursor;
    let (mut nr, mut nc) = (r, c);
    if !advance(&mut nr, &mut nc, rows, cols) {
        return;
    }
    let cur = classify(cell_char(term, r, c), big);
    let next = classify(cell_char(term, nr, nc), big);
    // Two cases that move us into a fresh word:
    //   - we're sitting on whitespace
    //   - we're at the end of the current word (next cell is a different class)
    if cur == WordClass::Whitespace || next != cur {
        r = nr;
        c = nc;
        while classify(cell_char(term, r, c), big) == WordClass::Whitespace {
            if !advance(&mut r, &mut c, rows, cols) {
                vi.cursor = (r, c);
                return;
            }
        }
    }
    // Now within a word; advance to its last cell (look-ahead, commit only
    // while the peek stays in the same class).
    let cur = classify(cell_char(term, r, c), big);
    loop {
        let mut pr = r;
        let mut pc = c;
        if !advance(&mut pr, &mut pc, rows, cols) {
            break;
        }
        if classify(cell_char(term, pr, pc), big) != cur {
            break;
        }
        r = pr;
        c = pc;
    }
    vi.cursor = (r, c);
}

/// Jump back to the start of the current word — or, if already at the
/// start, to the start of the previous word.
fn word_prev(vi: &mut ViMode, term: &Term, big: bool, cols: usize) {
    let (mut r, mut c) = vi.cursor;
    if !retreat(&mut r, &mut c, cols) {
        return;
    }
    while classify(cell_char(term, r, c), big) == WordClass::Whitespace {
        if !retreat(&mut r, &mut c, cols) {
            vi.cursor = (r, c);
            return;
        }
    }
    let cur = classify(cell_char(term, r, c), big);
    loop {
        let mut pr = r;
        let mut pc = c;
        if !retreat(&mut pr, &mut pc, cols) {
            break;
        }
        if classify(cell_char(term, pr, pc), big) != cur {
            break;
        }
        r = pr;
        c = pc;
    }
    vi.cursor = (r, c);
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

    #[test]
    fn count_accumulates_digits() {
        let mut v = ViMode::new();
        v.push_digit(1);
        v.push_digit(5);
        assert_eq!(v.pending_count, Some(15));
        // take_count returns and resets.
        assert_eq!(v.take_count(), 15);
        assert_eq!(v.pending_count, None);
        // No digits → defaults to 1.
        assert_eq!(v.take_count(), 1);
    }

    #[test]
    fn count_caps_to_avoid_runaway() {
        let mut v = ViMode::new();
        for _ in 0..20 {
            v.push_digit(9);
        }
        assert!(v.pending_count.unwrap() <= 10_000);
    }

    #[test]
    fn cancel_pending_clears_count_and_op() {
        let mut v = ViMode::new();
        v.push_digit(7);
        v.pending_op = Some(PendingOp::Goto);
        v.cancel_pending();
        assert!(v.pending_count.is_none());
        assert!(v.pending_op.is_none());
    }

    #[test]
    fn classify_word_vs_punct_vs_ws() {
        assert_eq!(classify('a', false), WordClass::Word);
        assert_eq!(classify('_', false), WordClass::Word);
        assert_eq!(classify('5', false), WordClass::Word);
        assert_eq!(classify(',', false), WordClass::Punct);
        assert_eq!(classify('/', false), WordClass::Punct);
        assert_eq!(classify(' ', false), WordClass::Whitespace);
        // Big-word mode collapses Word and Punct into Word.
        assert_eq!(classify(',', true), WordClass::Word);
        assert_eq!(classify('a', true), WordClass::Word);
        assert_eq!(classify(' ', true), WordClass::Whitespace);
    }
}
