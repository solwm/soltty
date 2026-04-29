use crate::theme::{Theme, ThemeLib};

/// Live-preview theme picker. While open, renders an overlay listing all
/// available themes; pressing Up/Down navigates and triggers a live theme
/// reload so the cursor's selected theme is what you actually see. Enter
/// commits the selection; Esc reverts to whatever theme was active when
/// the picker was opened.
pub struct Picker {
    themes: Vec<String>,
    /// Currently highlighted index.
    cursor: usize,
    /// Top of the visible list (for scrolling when there are more themes
    /// than rows in the modal).
    scroll: usize,
    /// Theme that was active when the picker opened, restored on cancel.
    original_index: usize,
}

impl Picker {
    pub fn new(lib: &ThemeLib, current_name: &str) -> Self {
        let names: Vec<String> = lib.names().map(|s| s.to_string()).collect();
        let original_index = lib.position(current_name).unwrap_or(0);
        Self {
            themes: names,
            cursor: original_index,
            scroll: 0,
            original_index,
        }
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Restore the original theme. Caller is responsible for closing the
    /// picker after calling this.
    pub fn cancel<'a>(&self, lib: &'a ThemeLib) -> &'a Theme {
        lib.at(self.original_index)
    }

    pub fn current<'a>(&self, lib: &'a ThemeLib) -> &'a Theme {
        lib.at(self.cursor)
    }

    pub fn move_up(&mut self) {
        if self.cursor == 0 {
            self.cursor = self.themes.len() - 1;
        } else {
            self.cursor -= 1;
        }
    }

    pub fn move_down(&mut self) {
        self.cursor = (self.cursor + 1) % self.themes.len();
    }

    pub fn page_up(&mut self, n: usize) {
        self.cursor = self.cursor.saturating_sub(n);
    }

    pub fn page_down(&mut self, n: usize) {
        self.cursor = (self.cursor + n).min(self.themes.len() - 1);
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.themes.len() - 1;
    }

    /// Compute the layout of the picker overlay for a given grid size.
    /// Returns `(box_origin, box_size, list_rows_visible)`.
    /// All in cell coords. Caller is responsible for actually drawing.
    pub fn layout(&mut self, grid_rows: usize, grid_cols: usize) -> Layout {
        // Modal: 40 cols × min(grid_rows-4, num_themes+3) rows.
        // +3 = top border, header, bottom border.
        let width = 40.min(grid_cols.saturating_sub(2));
        let max_height = grid_rows.saturating_sub(4);
        let list_rows = (self.themes.len() + 3).min(max_height);
        let height = list_rows.max(5); // pre-empt division by zero etc.
        let origin_col = grid_cols.saturating_sub(width) / 2;
        let origin_row = grid_rows.saturating_sub(height) / 2;
        let visible = height.saturating_sub(3); // minus borders+header

        // Adjust scroll so cursor stays in view.
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + visible {
            self.scroll = self.cursor + 1 - visible;
        }

        Layout {
            origin: (origin_row, origin_col),
            size: (height, width),
            list_visible: visible,
            scroll: self.scroll,
        }
    }

    pub fn themes(&self) -> &[String] {
        &self.themes
    }
}

/// Geometry chosen for one frame of the picker overlay.
pub struct Layout {
    pub origin: (usize, usize), // (row, col) in grid coords
    pub size: (usize, usize),   // (height, width) in cells
    pub list_visible: usize,    // how many list rows fit between borders
    pub scroll: usize,          // first visible list index
}
