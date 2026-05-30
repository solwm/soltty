use std::collections::VecDeque;

/// Approximate East Asian Wide / Fullwidth detection. Returns 2 for
/// codepoints that should occupy two terminal cells (CJK Unified
/// Ideographs, Kana, Hangul, fullwidth forms, common emoji blocks);
/// 1 for everything else.
///
/// This is hand-rolled from the obvious EAW=W/F ranges rather than
/// pulling in `unicode-width` — coverage is ~98% of typical CJK use,
/// and rare codepoints we miss simply render as width 1 (slightly
/// misaligned, not broken). If a user reports a missing range we add
/// it; the alternative (a multi-thousand-line table) is more code than
/// the rest of the grid module combined.
pub fn char_width(ch: char) -> u8 {
    let c = ch as u32;
    // Hot path: ASCII + Latin Extended A/B + IPA + Cyrillic + Greek
    // are *never* wide. Short-circuit so the per-cell cost in heavy
    // ASCII workloads (gol-c, syntax-highlighted code, plain text)
    // doesn't pay for the full EAW range scan.
    if c < 0x300 {
        return 1;
    }
    let wide = matches!(
        c,
        // CJK Symbols & Punctuation, Hiragana, Katakana, Bopomofo,
        // Hangul Compatibility Jamo, Kanbun, Bopomofo Extended,
        // CJK Strokes, Katakana Phonetic Extensions, Enclosed CJK,
        // CJK Compatibility, CJK Ext A
        0x3000..=0x303E
            | 0x3040..=0x309F
            | 0x30A0..=0x30FF
            | 0x3100..=0x312F
            | 0x3130..=0x318F
            | 0x31C0..=0x31EF
            | 0x31F0..=0x31FF
            | 0x3200..=0x32FF
            | 0x3300..=0x33FF
            | 0x3400..=0x4DBF
            // CJK Unified Ideographs (main block)
            | 0x4E00..=0x9FFF
            // Yi Syllables / Yi Radicals
            | 0xA000..=0xA4CF
            // Hangul Syllables
            | 0xAC00..=0xD7A3
            // CJK Compatibility Ideographs
            | 0xF900..=0xFAFF
            // Vertical Forms / CJK Compatibility Forms
            | 0xFE30..=0xFE4F
            // Fullwidth Forms (ASCII range fullwidth — but NOT halfwidth)
            | 0xFF00..=0xFF60
            // Fullwidth signs (¥ etc.)
            | 0xFFE0..=0xFFE6
            // Common emoji blocks (most are wide in monospace contexts)
            | 0x1F300..=0x1F5FF
            | 0x1F600..=0x1F64F
            | 0x1F680..=0x1F6FF
            | 0x1F900..=0x1F9FF
            // CJK Unified Ideographs Extensions B-G
            | 0x20000..=0x2FFFD
    );
    if wide { 2 } else { 1 }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Color {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Default for Color {
    fn default() -> Self {
        Color::Default
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct CellAttrs(pub u16);

impl CellAttrs {
    pub const BOLD: u16 = 1 << 0;
    pub const DIM: u16 = 1 << 1;
    pub const ITALIC: u16 = 1 << 2;
    pub const UNDERLINE: u16 = 1 << 3;
    pub const INVERSE: u16 = 1 << 4;
    pub const STRIKE: u16 = 1 << 5;
    pub const BLINK: u16 = 1 << 6;

    pub fn set(&mut self, mask: u16, on: bool) {
        if on {
            self.0 |= mask;
        } else {
            self.0 &= !mask;
        }
    }
    #[allow(dead_code)] // used by renderer in milestone 4
    pub fn has(self, mask: u16) -> bool {
        self.0 & mask != 0
    }
}

#[derive(Copy, Clone, Debug)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: CellAttrs,
    /// Cell width category:
    ///   1 = single-width (the common case)
    ///   2 = leading half of a wide character (CJK / fullwidth / emoji)
    ///   0 = spacer right of a wide character (no glyph of its own)
    pub width: u8,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            attrs: CellAttrs::default(),
            width: 1,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    pub cells: Vec<Cell>,
}

impl Row {
    fn blank(cols: usize, pen: Pen) -> Self {
        let cell = pen.blank_cell();
        Self {
            cells: vec![cell; cols],
        }
    }

    fn clear_with(&mut self, pen: Pen) {
        let cell = pen.blank_cell();
        for c in &mut self.cells {
            *c = cell;
        }
    }
}

/// Cursor shape for the renderer. The first three are settable by apps
/// via DECSCUSR (`CSI Ps SP q`) — blink/steady collapse, since we don't
/// animate the DECSCUSR way (see `App::cursor_visible_now`). HollowBlock
/// is internal and used for the vi-mode cursor; apps can't request it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
    HollowBlock,
}

#[derive(Copy, Clone, Debug, Default)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub visible: bool,
    /// Pending wrap: the cursor is at (row, cols) and the next print should wrap.
    /// Standard xterm "cursor stickiness" — printing at the right edge does NOT
    /// wrap until the next character actually arrives.
    pub wrap_next: bool,
    pub shape: CursorShape,
}

#[derive(Copy, Clone, Debug, Default)]
pub struct Pen {
    pub fg: Color,
    pub bg: Color,
    pub attrs: CellAttrs,
}

impl Pen {
    fn blank_cell(self) -> Cell {
        Cell {
            ch: ' ',
            fg: self.fg,
            bg: self.bg,
            attrs: CellAttrs::default(),
            width: 1,
        }
    }
}

pub struct Grid {
    pub rows: usize,
    pub cols: usize,
    /// Live grid rows. Stored as a `VecDeque` so full-screen scrolling
    /// (the common case) is O(1) — pop the evicted top and push a
    /// fresh blank at the bottom, no shifting. Region scrolls inside
    /// a scroll region are still O(n) but those are much rarer.
    pub lines: VecDeque<Row>,
    pub scrollback: VecDeque<Row>,
    pub scrollback_limit: usize,
    pub cursor: Cursor,
    pub pen: Pen,
    pub saved: Option<(Cursor, Pen)>,
    /// Inclusive top of the scrolling region (0..rows).
    pub scroll_top: usize,
    /// Inclusive bottom of the scrolling region (0..rows).
    pub scroll_bot: usize,
}

impl Grid {
    pub fn new(rows: usize, cols: usize, scrollback_limit: usize) -> Self {
        let pen = Pen::default();
        let mut lines: VecDeque<Row> = VecDeque::with_capacity(rows);
        for _ in 0..rows {
            lines.push_back(Row::blank(cols, pen));
        }
        Self {
            rows,
            cols,
            lines,
            scrollback: VecDeque::new(),
            scrollback_limit,
            cursor: Cursor {
                visible: true,
                ..Cursor::default()
            },
            pen,
            saved: None,
            scroll_top: 0,
            scroll_bot: rows.saturating_sub(1),
        }
    }

    pub fn resize(&mut self, rows: usize, cols: usize) {
        // Naive resize: extend/truncate rows and cells. A future pass can reflow.
        if cols != self.cols {
            for row in &mut self.lines {
                row.cells.resize(cols, self.pen.blank_cell());
            }
            self.cols = cols;
        }
        if rows != self.rows {
            if rows > self.rows {
                for _ in self.rows..rows {
                    self.lines.push_back(Row::blank(cols, self.pen));
                }
            } else {
                self.lines.truncate(rows);
            }
            self.rows = rows;
            self.scroll_top = 0;
            self.scroll_bot = rows.saturating_sub(1);
        }
        self.cursor.row = self.cursor.row.min(rows.saturating_sub(1));
        self.cursor.col = self.cursor.col.min(cols.saturating_sub(1));
        self.cursor.wrap_next = false;
    }

    /// ASCII fast path: caller knows the byte is in 0x20..=0x7E. Saves
    /// the `char_width` call, the wide-character wrap check, and the
    /// wide spacer write. Hot under any text-heavy workload — the
    /// printable-ASCII run in `Term::feed` calls this directly so the
    /// per-cell cost in gol-c / scroll-of-text is as tight as possible.
    #[inline]
    pub fn put_ascii_fast(&mut self, byte: u8) {
        if self.cursor.wrap_next {
            self.cursor.col = 0;
            self.line_feed();
            self.cursor.wrap_next = false;
        }
        let r = self.cursor.row;
        let c = self.cursor.col;
        if r < self.rows && c < self.cols {
            let pen_fg = self.pen.fg;
            let pen_bg = self.pen.bg;
            let pen_attrs = self.pen.attrs;
            let cell = &mut self.lines[r].cells[c];
            cell.ch = byte as char;
            cell.fg = pen_fg;
            cell.bg = pen_bg;
            cell.attrs = pen_attrs;
            cell.width = 1;
        }
        if c + 1 >= self.cols {
            self.cursor.wrap_next = true;
        } else {
            self.cursor.col = c + 1;
        }
    }

    pub fn put_char(&mut self, ch: char) {
        let width = char_width(ch);

        if self.cursor.wrap_next {
            self.cursor.col = 0;
            self.line_feed();
            self.cursor.wrap_next = false;
        }

        // A wide character that would straddle the right edge can't be
        // placed where it stands — wrap first so it lands intact at the
        // start of the next row.
        if width == 2 && self.cursor.col + 1 >= self.cols {
            self.cursor.col = 0;
            self.line_feed();
        }

        let r = self.cursor.row;
        let c = self.cursor.col;
        if r < self.rows && c < self.cols {
            let cell = &mut self.lines[r].cells[c];
            cell.ch = ch;
            cell.fg = self.pen.fg;
            cell.bg = self.pen.bg;
            cell.attrs = self.pen.attrs;
            cell.width = width;
            // For a wide leading, mark the next cell as a spacer
            // (width=0) carrying the same bg/fg so the bg paints
            // continuously across both cells. The spacer's ch is just
            // ' ' — the leading's glyph is what actually gets drawn,
            // spanning both cells in the renderer.
            if width == 2 && c + 1 < self.cols {
                let spacer = &mut self.lines[r].cells[c + 1];
                spacer.ch = ' ';
                spacer.fg = self.pen.fg;
                spacer.bg = self.pen.bg;
                spacer.attrs = self.pen.attrs;
                spacer.width = 0;
            }
        }

        let advance = width as usize;
        if c + advance >= self.cols {
            self.cursor.wrap_next = true;
        } else {
            self.cursor.col = c + advance;
        }
    }

    pub fn carriage_return(&mut self) {
        self.cursor.col = 0;
        self.cursor.wrap_next = false;
    }

    pub fn line_feed(&mut self) {
        self.cursor.wrap_next = false;
        if self.cursor.row == self.scroll_bot {
            self.scroll_up_in_region(1);
        } else if self.cursor.row + 1 < self.rows {
            self.cursor.row += 1;
        }
    }

    pub fn reverse_index(&mut self) {
        self.cursor.wrap_next = false;
        if self.cursor.row == self.scroll_top {
            self.scroll_down_in_region(1);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
        }
    }

    pub fn backspace(&mut self) {
        self.cursor.wrap_next = false;
        if self.cursor.col > 0 {
            self.cursor.col -= 1;
        }
    }

    pub fn tab(&mut self) {
        // Next 8-column tab stop, clamped to last column.
        let next = ((self.cursor.col / 8) + 1) * 8;
        self.cursor.col = next.min(self.cols.saturating_sub(1));
        self.cursor.wrap_next = false;
    }

    pub fn goto(&mut self, row: usize, col: usize) {
        self.cursor.row = row.min(self.rows.saturating_sub(1));
        self.cursor.col = col.min(self.cols.saturating_sub(1));
        self.cursor.wrap_next = false;
    }

    pub fn move_cursor(&mut self, drow: isize, dcol: isize) {
        let r = (self.cursor.row as isize + drow)
            .clamp(0, self.rows.saturating_sub(1) as isize) as usize;
        let c = (self.cursor.col as isize + dcol)
            .clamp(0, self.cols.saturating_sub(1) as isize) as usize;
        self.cursor.row = r;
        self.cursor.col = c;
        self.cursor.wrap_next = false;
    }

    /// Scroll the active region up by `n`, pushing displaced top lines into
    /// scrollback when the region covers the whole screen.
    pub fn scroll_up_in_region(&mut self, n: usize) {
        let n = n.min(self.scroll_bot - self.scroll_top + 1);
        let full_screen = self.scroll_top == 0 && self.scroll_bot == self.rows - 1;
        for _ in 0..n {
            if !full_screen {
                // Region scroll: rotate the [scroll_top..=scroll_bot]
                // sub-range. VecDeque doesn't have a slice rotate, so
                // do it by hand via swap pairs — same O(n) cost as
                // Vec's rotate_left.
                let blank = Row::blank(self.cols, self.pen);
                for i in self.scroll_top..self.scroll_bot {
                    self.lines.swap(i, i + 1);
                }
                self.lines[self.scroll_bot] = blank;
                continue;
            }
            if self.scrollback_limit == 0 {
                // Alt-screen scroll without history. Recycle: take the
                // top row, clear it, push to bottom. O(1) on VecDeque.
                let mut row = self.lines.pop_front().expect("rows > 0");
                row.clear_with(self.pen);
                self.lines.push_back(row);
                continue;
            }
            // Full-screen scroll with scrollback. Pop the top row off
            // the live grid (O(1) on VecDeque), push it onto scrollback,
            // and put a fresh blank at the bottom. Recycle the oldest
            // scrollback row when scrollback is full — saves an
            // alloc/free per scroll.
            let blank = if self.scrollback.len() >= self.scrollback_limit {
                let mut row = self.scrollback.pop_front().expect("len > 0");
                // `Grid::resize` doesn't touch scrollback, so a row
                // recycled across a width change needs reshaping.
                if row.cells.len() != self.cols {
                    row.cells.resize(self.cols, Cell::default());
                }
                row.clear_with(self.pen);
                row
            } else {
                Row::blank(self.cols, self.pen)
            };
            let old = self.lines.pop_front().expect("rows > 0");
            self.scrollback.push_back(old);
            self.lines.push_back(blank);
        }
    }

    pub fn scroll_down_in_region(&mut self, n: usize) {
        let n = n.min(self.scroll_bot - self.scroll_top + 1);
        for _ in 0..n {
            // VecDeque has no slice rotate_right; bubble in the other
            // direction by hand.
            for i in (self.scroll_top..self.scroll_bot).rev() {
                self.lines.swap(i, i + 1);
            }
            self.lines[self.scroll_top] = Row::blank(self.cols, self.pen);
        }
    }

    pub fn erase_line(&mut self, mode: u16) {
        let r = self.cursor.row;
        let c = self.cursor.col;
        let cell = self.pen.blank_cell();
        match mode {
            0 => {
                for cc in c..self.cols {
                    self.lines[r].cells[cc] = cell;
                }
            }
            1 => {
                for cc in 0..=c.min(self.cols - 1) {
                    self.lines[r].cells[cc] = cell;
                }
            }
            2 => self.lines[r].clear_with(self.pen),
            _ => {}
        }
    }

    pub fn erase_display(&mut self, mode: u16) {
        let r = self.cursor.row;
        match mode {
            0 => {
                self.erase_line(0);
                for rr in (r + 1)..self.rows {
                    self.lines[rr].clear_with(self.pen);
                }
            }
            1 => {
                for rr in 0..r {
                    self.lines[rr].clear_with(self.pen);
                }
                self.erase_line(1);
            }
            2 | 3 => {
                for row in &mut self.lines {
                    row.clear_with(self.pen);
                }
            }
            _ => {}
        }
    }

    pub fn erase_chars(&mut self, n: usize) {
        let r = self.cursor.row;
        let cell = self.pen.blank_cell();
        let end = (self.cursor.col + n).min(self.cols);
        for c in self.cursor.col..end {
            self.lines[r].cells[c] = cell;
        }
    }

    pub fn delete_chars(&mut self, n: usize) {
        let r = self.cursor.row;
        let c = self.cursor.col;
        let n = n.min(self.cols - c);
        let row = &mut self.lines[r].cells;
        row[c..].rotate_left(n);
        let blank = self.pen.blank_cell();
        for cc in (self.cols - n)..self.cols {
            row[cc] = blank;
        }
    }

    pub fn insert_chars(&mut self, n: usize) {
        let r = self.cursor.row;
        let c = self.cursor.col;
        let n = n.min(self.cols - c);
        let row = &mut self.lines[r].cells;
        row[c..].rotate_right(n);
        let blank = self.pen.blank_cell();
        for cc in c..(c + n) {
            row[cc] = blank;
        }
    }

    pub fn insert_lines(&mut self, n: usize) {
        if self.cursor.row < self.scroll_top || self.cursor.row > self.scroll_bot {
            return;
        }
        let n = n.min(self.scroll_bot - self.cursor.row + 1);
        for _ in 0..n {
            for i in (self.cursor.row..self.scroll_bot).rev() {
                self.lines.swap(i, i + 1);
            }
            self.lines[self.cursor.row] = Row::blank(self.cols, self.pen);
        }
    }

    pub fn delete_lines(&mut self, n: usize) {
        if self.cursor.row < self.scroll_top || self.cursor.row > self.scroll_bot {
            return;
        }
        let n = n.min(self.scroll_bot - self.cursor.row + 1);
        for _ in 0..n {
            for i in self.cursor.row..self.scroll_bot {
                self.lines.swap(i, i + 1);
            }
            self.lines[self.scroll_bot] = Row::blank(self.cols, self.pen);
        }
    }

    pub fn set_scroll_region(&mut self, top: usize, bot: usize) {
        if top < bot && bot < self.rows {
            self.scroll_top = top;
            self.scroll_bot = bot;
            self.cursor.row = 0;
            self.cursor.col = 0;
            self.cursor.wrap_next = false;
        }
    }

    pub fn save_cursor(&mut self) {
        self.saved = Some((self.cursor, self.pen));
    }

    pub fn restore_cursor(&mut self) {
        if let Some((cur, pen)) = self.saved {
            self.cursor = cur;
            self.pen = pen;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_width_ascii_is_single() {
        assert_eq!(char_width('A'), 1);
        assert_eq!(char_width(' '), 1);
        assert_eq!(char_width('5'), 1);
        assert_eq!(char_width('!'), 1);
    }

    #[test]
    fn char_width_latin_extended_stays_single() {
        // Accented Latin and Cyrillic are single-width in monospace.
        assert_eq!(char_width('é'), 1);
        assert_eq!(char_width('ñ'), 1);
        assert_eq!(char_width('Я'), 1);
    }

    #[test]
    fn char_width_cjk_unified_is_wide() {
        assert_eq!(char_width('中'), 2);
        assert_eq!(char_width('文'), 2);
        assert_eq!(char_width('日'), 2);
    }

    #[test]
    fn char_width_kana_and_hangul() {
        assert_eq!(char_width('あ'), 2); // Hiragana
        assert_eq!(char_width('カ'), 2); // Katakana
        assert_eq!(char_width('한'), 2); // Hangul
    }

    #[test]
    fn char_width_fullwidth_forms_are_wide() {
        // Fullwidth ASCII letters / digits.
        assert_eq!(char_width('Ａ'), 2);
        assert_eq!(char_width('１'), 2);
    }

    #[test]
    fn char_width_box_drawing_is_single() {
        // Box-drawing chars are EAW=N → width 1, even though they look
        // similar to wide chars culturally.
        assert_eq!(char_width('┌'), 1);
        assert_eq!(char_width('☐'), 1);
    }

    #[test]
    fn put_char_wide_advances_by_two_and_marks_spacer() {
        let mut g = Grid::new(3, 10, 0);
        // Pen carries SGR attrs / colors that should propagate to the
        // spacer too so the bg paints continuously.
        g.pen.fg = Color::Indexed(2);
        g.put_char('中');
        assert_eq!(g.cursor.col, 2);
        assert_eq!(g.lines[0].cells[0].ch, '中');
        assert_eq!(g.lines[0].cells[0].width, 2);
        assert_eq!(g.lines[0].cells[1].width, 0);
        assert_eq!(g.lines[0].cells[1].fg, Color::Indexed(2));
    }

    #[test]
    fn put_char_wide_at_right_edge_wraps_first() {
        let mut g = Grid::new(3, 5, 0);
        // Move cursor to last column.
        g.put_char('a');
        g.put_char('b');
        g.put_char('c');
        g.put_char('d');
        // Now cursor is at col=4 (the last cell). A wide char here
        // can't fit — should wrap to next row before placing.
        g.put_char('中');
        assert_eq!(g.cursor.row, 1);
        assert_eq!(g.cursor.col, 2);
        assert_eq!(g.lines[1].cells[0].ch, '中');
        assert_eq!(g.lines[1].cells[0].width, 2);
        assert_eq!(g.lines[1].cells[1].width, 0);
    }

    #[test]
    fn put_char_normal_followed_by_wide_lays_out_correctly() {
        let mut g = Grid::new(2, 10, 0);
        g.put_char('a');
        g.put_char('中');
        g.put_char('b');
        assert_eq!(g.lines[0].cells[0].ch, 'a');
        assert_eq!(g.lines[0].cells[0].width, 1);
        assert_eq!(g.lines[0].cells[1].ch, '中');
        assert_eq!(g.lines[0].cells[1].width, 2);
        assert_eq!(g.lines[0].cells[2].width, 0); // spacer
        assert_eq!(g.lines[0].cells[3].ch, 'b');
        assert_eq!(g.cursor.col, 4);
    }
}
