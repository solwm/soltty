use std::collections::VecDeque;

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
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            attrs: CellAttrs::default(),
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

#[derive(Copy, Clone, Debug, Default)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub visible: bool,
    /// Pending wrap: the cursor is at (row, cols) and the next print should wrap.
    /// Standard xterm "cursor stickiness" — printing at the right edge does NOT
    /// wrap until the next character actually arrives.
    pub wrap_next: bool,
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
        }
    }
}

pub struct Grid {
    pub rows: usize,
    pub cols: usize,
    pub lines: Vec<Row>,
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
        let lines = (0..rows).map(|_| Row::blank(cols, pen)).collect();
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
                    self.lines.push(Row::blank(cols, self.pen));
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

    pub fn put_char(&mut self, ch: char) {
        if self.cursor.wrap_next {
            self.cursor.col = 0;
            self.line_feed();
            self.cursor.wrap_next = false;
        }
        let r = self.cursor.row;
        let c = self.cursor.col;
        if r < self.rows && c < self.cols {
            let cell = &mut self.lines[r].cells[c];
            cell.ch = ch;
            cell.fg = self.pen.fg;
            cell.bg = self.pen.bg;
            cell.attrs = self.pen.attrs;
        }
        if c + 1 >= self.cols {
            self.cursor.wrap_next = true;
        } else {
            self.cursor.col = c + 1;
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
            if full_screen {
                let row = std::mem::replace(&mut self.lines[0], Row::blank(self.cols, self.pen));
                self.scrollback.push_back(row);
                while self.scrollback.len() > self.scrollback_limit {
                    self.scrollback.pop_front();
                }
                self.lines.rotate_left(1);
            } else {
                let blank = Row::blank(self.cols, self.pen);
                self.lines[self.scroll_top..=self.scroll_bot].rotate_left(1);
                self.lines[self.scroll_bot] = blank;
            }
        }
    }

    pub fn scroll_down_in_region(&mut self, n: usize) {
        let n = n.min(self.scroll_bot - self.scroll_top + 1);
        for _ in 0..n {
            self.lines[self.scroll_top..=self.scroll_bot].rotate_right(1);
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
            self.lines[self.cursor.row..=self.scroll_bot].rotate_right(1);
            self.lines[self.cursor.row] = Row::blank(self.cols, self.pen);
        }
    }

    pub fn delete_lines(&mut self, n: usize) {
        if self.cursor.row < self.scroll_top || self.cursor.row > self.scroll_bot {
            return;
        }
        let n = n.min(self.scroll_bot - self.cursor.row + 1);
        for _ in 0..n {
            self.lines[self.cursor.row..=self.scroll_bot].rotate_left(1);
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
