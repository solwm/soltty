use vte::{Params, Parser, Perform};

use crate::grid::{CellAttrs, Color, CursorShape, Grid, Row};

pub struct Term {
    primary: Grid,
    alt: Option<Grid>,
    on_alt: bool,
    parser: Parser,
    pub title: String,
    /// 0 = viewport pinned to the bottom of the live grid (the normal case);
    /// N>0 = scrolled N lines into history.
    pub viewport_offset: usize,
    /// Set/reset by DECSET 2004. When true, pastes should be wrapped in
    /// `ESC [ 200~ ... ESC [ 201~` so the application can distinguish
    /// pasted text from typed input.
    pub bracketed_paste: bool,
    /// DECCKM (DECSET 1). When true, unmodified arrow keys + Home/End
    /// emit `SS3 <letter>` (`ESC O A`) instead of `CSI <letter>`
    /// (`ESC [ A`). Vim and tmux ask for this in some modes; emitting
    /// the wrong form turns vim's normal-mode arrows into noise.
    /// Modifier-encoded arrows (Shift/Ctrl/Alt) keep the `CSI 1;<m><L>`
    /// form regardless — DECCKM only affects the unmodified path.
    pub application_cursor_keys: bool,
    /// Cached theme colors (sRGB). Used to answer `OSC 10/11/12;?`
    /// queries — neovim and tmux ask the terminal for these to pick
    /// contrasting colors. App keeps them in sync with the active
    /// theme via `set_theme_colors`. Sets via `OSC 10/11/12;<color>`
    /// are *ignored* — the theme owns the colors, and apps that try
    /// to override them shouldn't get to.
    pub default_fg: [u8; 3],
    pub default_bg: [u8; 3],
    pub cursor_color: [u8; 3],
    /// Bytes the parser wants to send back to the application (e.g. DSR
    /// cursor-position reports). Drained by the host loop after each
    /// `feed` and written to the PTY master.
    reply: Vec<u8>,
}

impl Term {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            primary: Grid::new(rows, cols, 10_000),
            alt: None,
            on_alt: false,
            parser: Parser::new(),
            title: String::new(),
            viewport_offset: 0,
            bracketed_paste: false,
            application_cursor_keys: false,
            // Until App calls set_theme_colors, OSC color queries
            // return all-black. App sets them right after construction
            // in `resumed`, so this only affects the very first frame.
            default_fg: [0, 0, 0],
            default_bg: [0, 0, 0],
            cursor_color: [0, 0, 0],
            reply: Vec::new(),
        }
    }

    /// Update the cached theme colors so OSC 10/11/12 queries return
    /// the right values. App calls this on startup and after every
    /// theme change.
    pub fn set_theme_colors(&mut self, fg: [u8; 3], bg: [u8; 3], cursor: [u8; 3]) {
        self.default_fg = fg;
        self.default_bg = bg;
        self.cursor_color = cursor;
    }

    /// Hand back any pending reply bytes (e.g. from DSR), clearing the
    /// internal buffer. Called by the host loop after every `feed`.
    pub fn take_reply(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.reply)
    }

    pub fn grid(&self) -> &Grid {
        if self.on_alt {
            self.alt.as_ref().unwrap_or(&self.primary)
        } else {
            &self.primary
        }
    }

    pub fn grid_mut(&mut self) -> &mut Grid {
        if self.on_alt {
            self.alt.as_mut().expect("alt grid must exist when on_alt=true")
        } else {
            &mut self.primary
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        // Snapshot scrollback so we can anchor the viewport if we're scrolled
        // into history while new content arrives.
        let sb_before = self.primary.scrollback.len();

        let mut parser = std::mem::take(&mut self.parser);
        let mut performer = Performer { term: self };
        for &b in bytes {
            parser.advance(&mut performer, b);
        }
        self.parser = parser;

        if self.viewport_offset > 0 {
            let sb_after = self.primary.scrollback.len();
            if sb_after > sb_before {
                let added = sb_after - sb_before;
                self.viewport_offset = (self.viewport_offset + added).min(sb_after);
            } else if sb_after < sb_before {
                // Scrollback ring evicted older lines; shrink offset to match.
                let dropped = sb_before - sb_after;
                self.viewport_offset = self.viewport_offset.saturating_sub(dropped);
            }
        }
    }

    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.primary.resize(rows, cols);
        if let Some(alt) = self.alt.as_mut() {
            alt.resize(rows, cols);
        }
        let max = self.grid().scrollback.len();
        if self.viewport_offset > max {
            self.viewport_offset = max;
        }
    }

    pub fn reset_view(&mut self) {
        self.viewport_offset = 0;
    }

    pub fn scroll_view(&mut self, delta: isize) {
        // Alt screen has no scrollback (vim/less own the whole grid).
        if self.on_alt {
            return;
        }
        let max = self.grid().scrollback.len() as isize;
        let new = (self.viewport_offset as isize + delta).clamp(0, max);
        self.viewport_offset = new as usize;
    }

    /// Row at viewport position `vrow` (0 = top of visible area).
    /// Pulls from scrollback when scrolled up; otherwise from the live grid.
    pub fn viewport_row(&self, vrow: usize) -> &Row {
        let g = self.grid();
        let sb_len = g.scrollback.len();
        // Viewport top in absolute (scrollback ++ live) coords.
        let top = sb_len.saturating_sub(self.viewport_offset);
        let abs = top + vrow;
        if abs < sb_len {
            &g.scrollback[abs]
        } else {
            &g.lines[abs - sb_len]
        }
    }

    /// Cursor's visual row/col within the current viewport, or `None` if
    /// it's scrolled out of view or DECTCEM-hidden via `?25l`. We honor
    /// `?25l` on both screens — programs like gol-c use it on the primary
    /// screen legitimately. If a program exits without restoring (`?25h`)
    /// the user's shell prompt will be cursor-less until the next keypress
    /// or `tput cnorm`; that's the same behavior as every other terminal.
    /// Current cursor shape (per-grid; alt and primary are independent).
    /// Set by DECSCUSR; defaults to Block.
    pub fn cursor_shape(&self) -> CursorShape {
        self.grid().cursor.shape
    }

    pub fn viewport_cursor(&self) -> Option<(usize, usize)> {
        let g = self.grid();
        if !g.cursor.visible {
            return None;
        }
        let vrow = g.cursor.row + self.viewport_offset;
        if vrow < g.rows {
            Some((vrow, g.cursor.col))
        } else {
            None
        }
    }

    fn enter_alt(&mut self) {
        if self.on_alt {
            return;
        }
        let (rows, cols) = (self.primary.rows, self.primary.cols);
        self.primary.save_cursor();
        self.alt = Some(Grid::new(rows, cols, 0));
        self.on_alt = true;
    }

    fn leave_alt(&mut self) {
        if !self.on_alt {
            return;
        }
        self.alt = None;
        self.on_alt = false;
        self.primary.restore_cursor();
    }
}

struct Performer<'a> {
    term: &'a mut Term,
}

impl<'a> Performer<'a> {
    fn grid(&mut self) -> &mut Grid {
        self.term.grid_mut()
    }
}

impl<'a> Perform for Performer<'a> {
    fn print(&mut self, c: char) {
        self.grid().put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        let g = self.grid();
        match byte {
            0x07 => {} // BEL
            0x08 => g.backspace(),
            0x09 => g.tab(),
            0x0A | 0x0B | 0x0C => g.line_feed(),
            0x0D => g.carriage_return(),
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        // DECSCUSR (`CSI Ps SP q`): cursor shape. The space intermediate
        // distinguishes it from any plain `CSI Ps q` (which we don't
        // implement). Apps like vim and zsh-vi-mode emit this to signal
        // editor mode (insert vs. normal/visual).
        if intermediates == [b' '] && action == 'q' {
            self.set_cursor_shape(arg(params, 0, 0));
            return;
        }

        // Private modes (CSI ? ... h/l) get their own intermediate byte.
        let private = intermediates.first().copied() == Some(b'?');

        // SGR (`CSI ... m`) is by far the hottest CSI — every truecolor
        // cell in the GoL stress test is one SGR. `apply_sgr` walks
        // `params` directly, so collecting them into a `Vec<u16>` would
        // be a per-cell heap allocation the dispatch doesn't even use.
        if !private && action == 'm' {
            apply_sgr(self.grid(), params);
            return;
        }

        // Read params lazily out of `params` instead of collecting them
        // into a `Vec<u16>`. Most arms only look at the first one or
        // two args; CUP at random row/col was the cursor_jumps bench's
        // hot spot precisely because we used to allocate per dispatch.

        match (private, action) {
            (false, 'A') => self.grid().move_cursor(-(arg(params, 0, 1) as isize), 0),
            (false, 'B') => self.grid().move_cursor(arg(params, 0, 1) as isize, 0),
            (false, 'C') => self.grid().move_cursor(0, arg(params, 0, 1) as isize),
            (false, 'D') => self.grid().move_cursor(0, -(arg(params, 0, 1) as isize)),
            (false, 'E') => {
                let n = arg(params, 0, 1) as isize;
                let g = self.grid();
                g.move_cursor(n, 0);
                g.carriage_return();
            }
            (false, 'F') => {
                let n = arg(params, 0, 1) as isize;
                let g = self.grid();
                g.move_cursor(-n, 0);
                g.carriage_return();
            }
            (false, 'G') => {
                let col = arg(params, 0, 1).saturating_sub(1) as usize;
                let r = self.grid().cursor.row;
                self.grid().goto(r, col);
            }
            (false, 'H') | (false, 'f') => {
                let row = arg(params, 0, 1).saturating_sub(1) as usize;
                let col = arg(params, 1, 1).saturating_sub(1) as usize;
                self.grid().goto(row, col);
            }
            (false, 'J') => self.grid().erase_display(arg(params, 0, 0)),
            (false, 'K') => self.grid().erase_line(arg(params, 0, 0)),
            (false, 'L') => self.grid().insert_lines(arg(params, 0, 1) as usize),
            (false, 'M') => self.grid().delete_lines(arg(params, 0, 1) as usize),
            (false, 'P') => self.grid().delete_chars(arg(params, 0, 1) as usize),
            (false, '@') => self.grid().insert_chars(arg(params, 0, 1) as usize),
            (false, 'X') => self.grid().erase_chars(arg(params, 0, 1) as usize),
            (false, 'd') => {
                let row = arg(params, 0, 1).saturating_sub(1) as usize;
                let c = self.grid().cursor.col;
                self.grid().goto(row, c);
            }
            (false, 'r') => {
                let top = arg(params, 0, 1).saturating_sub(1) as usize;
                let bot = arg(params, 1, self.grid().rows as u16).saturating_sub(1) as usize;
                self.grid().set_scroll_region(top, bot);
            }
            (false, 's') => self.grid().save_cursor(),
            (false, 'u') => self.grid().restore_cursor(),
            (false, 'm') => apply_sgr(self.grid(), params),
            (false, 'n') => self.dsr(arg(params, 0, 0)),
            (true, 'h') => self.set_private_modes(params, true),
            (true, 'l') => self.set_private_modes(params, false),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if !intermediates.is_empty() {
            return;
        }
        match byte {
            b'7' => self.grid().save_cursor(),
            b'8' => self.grid().restore_cursor(),
            b'D' => self.grid().line_feed(),
            b'E' => {
                let g = self.grid();
                g.line_feed();
                g.carriage_return();
            }
            b'M' => self.grid().reverse_index(),
            b'c' => {
                let (rows, cols) = (self.grid().rows, self.grid().cols);
                *self.grid() = Grid::new(rows, cols, self.grid().scrollback_limit);
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let Some(code) = params.first().copied() else {
            return;
        };
        let payload: Vec<u8> = params[1..].iter().flat_map(|p| p.iter().copied()).collect();
        match code {
            b"0" | b"2" => {
                // Window / icon title.
                if let Ok(s) = std::str::from_utf8(&payload) {
                    self.term.title = s.to_owned();
                }
            }
            b"10" | b"11" | b"12" => {
                // Default fg / bg / cursor color. Apps query with
                // payload="?" to learn our theme; sets are ignored
                // because the theme is the source of truth.
                if payload == b"?" {
                    let (n, color) = match code {
                        b"10" => (10u8, self.term.default_fg),
                        b"11" => (11u8, self.term.default_bg),
                        b"12" => (12u8, self.term.cursor_color),
                        _ => unreachable!(),
                    };
                    // xterm-style reply: `OSC <n>; rgb:RRRR/GGGG/BBBB
                    // BEL`. Each component is 16-bit so we double the
                    // 8-bit byte (`5e -> 5e5e`); apps that want the
                    // truncated value just look at the high half.
                    let resp = format!(
                        "\x1b]{};rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}\x07",
                        n,
                        color[0], color[0],
                        color[1], color[1],
                        color[2], color[2],
                    );
                    self.term.reply.extend_from_slice(resp.as_bytes());
                }
                // Set requests are intentionally not honored.
            }
            _ => {}
        }
    }

    fn hook(&mut self, _: &Params, _: &[u8], _: bool, _: char) {}
    fn put(&mut self, _: u8) {}
    fn unhook(&mut self) {}
}

impl<'a> Performer<'a> {
    /// `CSI Pn n` — Device Status Report. Pn=5 asks "are you ok?",
    /// Pn=6 asks "where's the cursor?". Replies feed back through
    /// `Term::reply` and are written to the PTY by the host loop.
    fn dsr(&mut self, n: u16) {
        match n {
            5 => self.term.reply.extend_from_slice(b"\x1b[0n"),
            6 => {
                let (row, col) = {
                    let g = self.term.grid();
                    (g.cursor.row + 1, g.cursor.col + 1)
                };
                let resp = format!("\x1b[{row};{col}R");
                self.term.reply.extend_from_slice(resp.as_bytes());
            }
            _ => {}
        }
    }

    /// DECSCUSR `Ps` → cursor shape. We collapse blink and steady (we
    /// don't animate). Unknown codes leave the shape untouched, which
    /// matches what real terminals do.
    fn set_cursor_shape(&mut self, ps: u16) {
        let shape = match ps {
            0 | 1 | 2 => CursorShape::Block,
            3 | 4 => CursorShape::Underline,
            5 | 6 => CursorShape::Bar,
            _ => return,
        };
        self.grid().cursor.shape = shape;
    }

    fn set_private_modes(&mut self, params: &Params, on: bool) {
        for sub in params.iter() {
            let n = sub.first().copied().unwrap_or(0);
            match n {
                1 => self.term.application_cursor_keys = on,
                25 => self.grid().cursor.visible = on,
                47 | 1047 | 1049 => {
                    if on {
                        if n == 1049 {
                            // 1049 also clears the alt screen on entry.
                            self.term.enter_alt();
                            self.grid().erase_display(2);
                        } else {
                            self.term.enter_alt();
                        }
                    } else {
                        self.term.leave_alt();
                    }
                }
                2004 => self.term.bracketed_paste = on,
                _ => {}
            }
        }
    }
}

/// Read the n-th param's first subparam. Returns `default` when the
/// param is missing or zero (per ECMA-48: a zero arg is treated as
/// "default" for movement commands). Walks `params` directly without
/// allocating — `nth(idx)` is O(idx) and `idx` is at most 1 in our
/// dispatch.
fn arg(params: &Params, idx: usize, default: u16) -> u16 {
    params
        .iter()
        .nth(idx)
        .and_then(|sub| sub.first().copied())
        .filter(|n| *n != 0)
        .unwrap_or(default)
}

fn apply_sgr(grid: &mut Grid, params: &Params) {
    if params.is_empty() {
        grid.pen = Default::default();
        return;
    }
    // We need to walk params with the ability to consume extended color
    // arguments that may span multiple top-level params (38;5;N or 38;2;R;G;B)
    // OR be packed as subparameters in a single param (38:5:N).
    let mut iter = params.iter().peekable();
    while let Some(sub) = iter.next() {
        // Single subparam — the standard ; form. Look ahead for extended.
        if sub.len() == 1 {
            let code = sub[0];
            match code {
                0 => grid.pen = Default::default(),
                1 => grid.pen.attrs.set(CellAttrs::BOLD, true),
                2 => grid.pen.attrs.set(CellAttrs::DIM, true),
                3 => grid.pen.attrs.set(CellAttrs::ITALIC, true),
                4 => grid.pen.attrs.set(CellAttrs::UNDERLINE, true),
                5 => grid.pen.attrs.set(CellAttrs::BLINK, true),
                7 => grid.pen.attrs.set(CellAttrs::INVERSE, true),
                9 => grid.pen.attrs.set(CellAttrs::STRIKE, true),
                22 => {
                    grid.pen.attrs.set(CellAttrs::BOLD, false);
                    grid.pen.attrs.set(CellAttrs::DIM, false);
                }
                23 => grid.pen.attrs.set(CellAttrs::ITALIC, false),
                24 => grid.pen.attrs.set(CellAttrs::UNDERLINE, false),
                25 => grid.pen.attrs.set(CellAttrs::BLINK, false),
                27 => grid.pen.attrs.set(CellAttrs::INVERSE, false),
                29 => grid.pen.attrs.set(CellAttrs::STRIKE, false),
                30..=37 => grid.pen.fg = Color::Indexed((code - 30) as u8),
                39 => grid.pen.fg = Color::Default,
                40..=47 => grid.pen.bg = Color::Indexed((code - 40) as u8),
                49 => grid.pen.bg = Color::Default,
                90..=97 => grid.pen.fg = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => grid.pen.bg = Color::Indexed((code - 100 + 8) as u8),
                38 => {
                    if let Some(color) = read_extended_color(&mut iter) {
                        grid.pen.fg = color;
                    }
                }
                48 => {
                    if let Some(color) = read_extended_color(&mut iter) {
                        grid.pen.bg = color;
                    }
                }
                _ => {}
            }
        } else {
            // Subparameter form: e.g. [38, 5, n] or [38, 2, r, g, b].
            if let Some(color) = parse_extended_color_subparams(sub) {
                match sub[0] {
                    38 => grid.pen.fg = color,
                    48 => grid.pen.bg = color,
                    _ => {}
                }
            }
        }
    }
}

fn read_extended_color<'a>(
    iter: &mut std::iter::Peekable<vte::ParamsIter<'a>>,
) -> Option<Color> {
    let mode = iter.next()?.first().copied()?;
    match mode {
        5 => {
            let n = iter.next()?.first().copied()?;
            Some(Color::Indexed(n as u8))
        }
        2 => {
            let r = iter.next()?.first().copied()? as u8;
            let g = iter.next()?.first().copied()? as u8;
            let b = iter.next()?.first().copied()? as u8;
            Some(Color::Rgb(r, g, b))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(grid: &Grid, row: usize) -> String {
        grid.lines[row].cells.iter().map(|c| c.ch).collect()
    }

    #[test]
    fn prints_plain_text() {
        let mut t = Term::new(4, 10);
        t.feed(b"hello");
        assert!(line(t.grid(), 0).starts_with("hello"));
        assert_eq!(t.grid().cursor.row, 0);
        assert_eq!(t.grid().cursor.col, 5);
    }

    #[test]
    fn lf_advances_row_cr_resets_col() {
        let mut t = Term::new(4, 10);
        t.feed(b"ab\r\ncd");
        assert!(line(t.grid(), 0).starts_with("ab"));
        assert!(line(t.grid(), 1).starts_with("cd"));
    }

    #[test]
    fn line_wraps_at_right_edge() {
        let mut t = Term::new(4, 5);
        t.feed(b"abcdefg");
        assert_eq!(&line(t.grid(), 0)[..5], "abcde");
        assert_eq!(&line(t.grid(), 1)[..2], "fg");
    }

    #[test]
    fn scrolls_when_lf_past_bottom() {
        let mut t = Term::new(2, 4);
        t.feed(b"a\r\nb\r\nc");
        assert_eq!(t.grid().scrollback.len(), 1);
        assert!(line(t.grid(), 0).starts_with("b"));
        assert!(line(t.grid(), 1).starts_with("c"));
    }

    #[test]
    fn sgr_truecolor_fg_applies_to_pen() {
        let mut t = Term::new(2, 4);
        t.feed(b"\x1b[38;2;10;20;30mX");
        let cell = &t.grid().lines[0].cells[0];
        assert_eq!(cell.ch, 'X');
        assert_eq!(cell.fg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn cup_positions_cursor() {
        let mut t = Term::new(5, 10);
        t.feed(b"\x1b[3;5HX");
        assert_eq!(t.grid().lines[2].cells[4].ch, 'X');
    }

    #[test]
    fn scroll_view_walks_into_scrollback() {
        let mut t = Term::new(2, 4);
        t.feed(b"a\r\nb\r\nc\r\nd"); // pushes "a", "b" to scrollback, "c\nd" live
        assert_eq!(t.grid().scrollback.len(), 2);
        t.scroll_view(2);
        assert_eq!(t.viewport_offset, 2);
        // viewport_row(0) should now be the oldest scrollback line
        assert!(t.viewport_row(0).cells[0].ch == 'a');
        // Cursor scrolled out of view
        assert!(t.viewport_cursor().is_none());
    }

    #[test]
    fn keypress_resets_view() {
        let mut t = Term::new(2, 4);
        t.feed(b"a\r\nb\r\nc\r\nd");
        t.scroll_view(2);
        assert_eq!(t.viewport_offset, 2);
        t.reset_view();
        assert_eq!(t.viewport_offset, 0);
        assert!(t.viewport_cursor().is_some());
    }

    #[test]
    fn viewport_anchors_when_output_arrives_during_scrollback() {
        let mut t = Term::new(2, 4);
        t.feed(b"a\r\nb\r\nc\r\nd"); // scrollback=[a,b], live=[c,d]
        t.scroll_view(2);            // viewing [a,b]
        let top_before = t.viewport_row(0).cells[0].ch;
        t.feed(b"\r\ne");            // pushes "c" into scrollback
        // Anchored: top of viewport should still show 'a'
        assert_eq!(top_before, 'a');
        assert_eq!(t.viewport_row(0).cells[0].ch, 'a');
    }

    #[test]
    fn decscusr_sets_cursor_shape() {
        let mut t = Term::new(2, 4);
        // Default is Block.
        assert_eq!(t.cursor_shape(), CursorShape::Block);
        // Steady bar.
        t.feed(b"\x1b[6 q");
        assert_eq!(t.cursor_shape(), CursorShape::Bar);
        // Blinking underline collapses to plain Underline.
        t.feed(b"\x1b[3 q");
        assert_eq!(t.cursor_shape(), CursorShape::Underline);
        // No-arg form == reset to default Block.
        t.feed(b"\x1b[ q");
        assert_eq!(t.cursor_shape(), CursorShape::Block);
    }

    #[test]
    fn cursor_shape_is_per_grid() {
        let mut t = Term::new(3, 4);
        t.feed(b"\x1b[5 q"); // Bar on primary
        assert_eq!(t.cursor_shape(), CursorShape::Bar);
        t.feed(b"\x1b[?1049h"); // enter alt
        // Alt starts fresh, default Block.
        assert_eq!(t.cursor_shape(), CursorShape::Block);
        t.feed(b"\x1b[3 q"); // Underline on alt
        assert_eq!(t.cursor_shape(), CursorShape::Underline);
        t.feed(b"\x1b[?1049l"); // back to primary
        // Primary's shape preserved.
        assert_eq!(t.cursor_shape(), CursorShape::Bar);
    }

    #[test]
    fn osc_11_query_reports_default_bg() {
        let mut t = Term::new(2, 4);
        t.set_theme_colors([0xab, 0xcd, 0xef], [0x12, 0x34, 0x56], [0xff, 0xff, 0xff]);
        // OSC 11 ; ? BEL  → query for default background.
        t.feed(b"\x1b]11;?\x07");
        let reply = t.take_reply();
        let s = std::str::from_utf8(&reply).unwrap();
        // Both 8-bit halves of each component should appear (16-bit
        // form). Component bytes are 12 / 34 / 56 → "1212/3434/5656".
        assert!(s.starts_with("\x1b]11;rgb:1212/3434/5656"));
        assert!(s.ends_with('\x07'));
    }

    #[test]
    fn osc_10_and_12_query_use_their_colors() {
        let mut t = Term::new(2, 4);
        t.set_theme_colors([0x10, 0x20, 0x30], [0x99, 0x88, 0x77], [0xaa, 0xbb, 0xcc]);
        t.feed(b"\x1b]10;?\x07");
        let r = t.take_reply();
        assert!(std::str::from_utf8(&r)
            .unwrap()
            .starts_with("\x1b]10;rgb:1010/2020/3030"));
        t.feed(b"\x1b]12;?\x07");
        let r = t.take_reply();
        assert!(std::str::from_utf8(&r)
            .unwrap()
            .starts_with("\x1b]12;rgb:aaaa/bbbb/cccc"));
    }

    #[test]
    fn osc_11_set_request_is_ignored() {
        let mut t = Term::new(2, 4);
        let original_bg = [0x28, 0x2a, 0x36];
        t.set_theme_colors([0xff, 0xff, 0xff], original_bg, [0xaa, 0xbb, 0xcc]);
        // App tries to override the background.
        t.feed(b"\x1b]11;rgb:ff0000\x07");
        // No reply (it wasn't a query) and the cached bg didn't change.
        assert!(t.take_reply().is_empty());
        assert_eq!(t.default_bg, original_bg);
    }

    #[test]
    fn decckm_toggles_application_cursor_keys() {
        let mut t = Term::new(2, 4);
        // Default off.
        assert!(!t.application_cursor_keys);
        // DECSET 1 → on.
        t.feed(b"\x1b[?1h");
        assert!(t.application_cursor_keys);
        // DECRESET 1 → off.
        t.feed(b"\x1b[?1l");
        assert!(!t.application_cursor_keys);
        // Combined DECSET with multiple modes: only DECCKM should flip.
        t.feed(b"\x1b[?1;2004h");
        assert!(t.application_cursor_keys);
        assert!(t.bracketed_paste);
    }

    #[test]
    fn alt_screen_round_trip() {
        let mut t = Term::new(3, 4);
        t.feed(b"abc\x1b[?1049h");
        assert!(t.on_alt);
        t.feed(b"X");
        assert_eq!(t.grid().lines[0].cells[0].ch, 'X');
        t.feed(b"\x1b[?1049l");
        assert!(!t.on_alt);
        assert!(line(t.grid(), 0).starts_with("abc"));
    }
}

fn parse_extended_color_subparams(sub: &[u16]) -> Option<Color> {
    match sub.get(1).copied()? {
        5 => Some(Color::Indexed(sub.get(2).copied()? as u8)),
        2 => {
            // Some emitters use [38, 2, _, r, g, b] (with a colorspace slot).
            let (r, g, b) = if sub.len() >= 6 {
                (sub[3] as u8, sub[4] as u8, sub[5] as u8)
            } else {
                (
                    sub.get(2).copied()? as u8,
                    sub.get(3).copied()? as u8,
                    sub.get(4).copied()? as u8,
                )
            };
            Some(Color::Rgb(r, g, b))
        }
        _ => None,
    }
}
