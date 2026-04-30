use crate::term::Term;

/// What kind of region the selection covers. `Char` is the row-major
/// run between anchor and end (mouse drag, vim `v`, vim `V` via the
/// full-row trick). `Block` is a true rectangle — anchor and end are
/// opposite corners, every cell between them is selected (vim `Ctrl-v`).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum SelectionMode {
    #[default]
    Char,
    Block,
}

/// A live selection in viewport coordinates. `anchor` is where the user
/// pressed; `end` follows the mouse during drag and freezes on release.
/// Both stored unnormalized so we can tell which way the user dragged.
#[derive(Copy, Clone, Debug)]
pub struct Selection {
    pub anchor: (usize, usize),
    pub end: (usize, usize),
    /// True while the mouse button is still held.
    pub dragging: bool,
    pub mode: SelectionMode,
}

impl Selection {
    pub fn new(at: (usize, usize)) -> Self {
        Self {
            anchor: at,
            end: at,
            dragging: true,
            mode: SelectionMode::Char,
        }
    }

    /// `(start, end)` in row-major order. Only meaningful for
    /// `SelectionMode::Char` — Block mode uses per-axis min/max
    /// directly via `block_bounds`.
    pub fn normalized(&self) -> ((usize, usize), (usize, usize)) {
        if (self.anchor.0, self.anchor.1) <= (self.end.0, self.end.1) {
            (self.anchor, self.end)
        } else {
            (self.end, self.anchor)
        }
    }

    /// Per-axis bounds for a block selection: `((r1, c1), (r2, c2))`
    /// where r1 <= r2 and c1 <= c2.
    pub fn block_bounds(&self) -> ((usize, usize), (usize, usize)) {
        let r1 = self.anchor.0.min(self.end.0);
        let r2 = self.anchor.0.max(self.end.0);
        let c1 = self.anchor.1.min(self.end.1);
        let c2 = self.anchor.1.max(self.end.1);
        ((r1, c1), (r2, c2))
    }

    pub fn contains(&self, row: usize, col: usize) -> bool {
        match self.mode {
            SelectionMode::Char => {
                let ((sr, sc), (er, ec)) = self.normalized();
                if row < sr || row > er {
                    return false;
                }
                if sr == er {
                    col >= sc && col <= ec
                } else if row == sr {
                    col >= sc
                } else if row == er {
                    col <= ec
                } else {
                    true
                }
            }
            SelectionMode::Block => {
                let ((r1, c1), (r2, c2)) = self.block_bounds();
                row >= r1 && row <= r2 && col >= c1 && col <= c2
            }
        }
    }

    /// Empty/zero-length: anchor and end are at the same cell. We treat
    /// that as "no selection" for clipboard purposes (a stray click
    /// shouldn't clobber the system clipboard).
    pub fn is_empty(&self) -> bool {
        self.anchor == self.end
    }
}

/// Find the bounds of the "word" containing `(row, col)` in the viewport.
/// A word is a run of contiguous non-whitespace characters. Returns
/// `((row, start_col), (row, end_col_inclusive))`. If the click landed on
/// whitespace, returns just that one cell (no expansion).
pub fn word_bounds(term: &Term, row: usize, col: usize) -> ((usize, usize), (usize, usize)) {
    let cells = &term.viewport_row(row).cells;
    let cols = term.grid().cols;
    if col >= cells.len() || !is_word_char(cells[col].ch) {
        return ((row, col), (row, col));
    }
    let mut start = col;
    while start > 0 && is_word_char(cells[start - 1].ch) {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < cols.min(cells.len()) && is_word_char(cells[end + 1].ch) {
        end += 1;
    }
    ((row, start), (row, end))
}

/// Bounds of the entire row at `row`. Used for triple-click line select.
pub fn line_bounds(term: &Term, row: usize) -> ((usize, usize), (usize, usize)) {
    let cols = term.grid().cols.saturating_sub(1);
    ((row, 0), (row, cols))
}

fn is_word_char(c: char) -> bool {
    !c.is_whitespace() && c != '\0'
}

/// Helper used by tests so we don't have to spin up a full Term.
#[cfg(test)]
fn word_bounds_in_cells(cells: &[crate::grid::Cell], col: usize) -> (usize, usize) {
    if col >= cells.len() || !is_word_char(cells[col].ch) {
        return (col, col);
    }
    let mut s = col;
    while s > 0 && is_word_char(cells[s - 1].ch) {
        s -= 1;
    }
    let mut e = col;
    while e + 1 < cells.len() && is_word_char(cells[e + 1].ch) {
        e += 1;
    }
    (s, e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Cell;

    fn cells(s: &str) -> Vec<Cell> {
        s.chars()
            .map(|c| Cell {
                ch: c,
                ..Cell::default()
            })
            .collect()
    }

    #[test]
    fn word_bounds_finds_run() {
        let row = cells("hello world");
        assert_eq!(word_bounds_in_cells(&row, 0), (0, 4));
        assert_eq!(word_bounds_in_cells(&row, 4), (0, 4));
        assert_eq!(word_bounds_in_cells(&row, 6), (6, 10));
        // Click on the space — doesn't expand.
        assert_eq!(word_bounds_in_cells(&row, 5), (5, 5));
    }

    #[test]
    fn char_mode_contains_row_major() {
        let mut sel = Selection::new((1, 3));
        sel.end = (3, 5);
        sel.dragging = false;
        // Inside the row-major run.
        assert!(sel.contains(1, 3));
        assert!(sel.contains(2, 0));   // middle row, all cols
        assert!(sel.contains(3, 5));
        // Char mode: cells in cols 0-2 of row 1 are NOT included
        // (selection starts at col 3 of row 1).
        assert!(!sel.contains(1, 2));
        // After the end.
        assert!(!sel.contains(3, 6));
    }

    #[test]
    fn block_mode_contains_rect() {
        let mut sel = Selection::new((1, 3));
        sel.end = (3, 5);
        sel.mode = SelectionMode::Block;
        sel.dragging = false;
        // Inside the rect.
        assert!(sel.contains(1, 3));
        assert!(sel.contains(2, 4));
        assert!(sel.contains(3, 5));
        // Block mode: row 2 cols 0-2 are OUTSIDE the rect, even though
        // Char mode would have included them.
        assert!(!sel.contains(2, 0));
        assert!(!sel.contains(2, 2));
        assert!(!sel.contains(2, 6));
        // Above / below the rect.
        assert!(!sel.contains(0, 4));
        assert!(!sel.contains(4, 4));
    }

    #[test]
    fn block_mode_normalises_corners() {
        // Anchor bottom-right, end top-left → still a rect from
        // top-left to bottom-right.
        let mut sel = Selection::new((5, 9));
        sel.end = (2, 4);
        sel.mode = SelectionMode::Block;
        let ((r1, c1), (r2, c2)) = sel.block_bounds();
        assert_eq!((r1, c1), (2, 4));
        assert_eq!((r2, c2), (5, 9));
        assert!(sel.contains(3, 6));
        assert!(!sel.contains(3, 3));
    }

    #[test]
    fn word_bounds_handles_punctuation() {
        // We treat `is_word_char` as "non-whitespace" so paths and URLs
        // get selected as one unit.
        let row = cells("/usr/local/bin");
        assert_eq!(word_bounds_in_cells(&row, 0), (0, 13));
        assert_eq!(word_bounds_in_cells(&row, 13), (0, 13));
    }

    #[test]
    fn block_extract_keeps_columns() {
        use crate::term::Term;
        let mut t = Term::new(4, 20);
        t.feed(b"abcdefghij\r\nklmnopqrst\r\nuvwxyz1234");
        // Rect from (0, 2) to (2, 5) — cols c..f on each of three rows.
        let mut sel = Selection::new((0, 2));
        sel.end = (2, 5);
        sel.mode = SelectionMode::Block;
        sel.dragging = false;
        let text = extract_text(&t, &sel);
        assert_eq!(text, "cdef\nmnop\nwxyz");
    }

    #[test]
    fn block_extract_preserves_internal_whitespace() {
        use crate::term::Term;
        let mut t = Term::new(2, 20);
        t.feed(b"aa  bb\r\ncc  dd");
        let mut sel = Selection::new((0, 1));
        sel.end = (1, 5);
        sel.mode = SelectionMode::Block;
        let text = extract_text(&t, &sel);
        // Char mode would trim trailing whitespace per line; Block keeps
        // the column-aligned spaces because the user explicitly picked
        // those columns.
        assert_eq!(text, "a  bb\nc  dd");
    }
}


/// Build a copy-able string from the cells covered by `sel`. Pulls rows
/// via `Term::viewport_row` so it works in scrollback too. Char-mode
/// trims trailing whitespace per line (copying the right-edge padding
/// spaces is almost never what users want); Block-mode preserves
/// trailing whitespace inside the rect because the user explicitly
/// selected that column range and removing pad would break alignment.
pub fn extract_text(term: &Term, sel: &Selection) -> String {
    let cols = term.grid().cols;
    let mut out = String::new();
    match sel.mode {
        SelectionMode::Char => {
            let ((sr, sc), (er, ec)) = sel.normalized();
            for row in sr..=er {
                let r = term.viewport_row(row);
                let start = if row == sr { sc } else { 0 };
                let end = if row == er {
                    (ec + 1).min(cols)
                } else {
                    cols
                };
                let line: String = r.cells[start..end].iter().map(|c| c.ch).collect();
                out.push_str(line.trim_end());
                if row < er {
                    out.push('\n');
                }
            }
        }
        SelectionMode::Block => {
            let ((r1, c1), (r2, c2)) = sel.block_bounds();
            let end = (c2 + 1).min(cols);
            for row in r1..=r2 {
                let r = term.viewport_row(row);
                let line: String = r.cells[c1..end].iter().map(|c| c.ch).collect();
                out.push_str(&line);
                if row < r2 {
                    out.push('\n');
                }
            }
        }
    }
    out
}
