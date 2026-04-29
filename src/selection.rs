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
