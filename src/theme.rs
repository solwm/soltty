use std::path::PathBuf;

/// One color theme. The 16 ANSI colors plus default fg/bg and cursor.
/// 256-color cube + greyscale (indices 16..=255) are derived from the
/// 16-color base via the standard xterm formula at render time.
#[derive(Clone, Debug)]
pub struct Theme {
    pub name: String,
    pub bg: [u8; 3],
    pub fg: [u8; 3],
    pub cursor_bg: [u8; 3],
    pub cursor_fg: [u8; 3],
    pub palette: [[u8; 3]; 16],
}

/// The static-data form used for vendored themes. Embedded by `build.rs`.
pub struct BuiltinTheme {
    pub name: &'static str,
    pub bg: [u8; 3],
    pub fg: [u8; 3],
    pub cursor_bg: [u8; 3],
    pub cursor_fg: [u8; 3],
    pub palette: [[u8; 3]; 16],
}

impl Theme {
    fn from_builtin(b: &BuiltinTheme) -> Self {
        Self {
            name: b.name.to_string(),
            bg: b.bg,
            fg: b.fg,
            cursor_bg: b.cursor_bg,
            cursor_fg: b.cursor_fg,
            palette: b.palette,
        }
    }
}

include!(concat!(env!("OUT_DIR"), "/themes_data.rs"));

pub const DEFAULT_THEME_NAME: &str = "Dracula";

pub struct ThemeLib {
    themes: Vec<Theme>,
}

impl ThemeLib {
    /// Builtins + anything in `~/.config/soltty/themes/*.toml`. User
    /// themes override builtins with the same (case-insensitive) name.
    pub fn load() -> Self {
        let mut themes: Vec<Theme> = BUILTIN_THEMES.iter().map(Theme::from_builtin).collect();

        if let Some(dir) = user_themes_dir() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().map(|e| e == "toml").unwrap_or(false) {
                        let name = path
                            .file_stem()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|src| parse_user_theme(&name, &src)) {
                            Ok(theme) => {
                                if let Some(slot) = themes.iter_mut().find(|t| {
                                    t.name.eq_ignore_ascii_case(&theme.name)
                                }) {
                                    *slot = theme;
                                } else {
                                    themes.push(theme);
                                }
                            }
                            Err(e) => log::warn!(
                                "skipping {}: {e}",
                                path.display()
                            ),
                        }
                    }
                }
            }
        }

        themes.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Self { themes }
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.themes.iter().map(|t| t.name.as_str())
    }

    #[allow(dead_code)]
    pub fn all(&self) -> &[Theme] {
        &self.themes
    }

    /// Look up a theme. Tries case-insensitive exact match first, then a
    /// case-insensitive substring match. With 500+ themes a query like
    /// `dracu` matches several (Dracula, Dracula+, Dracula Pro…), so we
    /// return the shortest match — the canonical short name is the one
    /// users almost always mean.
    pub fn find(&self, name: &str) -> Option<&Theme> {
        let needle = name.to_lowercase();
        if let Some(t) = self.themes.iter().find(|t| t.name.to_lowercase() == needle) {
            return Some(t);
        }
        self.themes
            .iter()
            .filter(|t| t.name.to_lowercase().contains(&needle))
            .min_by_key(|t| t.name.len())
    }

    pub fn default(&self) -> &Theme {
        self.find(DEFAULT_THEME_NAME)
            .or_else(|| self.themes.first())
            .expect("at least one theme")
    }

    pub fn position(&self, name: &str) -> Option<usize> {
        self.themes
            .iter()
            .position(|t| t.name.eq_ignore_ascii_case(name))
    }

    pub fn at(&self, idx: usize) -> &Theme {
        &self.themes[idx]
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.themes.len()
    }
}

fn user_themes_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let p = PathBuf::from(home).join(".config/soltty/themes");
    Some(p)
}

/// Runtime TOML-subset parser, mirrors the one in `build.rs`. Accepts the
/// same shape of file that vendored themes use.
fn parse_user_theme(name: &str, src: &str) -> Result<Theme, String> {
    let mut bg = None;
    let mut fg = None;
    let mut cursor_bg = None;
    let mut cursor_fg = None;
    let mut palette: [Option<[u8; 3]>; 16] = [None; 16];
    let mut section: Option<String> = None;

    for (lineno, raw) in src.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = Some(rest.to_string());
            continue;
        }
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| format!("line {}: missing `=`", lineno + 1))?;
        let k = k.trim();
        let v = v.trim().trim_matches(|c: char| c == '"' || c == '\'');
        let rgb = parse_hex(v)
            .ok_or_else(|| format!("line {}: bad color {v:?}", lineno + 1))?;
        match (section.as_deref().unwrap_or(""), k) {
            ("colors.primary", "background") => bg = Some(rgb),
            ("colors.primary", "foreground") => fg = Some(rgb),
            ("colors.cursor", "text") => cursor_fg = Some(rgb),
            ("colors.cursor", "cursor") => cursor_bg = Some(rgb),
            ("colors.normal", name) => {
                if let Some(i) = ansi_name(name, false) {
                    palette[i] = Some(rgb);
                }
            }
            ("colors.bright", name) => {
                if let Some(i) = ansi_name(name, true) {
                    palette[i] = Some(rgb);
                }
            }
            _ => {}
        }
    }

    let bg = bg.ok_or("missing colors.primary.background")?;
    let fg = fg.ok_or("missing colors.primary.foreground")?;
    let mut p = [[0u8; 3]; 16];
    for (i, c) in palette.iter().enumerate() {
        p[i] = c.ok_or_else(|| format!("missing palette index {i}"))?;
    }
    Ok(Theme {
        name: name.to_string(),
        bg,
        fg,
        cursor_bg: cursor_bg.unwrap_or(fg),
        cursor_fg: cursor_fg.unwrap_or(bg),
        palette: p,
    })
}

fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_basic = false;
    let mut in_literal = false;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' if !in_literal => in_basic = !in_basic,
            b'\'' if !in_basic => in_literal = !in_literal,
            b'#' if !in_basic && !in_literal => return &line[..i],
            _ => {}
        }
    }
    line
}

fn parse_hex(s: &str) -> Option<[u8; 3]> {
    let s = s.trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some([r, g, b])
}

fn ansi_name(name: &str, bright: bool) -> Option<usize> {
    let base = match name {
        "black" => 0,
        "red" => 1,
        "green" => 2,
        "yellow" => 3,
        "blue" => 4,
        "magenta" => 5,
        "cyan" => 6,
        "white" => 7,
        _ => return None,
    };
    Some(if bright { base + 8 } else { base })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_theme() {
        // Triple `#` delimiter so the `"#` sequences inside don't close the raw string.
        let src = r###"
[colors.primary]
background = "#000000"
foreground = "#ffffff"
[colors.normal]
black = "#111111"
red = "#222222"
green = "#333333"
yellow = "#444444"
blue = "#555555"
magenta = "#666666"
cyan = "#777777"
white = "#888888"
[colors.bright]
black = "#aaaaaa"
red = "#bbbbbb"
green = "#cccccc"
yellow = "#dddddd"
blue = "#eeeeee"
magenta = "#fefefe"
cyan = "#fdfdfd"
white = "#ffffff"
"###;
        let t = parse_user_theme("MyTheme", src).unwrap();
        assert_eq!(t.bg, [0, 0, 0]);
        assert_eq!(t.fg, [0xff, 0xff, 0xff]);
        assert_eq!(t.palette[0], [0x11, 0x11, 0x11]);
        assert_eq!(t.palette[15], [0xff, 0xff, 0xff]);
        // Cursor colors default to fg/bg when unspecified.
        assert_eq!(t.cursor_bg, t.fg);
        assert_eq!(t.cursor_fg, t.bg);
    }

    #[test]
    fn lib_finds_dracula() {
        let lib = ThemeLib::load();
        assert!(lib.find("Dracula").is_some());
        assert!(lib.find("DrAcUlA").is_some());
        assert!(lib.find("dracu").is_some()); // substring
    }

    #[test]
    fn lib_default_is_dracula() {
        let lib = ThemeLib::load();
        assert_eq!(lib.default().name.to_lowercase(), "dracula");
    }
}
