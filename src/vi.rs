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

use crate::selection::{Selection, SelectionMode};
use crate::term::Term;

/// What kind of visual selection the user is in. `None` means vi-mode
/// normal (motion only); `Char`, `Line`, `Block` correspond to vim's
/// `v`, `V`, `Ctrl-v`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum VisualMode {
    None,
    Char,
    Line,
    Block,
}

/// Multi-key sequences in vim wait for one or more follow-up keys. We
/// stash that in-between state here; each variant carries whatever it
/// needs to finish once the user types the next key.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PendingOp {
    /// Saw `g`, waiting for the second `g` to mean DocStart.
    Goto,
    /// Saw `y`, waiting for a motion / text-object / second `y` (yy)
    /// to determine the yank range. `start_abs` is the cursor's
    /// absolute (scrollback ++ live) position when `y` was pressed —
    /// using absolute coords means motions that scroll the viewport
    /// (`G`, `gg`, page motions) still produce correct ranges.
    Yank { start_abs: (usize, usize) },
    /// Saw `yi`, waiting for a text-object char (`w`, `W`, …) to
    /// yank the inner range.
    YankInner,
    /// Saw `ya`, waiting for a text-object char (`w`, `W`, …) to
    /// yank the surrounding range (object plus adjacent whitespace).
    YankAround,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SearchDirection {
    Forward,  // /query
    Backward, // ?query
}

/// One substring hit, in absolute (scrollback ++ live) row coords. The
/// columns are inclusive — `col_end` is the cell of the last char of
/// the match, not one past it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub row: usize,
    pub col_start: usize,
    pub col_end: usize,
}

/// Live-search state. Set when the user presses `/` or `?`; cleared on
/// Esc-cancel or vi-mode exit. While `typing` is true, every keystroke
/// either edits the query or commits/cancels — vi motions are inert.
pub struct Search {
    pub direction: SearchDirection,
    pub query: String,
    pub typing: bool,
    pub matches: Vec<Match>,
    pub current: Option<usize>,
    /// Where the vi cursor was when search opened. Esc-cancel restores
    /// this so the user lands back at their starting point.
    pub origin_cursor: (usize, usize),
    pub origin_viewport_offset: usize,
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
    /// Active search (live-as-typed and post-commit). `None` when no
    /// search is running. The `typing` flag inside distinguishes "user
    /// is editing query" from "matches are highlighted, n/N navigates".
    pub search: Option<Search>,
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
            search: None,
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
        self.search = None;
    }

    pub fn exit(&mut self) {
        self.active = false;
        self.visual = VisualMode::None;
        self.visual_anchor = None;
        self.pending_count = None;
        self.pending_op = None;
        self.search = None;
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

    pub fn start_visual_block(&mut self) {
        self.visual = VisualMode::Block;
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
                sel.mode = SelectionMode::Char;
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
                sel.mode = SelectionMode::Char;
                Some(sel)
            }
            VisualMode::Block => {
                // Block mode keeps anchor and cursor as the diagonal
                // corners — `Selection::contains` and `extract_text`
                // do per-axis min/max via `block_bounds`.
                let mut sel = Selection::new(anchor);
                sel.end = self.cursor;
                sel.dragging = false;
                sel.mode = SelectionMode::Block;
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
    /// Default activation: `Ctrl+N`. Note: this shadows readline's
    /// "next history" binding at the shell prompt; users who want that
    /// back can set `SOLTTY_VI_KEY` to e.g. `ctrl+shift+space` or
    /// `alt+v`.
    pub fn default_activation() -> Self {
        Self {
            key: KeyMatch::Char('n'),
            ctrl: true,
            shift: false,
            alt: false,
        }
    }

    /// Resolve the activation keybind. Precedence: `SOLTTY_VI_KEY`
    /// env var > config-file value > built-in default (`Ctrl+N`). Bad
    /// input at any source logs a warning and falls through; the
    /// terminal always starts.
    pub fn resolve(config_value: Option<&str>) -> Self {
        if let Ok(s) = std::env::var("SOLTTY_VI_KEY") {
            match parse_vi_key(&s) {
                Some(k) => {
                    log::info!("vi-mode keybind: {s:?} (from SOLTTY_VI_KEY)");
                    return k;
                }
                None => {
                    log::warn!(
                        "SOLTTY_VI_KEY={s:?} did not parse; trying config-file value"
                    );
                }
            }
        }
        if let Some(s) = config_value {
            match parse_vi_key(s) {
                Some(k) => {
                    log::info!("vi-mode keybind: {s:?} (from config)");
                    return k;
                }
                None => {
                    log::warn!("config vi_mode={s:?} did not parse; using default");
                }
            }
        }
        Self::default_activation()
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

/// Substring scan over the entire scrollback + live grid. Returns hits
/// in absolute (scrollback ++ live) row coords. Empty query → empty
/// list. Cells are 1 char each so column index == char index.
///
/// Non-overlapping: after a hit at (row, c), the next search on that
/// row resumes at `c + query.len()`. "aaaa" / "aa" → 2 matches at 0
/// and 2, not 3 with overlap. Matches what most users expect.
pub fn find_matches(term: &Term, query: &str) -> Vec<Match> {
    let mut out = Vec::new();
    if query.is_empty() {
        return out;
    }
    let g = term.grid();
    let sb_len = g.scrollback.len();
    let total = sb_len + g.lines.len();
    let qlen = query.chars().count();
    for abs_row in 0..total {
        let row = if abs_row < sb_len {
            &g.scrollback[abs_row]
        } else {
            &g.lines[abs_row - sb_len]
        };
        let line: String = row.cells.iter().map(|c| c.ch).collect();
        let mut byte_from = 0usize;
        while let Some(rel) = line[byte_from..].find(query) {
            let abs_byte = byte_from + rel;
            let start_char = line[..abs_byte].chars().count();
            out.push(Match {
                row: abs_row,
                col_start: start_char,
                col_end: start_char + qlen - 1,
            });
            byte_from = abs_byte + query.len();
            if byte_from >= line.len() {
                break;
            }
        }
    }
    out
}

/// Choose the "current" match index given the search-origin position
/// and direction. Wraps around at ends — vim does this and users expect
/// it so a single-match-far-away search is still findable.
pub fn pick_current_match(
    matches: &[Match],
    from_row: usize,
    from_col: usize,
    direction: SearchDirection,
) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    match direction {
        SearchDirection::Forward => matches
            .iter()
            .position(|m| {
                m.row > from_row || (m.row == from_row && m.col_start >= from_col)
            })
            .or(Some(0)),
        SearchDirection::Backward => matches
            .iter()
            .rposition(|m| {
                m.row < from_row || (m.row == from_row && m.col_start <= from_col)
            })
            .or(Some(matches.len() - 1)),
    }
}

/// Convert a viewport-relative row to absolute (scrollback ++ live)
/// coords given a specific viewport_offset. Used when we need to
/// remember an origin even after the viewport scrolls.
pub fn viewport_to_absolute(term: &Term, vrow: usize, viewport_offset: usize) -> usize {
    let sb_len = term.grid().scrollback.len();
    let top = sb_len.saturating_sub(viewport_offset);
    top + vrow
}

/// Same as `viewport_to_absolute` but for a (row, col) pair using the
/// term's *current* viewport_offset. Convenient for the operator-yank
/// path, which captures the cursor's absolute position at `y` so the
/// post-motion endpoint stays consistent even if the motion scrolls.
pub fn cursor_to_absolute(term: &Term, cursor: (usize, usize)) -> (usize, usize) {
    (
        viewport_to_absolute(term, cursor.0, term.viewport_offset),
        cursor.1,
    )
}

/// Pull the text inside `[start, end]` (inclusive both ends) as a
/// String, with line breaks at row boundaries. Both endpoints are in
/// absolute (scrollback ++ live) coords so this works across viewport
/// scrolls. Trailing whitespace on each row is trimmed — same policy
/// as `selection::extract_text` for `Char` mode.
pub fn extract_absolute_range(
    term: &Term,
    start: (usize, usize),
    end: (usize, usize),
) -> String {
    let (s, e) = if start <= end { (start, end) } else { (end, start) };
    let g = term.grid();
    let sb_len = g.scrollback.len();
    let live_len = g.lines.len();
    let total = sb_len + live_len;
    let cols = g.cols;
    let mut out = String::new();
    for abs_row in s.0..=e.0 {
        if abs_row >= total {
            break;
        }
        let row = if abs_row < sb_len {
            &g.scrollback[abs_row]
        } else {
            &g.lines[abs_row - sb_len]
        };
        let start_col = if abs_row == s.0 { s.1 } else { 0 };
        let end_col = if abs_row == e.0 {
            (e.1 + 1).min(cols)
        } else {
            cols
        };
        if start_col >= row.cells.len() {
            // Row truncated to fewer cells than the range expected; emit
            // nothing for this row.
        } else {
            let end_clamped = end_col.min(row.cells.len());
            let line: String = row.cells[start_col..end_clamped]
                .iter()
                .map(|c| c.ch)
                .collect();
            out.push_str(line.trim_end());
        }
        if abs_row < e.0 {
            out.push('\n');
        }
    }
    out
}

/// Inner-word text object: the run of same-class characters around the
/// cursor's column on its current viewport row. `big` toggles word vs
/// WORD semantics (alphanumerics + underscore vs any non-whitespace).
/// Returns viewport-coord endpoints; both have the same row since vim's
/// iw doesn't cross line boundaries.
pub fn inner_word_bounds(
    term: &Term,
    cursor: (usize, usize),
    big: bool,
) -> ((usize, usize), (usize, usize)) {
    let (row, col) = cursor;
    let cells = &term.viewport_row(row).cells;
    if cells.is_empty() {
        return ((row, col), (row, col));
    }
    let col = col.min(cells.len() - 1);
    let cur = classify(cells[col].ch, big);
    let mut start = col;
    while start > 0 && classify(cells[start - 1].ch, big) == cur {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < cells.len() && classify(cells[end + 1].ch, big) == cur {
        end += 1;
    }
    ((row, start), (row, end))
}

/// Around-word text object: the inner-word range plus adjacent
/// whitespace on one side. Vim prefers the run *after* the word; if
/// there's none (word at line end), it falls back to whitespace
/// *before* the word. Returns viewport-coord endpoints.
pub fn around_word_bounds(
    term: &Term,
    cursor: (usize, usize),
    big: bool,
) -> ((usize, usize), (usize, usize)) {
    let ((row, mut start), (_, mut end)) = inner_word_bounds(term, cursor, big);
    let cells = &term.viewport_row(row).cells;
    if cells.is_empty() {
        return ((row, start), (row, end));
    }
    let cur_class = classify(cells[start].ch, big);
    if cur_class == WordClass::Whitespace {
        // Cursor is on whitespace already → just give back the run.
        return ((row, start), (row, end));
    }
    // Try to extend into trailing whitespace.
    let mut extended = end;
    while extended + 1 < cells.len()
        && classify(cells[extended + 1].ch, big) == WordClass::Whitespace
    {
        extended += 1;
    }
    if extended > end {
        end = extended;
    } else {
        // No trailing whitespace; absorb leading instead.
        while start > 0 && classify(cells[start - 1].ch, big) == WordClass::Whitespace {
            start -= 1;
        }
    }
    ((row, start), (row, end))
}

/// Scroll the viewport (if needed) so that `match_row` (absolute) is
/// visible. Returns the viewport row the match ended up on. We center
/// when the match was off-screen; if it's already in view, we leave
/// the viewport alone.
pub fn jump_viewport_to(term: &mut Term, match_row: usize, view_rows: usize) -> usize {
    let sb_len = term.grid().scrollback.len();
    let cur_top = sb_len.saturating_sub(term.viewport_offset);
    let in_view = match_row >= cur_top && match_row < cur_top + view_rows;
    if !in_view {
        let target_top = match_row.saturating_sub(view_rows / 2);
        let new_offset = sb_len.saturating_sub(target_top);
        let delta = new_offset as isize - term.viewport_offset as isize;
        term.scroll_view(delta);
    }
    let new_top = term
        .grid()
        .scrollback
        .len()
        .saturating_sub(term.viewport_offset);
    match_row.saturating_sub(new_top)
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
        let n = Key::Character(SmolStr::new("n"));
        // Plain Ctrl+N → match.
        assert!(k.matches(&n, ModifiersState::CONTROL));
        // Extra modifiers → reject.
        assert!(!k.matches(&n, ModifiersState::CONTROL | ModifiersState::SHIFT));
        // Missing ctrl → reject.
        assert!(!k.matches(&n, ModifiersState::empty()));
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
    fn find_matches_basic() {
        let mut t = Term::new(4, 20);
        t.feed(b"hello world\r\nfoo bar baz\r\nhello again");
        let m = find_matches(&t, "hello");
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].row, 0);
        assert_eq!(m[0].col_start, 0);
        assert_eq!(m[0].col_end, 4);
        assert_eq!(m[1].row, 2);
        assert_eq!(m[1].col_start, 0);
        assert_eq!(m[1].col_end, 4);
    }

    #[test]
    fn find_matches_multiple_per_line() {
        let mut t = Term::new(2, 30);
        t.feed(b"foofoo bar foo baz");
        // "foo" appears at cols 0, 3, 11. We don't allow overlap, so the
        // run "foofoo" yields hits at 0 and 3, not 0/1/2/3 as overlapping
        // would. A hit at 0 advances 3 chars, so the next viable position
        // is col 3.
        let m = find_matches(&t, "foo");
        let cols: Vec<usize> = m.iter().map(|x| x.col_start).collect();
        assert_eq!(cols, vec![0, 3, 11]);
    }

    #[test]
    fn find_matches_empty_query_is_empty() {
        let mut t = Term::new(2, 10);
        t.feed(b"hello");
        assert!(find_matches(&t, "").is_empty());
    }

    #[test]
    fn pick_current_match_forward_picks_next() {
        let ms = vec![
            Match { row: 1, col_start: 0, col_end: 2 },
            Match { row: 5, col_start: 3, col_end: 5 },
            Match { row: 10, col_start: 0, col_end: 2 },
        ];
        assert_eq!(
            pick_current_match(&ms, 3, 0, SearchDirection::Forward),
            Some(1)
        );
        // Past the last → wrap to first.
        assert_eq!(
            pick_current_match(&ms, 11, 0, SearchDirection::Forward),
            Some(0)
        );
    }

    #[test]
    fn pick_current_match_backward_picks_prev() {
        let ms = vec![
            Match { row: 1, col_start: 0, col_end: 2 },
            Match { row: 5, col_start: 3, col_end: 5 },
            Match { row: 10, col_start: 0, col_end: 2 },
        ];
        assert_eq!(
            pick_current_match(&ms, 7, 0, SearchDirection::Backward),
            Some(1)
        );
        // Before the first → wrap to last.
        assert_eq!(
            pick_current_match(&ms, 0, 0, SearchDirection::Backward),
            Some(2)
        );
    }

    #[test]
    fn inner_word_finds_run_around_cursor() {
        let mut t = Term::new(2, 30);
        t.feed(b"hello world,foo");
        // Cursor on 'l' (col 3) — inner word is "hello" (cols 0..=4).
        let ((r1, c1), (r2, c2)) = inner_word_bounds(&t, (0, 3), false);
        assert_eq!((r1, c1), (0, 0));
        assert_eq!((r2, c2), (0, 4));

        // Cursor on space (col 5) — inner whitespace is just that one cell.
        let ((_, c1), (_, c2)) = inner_word_bounds(&t, (0, 5), false);
        assert_eq!(c1, 5);
        assert_eq!(c2, 5);

        // Cursor on ',' (col 11) — punctuation is its own class in word
        // (small) mode; just the comma.
        let ((_, c1), (_, c2)) = inner_word_bounds(&t, (0, 11), false);
        assert_eq!((c1, c2), (11, 11));

        // Big-word mode: punctuation collapses with letters, so the
        // entire "world,foo" run from col 6 to 14 is one WORD.
        let ((_, c1), (_, c2)) = inner_word_bounds(&t, (0, 11), true);
        assert_eq!((c1, c2), (6, 14));
    }

    #[test]
    fn around_word_extends_into_trailing_ws() {
        let mut t = Term::new(2, 30);
        t.feed(b"foo  bar  baz");
        // Cursor on 'f' (col 0) → around is "foo  " (cols 0..=4).
        let ((_, c1), (_, c2)) = around_word_bounds(&t, (0, 1), false);
        assert_eq!((c1, c2), (0, 4));

        // Cursor on 'b' of "bar" (col 5) → around is "bar  " (cols 5..=9).
        let ((_, c1), (_, c2)) = around_word_bounds(&t, (0, 5), false);
        assert_eq!((c1, c2), (5, 9));
    }

    #[test]
    fn extract_absolute_range_crosses_rows() {
        let mut t = Term::new(3, 10);
        t.feed(b"hello\r\nworld\r\nfoo");
        // Live grid only (no scrollback) — absolute row indices 0..=2.
        let s = extract_absolute_range(&t, (0, 2), (1, 1));
        assert_eq!(s, "llo\nwo");
    }

    #[test]
    fn extract_absolute_range_normalises_swapped_endpoints() {
        let mut t = Term::new(2, 20);
        t.feed(b"abcdefghij");
        let forward = extract_absolute_range(&t, (0, 1), (0, 4));
        let backward = extract_absolute_range(&t, (0, 4), (0, 1));
        assert_eq!(forward, backward);
        assert_eq!(forward, "bcde");
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
