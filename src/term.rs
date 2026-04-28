use vte::{Params, Parser, Perform};

use crate::grid::{CellAttrs, Color, Grid, Row};

pub struct Term {
    primary: Grid,
    alt: Option<Grid>,
    on_alt: bool,
    parser: Parser,
    pub title: String,
    /// 0 = viewport pinned to the bottom of the live grid (the normal case);
    /// N>0 = scrolled N lines into history.
    pub viewport_offset: usize,
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
        }
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

    /// Cursor's visual row/col within the current viewport, if visible.
    /// Returns None when the cursor is scrolled out of the visible region,
    /// or DECTCEM-hidden *while on the alt screen*. On the primary screen we
    /// deliberately ignore `?25l` — programs that crash without restoring the
    /// cursor (e.g. anything that runs `printf("\x1b[?25l")` and then exits)
    /// would otherwise leave the user with a permanently invisible cursor at
    /// the shell prompt. Vim/less/htop all use the alt screen, so they're
    /// unaffected.
    pub fn viewport_cursor(&self) -> Option<(usize, usize)> {
        let g = self.grid();
        let visible = if self.on_alt {
            g.cursor.visible
        } else {
            true
        };
        if !visible {
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
        // Private modes (CSI ? ... h/l) get their own intermediate byte.
        let private = intermediates.first().copied() == Some(b'?');
        let nums: Vec<u16> = collect_first_subparams(params);

        match (private, action) {
            (false, 'A') => self.grid().move_cursor(-(arg(&nums, 0, 1) as isize), 0),
            (false, 'B') => self.grid().move_cursor(arg(&nums, 0, 1) as isize, 0),
            (false, 'C') => self.grid().move_cursor(0, arg(&nums, 0, 1) as isize),
            (false, 'D') => self.grid().move_cursor(0, -(arg(&nums, 0, 1) as isize)),
            (false, 'E') => {
                let n = arg(&nums, 0, 1) as isize;
                let g = self.grid();
                g.move_cursor(n, 0);
                g.carriage_return();
            }
            (false, 'F') => {
                let n = arg(&nums, 0, 1) as isize;
                let g = self.grid();
                g.move_cursor(-n, 0);
                g.carriage_return();
            }
            (false, 'G') => {
                let col = arg(&nums, 0, 1).saturating_sub(1) as usize;
                let r = self.grid().cursor.row;
                self.grid().goto(r, col);
            }
            (false, 'H') | (false, 'f') => {
                let row = arg(&nums, 0, 1).saturating_sub(1) as usize;
                let col = arg(&nums, 1, 1).saturating_sub(1) as usize;
                self.grid().goto(row, col);
            }
            (false, 'J') => self.grid().erase_display(arg(&nums, 0, 0)),
            (false, 'K') => self.grid().erase_line(arg(&nums, 0, 0)),
            (false, 'L') => self.grid().insert_lines(arg(&nums, 0, 1) as usize),
            (false, 'M') => self.grid().delete_lines(arg(&nums, 0, 1) as usize),
            (false, 'P') => self.grid().delete_chars(arg(&nums, 0, 1) as usize),
            (false, '@') => self.grid().insert_chars(arg(&nums, 0, 1) as usize),
            (false, 'X') => self.grid().erase_chars(arg(&nums, 0, 1) as usize),
            (false, 'd') => {
                let row = arg(&nums, 0, 1).saturating_sub(1) as usize;
                let c = self.grid().cursor.col;
                self.grid().goto(row, c);
            }
            (false, 'r') => {
                let top = arg(&nums, 0, 1).saturating_sub(1) as usize;
                let bot = arg(&nums, 1, self.grid().rows as u16).saturating_sub(1) as usize;
                self.grid().set_scroll_region(top, bot);
            }
            (false, 's') => self.grid().save_cursor(),
            (false, 'u') => self.grid().restore_cursor(),
            (false, 'm') => apply_sgr(self.grid(), params),
            (true, 'h') => self.set_private_modes(&nums, true),
            (true, 'l') => self.set_private_modes(&nums, false),
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
        // Window/icon title: OSC 0;<title> or OSC 2;<title>.
        if let [code, rest @ ..] = params {
            if matches!(*code, b"0" | b"2") {
                let title: Vec<u8> = rest.iter().flat_map(|p| p.iter().copied()).collect();
                if let Ok(s) = std::str::from_utf8(&title) {
                    self.term.title = s.to_owned();
                }
            }
        }
    }

    fn hook(&mut self, _: &Params, _: &[u8], _: bool, _: char) {}
    fn put(&mut self, _: u8) {}
    fn unhook(&mut self) {}
}

impl<'a> Performer<'a> {
    fn set_private_modes(&mut self, nums: &[u16], on: bool) {
        for &n in nums {
            match n {
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
                _ => {}
            }
        }
    }
}

fn arg(nums: &[u16], idx: usize, default: u16) -> u16 {
    nums.get(idx)
        .copied()
        .filter(|n| *n != 0)
        .unwrap_or(default)
}

fn collect_first_subparams(params: &Params) -> Vec<u16> {
    params
        .iter()
        .map(|sub| sub.first().copied().unwrap_or(0))
        .collect()
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
