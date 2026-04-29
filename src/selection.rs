use crate::term::Term;

/// A live selection in viewport coordinates. `anchor` is where the user
/// pressed; `end` follows the mouse during drag and freezes on release.
/// Both stored unnormalized so we can tell which way the user dragged.
#[derive(Copy, Clone, Debug)]
pub struct Selection {
    pub anchor: (usize, usize),
    pub end: (usize, usize),
    /// True while the mouse button is still held.
    pub dragging: bool,
}

impl Selection {
    pub fn new(at: (usize, usize)) -> Self {
        Self {
            anchor: at,
            end: at,
            dragging: true,
        }
    }

    /// `(start, end)` in row-major order.
    pub fn normalized(&self) -> ((usize, usize), (usize, usize)) {
        if (self.anchor.0, self.anchor.1) <= (self.end.0, self.end.1) {
            (self.anchor, self.end)
        } else {
            (self.end, self.anchor)
        }
    }

    pub fn contains(&self, row: usize, col: usize) -> bool {
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
    fn word_bounds_handles_punctuation() {
        // We treat `is_word_char` as "non-whitespace" so paths and URLs
        // get selected as one unit.
        let row = cells("/usr/local/bin");
        assert_eq!(word_bounds_in_cells(&row, 0), (0, 13));
        assert_eq!(word_bounds_in_cells(&row, 13), (0, 13));
    }
}


/// Build a copy-able string from the cells covered by `sel`. Pulls rows
/// via `Term::viewport_row` so it works in scrollback too. Trims trailing
/// whitespace per line — copying the right-edge padding spaces is almost
/// never what users want.
pub fn extract_text(term: &Term, sel: &Selection) -> String {
    let ((sr, sc), (er, ec)) = sel.normalized();
    let cols = term.grid().cols;
    let mut out = String::new();
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
    out
}
