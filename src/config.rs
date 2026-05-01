//! User config file at `~/.config/soltty/soltty.conf` (TOML-subset).
//!
//! Precedence for any setting that has multiple sources:
//!
//!   CLI flag  >  env var  >  config file  >  built-in default
//!
//! Each subsystem reads its own settings (font.rs reads the font keys,
//! main.rs reads theme / vi_keybind / frame_hz, etc.). The Config
//! struct just holds the parsed values; all the precedence logic lives
//! at the call sites so the rules are visible where they matter.
//!
//! The parser is a tiny TOML subset — same shape as the one in
//! `build.rs` / `theme.rs`. We don't pull in the `toml` crate because
//! the surface area we need is small (sections, key=value, strings,
//! numbers, booleans, comments) and adding a 5k-line dep for it is
//! out of proportion.
//!
//! Unknown keys are silently ignored so users with a config from a
//! newer build can downgrade without the terminal refusing to start.

use std::path::{Path, PathBuf};

/// Sample config shipped alongside the binary. On first run we copy
/// this to `~/.config/soltty/soltty.conf` so the user has something to
/// edit instead of having to read the docs to know the keys exist.
/// Every key is commented out, so seeding doesn't change any
/// behavior — it's just a discoverability aid.
const DEFAULT_TEMPLATE: &str = include_str!("../docs/soltty.conf.example");

#[derive(Default, Debug, Clone)]
pub struct Config {
    pub theme: Option<String>,
    pub font: Option<PathBuf>,
    pub font_bold: Option<PathBuf>,
    pub font_italic: Option<PathBuf>,
    pub font_bold_italic: Option<PathBuf>,
    pub font_size: Option<f32>,
    pub frame_hz: Option<u32>,
    pub vi_keybind: Option<String>,
}

impl Config {
    /// Load `~/.config/soltty/soltty.conf`. Behavior:
    ///
    ///   - Missing file → seed the path from the bundled template
    ///     (`docs/soltty.conf.example`, every key commented out) and
    ///     return default Config. The file then exists for the user
    ///     to edit, but no behavior changes.
    ///   - Parse error → log a warning and return default. A bad
    ///     config shouldn't keep the terminal from starting.
    ///   - Seed failure (read-only home, etc.) → also just default.
    ///     The user can still create the file manually if they care.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        let src = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Err(e) = seed_default(&path) {
                    log::warn!(
                        "config: could not seed {}: {e}",
                        path.display()
                    );
                } else {
                    log::info!("config: seeded {}", path.display());
                }
                return Self::default();
            }
            Err(e) => {
                log::warn!("config: read {} failed: {e}", path.display());
                return Self::default();
            }
        };
        match parse(&src) {
            Ok(cfg) => {
                log::info!("config: loaded {}", path.display());
                cfg
            }
            Err(e) => {
                log::warn!("config: {} did not parse: {e}", path.display());
                Self::default()
            }
        }
    }
}

/// Write the bundled template to `path`, creating parent directories
/// as needed. Refuses to overwrite an existing file (shouldn't happen
/// since `load` only calls this on NotFound, but defensive).
fn seed_default(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, DEFAULT_TEMPLATE)
}

fn config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/soltty/soltty.conf"))
}

/// Parse a TOML-subset string into a `Config`. The subset:
///   - `[section]` headers, dotted-section addressing in keys
///   - `key = value` lines
///   - values: `"basic"` / `'literal'` strings, integers, floats,
///     booleans (`true` / `false`)
///   - `#`-comments (outside strings, both quote styles)
///   - blank lines and arbitrary whitespace
///
/// Unknown keys are skipped silently. Malformed lines (no `=`, bad
/// number) return Err. Unrecognised values get logged but don't abort
/// the load — only structural issues do.
pub fn parse(src: &str) -> Result<Config, String> {
    let mut cfg = Config::default();
    let mut section: Option<String> = None;

    for (lineno, raw) in src.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = Some(rest.trim().to_string());
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {}: missing `=`", lineno + 1))?;
        let key = key.trim();
        let value = value.trim();
        let qualified = match section.as_deref() {
            Some(s) => format!("{s}.{key}"),
            None => key.to_string(),
        };
        apply_key(&mut cfg, &qualified, value, lineno + 1)?;
    }
    Ok(cfg)
}

/// Set a single `Config` field from a `qualified.key` and raw
/// (un-stripped) value. Section-qualified and bare keys are both
/// accepted so the user can write either flat or sectioned TOML.
fn apply_key(cfg: &mut Config, qualified: &str, raw_value: &str, lineno: usize) -> Result<(), String> {
    let str_val = unquote(raw_value);
    match qualified {
        // Appearance
        "appearance.theme" | "theme" => cfg.theme = Some(str_val.to_string()),
        "appearance.font" | "font" => cfg.font = Some(PathBuf::from(str_val)),
        "appearance.font_bold" | "font_bold" => cfg.font_bold = Some(PathBuf::from(str_val)),
        "appearance.font_italic" | "font_italic" => {
            cfg.font_italic = Some(PathBuf::from(str_val));
        }
        "appearance.font_bold_italic" | "font_bold_italic" => {
            cfg.font_bold_italic = Some(PathBuf::from(str_val));
        }
        "appearance.font_size" | "font_size" => {
            cfg.font_size = Some(parse_f32(raw_value, lineno, "font_size")?);
        }
        // Behavior
        "behavior.frame_hz" | "frame_hz" => {
            cfg.frame_hz = Some(parse_u32(raw_value, lineno, "frame_hz")?);
        }
        // Keybinds
        "keybinds.vi_mode" | "keybinds.vi_keybind" | "vi_mode" | "vi_keybind" => {
            cfg.vi_keybind = Some(str_val.to_string());
        }
        // Anything else is ignored on purpose: lets a user keep a
        // config from a newer build of soltty without rolling back.
        _ => {
            log::debug!("config: ignoring unknown key {:?}", qualified);
        }
    }
    Ok(())
}

fn parse_f32(raw: &str, lineno: usize, name: &str) -> Result<f32, String> {
    let unquoted = unquote(raw);
    unquoted
        .parse::<f32>()
        .map_err(|e| format!("line {lineno}: bad {name}: {e}"))
}

fn parse_u32(raw: &str, lineno: usize, name: &str) -> Result<u32, String> {
    let unquoted = unquote(raw);
    unquoted
        .parse::<u32>()
        .map_err(|e| format!("line {lineno}: bad {name}: {e}"))
}

/// Strip surrounding quotes from a TOML string value. Accepts both
/// basic (`"..."`) and literal (`'...'`) strings; non-string values
/// pass through unchanged.
fn unquote(value: &str) -> &str {
    let trimmed = value.trim();
    if (trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2)
        || (trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2)
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    }
}

/// Remove the trailing `# ...` from a line, but only if the `#` is
/// outside any string. Same logic as the TOML parser in `theme.rs`
/// and `build.rs` — needed because some config values (paths, keybind
/// strings) may legitimately contain `#`.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_yields_default_config() {
        let c = parse("").unwrap();
        assert!(c.theme.is_none() && c.font.is_none() && c.vi_keybind.is_none());
    }

    #[test]
    fn flat_keys_work() {
        let src = "theme = \"Solarized Dark\"\nvi_mode = 'ctrl+n'\n";
        let c = parse(src).unwrap();
        assert_eq!(c.theme.as_deref(), Some("Solarized Dark"));
        assert_eq!(c.vi_keybind.as_deref(), Some("ctrl+n"));
    }

    #[test]
    fn sectioned_keys_work() {
        let src = "[appearance]\ntheme = \"Dracula\"\nfont_size = 14.5\n\n[keybinds]\nvi_mode = \"alt+v\"\n";
        let c = parse(src).unwrap();
        assert_eq!(c.theme.as_deref(), Some("Dracula"));
        assert_eq!(c.font_size, Some(14.5));
        assert_eq!(c.vi_keybind.as_deref(), Some("alt+v"));
    }

    #[test]
    fn comments_and_blank_lines_skipped() {
        let src = "# top-level comment\n\n[appearance]  # trailing\ntheme = \"X\"   # inline\n";
        let c = parse(src).unwrap();
        assert_eq!(c.theme.as_deref(), Some("X"));
    }

    #[test]
    fn hash_inside_string_not_a_comment() {
        // Some font paths or keybinds could legitimately contain `#`.
        let src = "[appearance]\nfont = \"/x/foo#bar.ttf\"\n";
        let c = parse(src).unwrap();
        assert_eq!(
            c.font.as_deref().and_then(|p| p.to_str()),
            Some("/x/foo#bar.ttf")
        );
    }

    #[test]
    fn unknown_keys_are_silently_ignored() {
        // Forward-compat: user with a config from a future build can
        // downgrade without any change keeping the terminal from
        // starting.
        let src = "[xyz]\nsomething = 1\n[appearance]\ntheme = \"Dracula\"\n";
        let c = parse(src).unwrap();
        assert_eq!(c.theme.as_deref(), Some("Dracula"));
    }

    #[test]
    fn missing_equals_returns_err() {
        // Structural problems abort the parse so the user can fix it.
        let src = "[x]\nbad line\n";
        let err = parse(src).unwrap_err();
        assert!(err.contains("line 2"));
    }

    #[test]
    fn bad_number_returns_err() {
        let src = "font_size = oops\n";
        let err = parse(src).unwrap_err();
        assert!(err.contains("font_size"));
    }

    #[test]
    fn frame_hz_parses() {
        let src = "[behavior]\nframe_hz = 240\n";
        let c = parse(src).unwrap();
        assert_eq!(c.frame_hz, Some(240));
    }

    #[test]
    fn seed_default_writes_template_when_missing() {
        let dir = std::env::temp_dir().join(format!(
            "soltty-seed-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("soltty.conf");
        seed_default(&path).unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("[appearance]"),
            "seeded file should include the [appearance] section header"
        );
        assert!(
            written.contains("# theme = "),
            "seeded keys should be commented out — first-run shouldn't change behavior"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_default_does_not_overwrite_existing_file() {
        let dir = std::env::temp_dir().join(format!(
            "soltty-seed-noclobber-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("soltty.conf");
        std::fs::write(&path, "user content").unwrap();
        seed_default(&path).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after, "user content", "seed must not clobber");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn font_paths_parse_per_variant() {
        let src = "[appearance]\nfont = \"/a/Regular.ttf\"\nfont_bold = \"/a/Bold.ttf\"\nfont_italic = '/a/Italic.ttf'\nfont_bold_italic = '/a/BoldItalic.ttf'\n";
        let c = parse(src).unwrap();
        assert_eq!(c.font.as_deref().and_then(|p| p.to_str()), Some("/a/Regular.ttf"));
        assert_eq!(c.font_bold.as_deref().and_then(|p| p.to_str()), Some("/a/Bold.ttf"));
        assert_eq!(c.font_italic.as_deref().and_then(|p| p.to_str()), Some("/a/Italic.ttf"));
        assert_eq!(c.font_bold_italic.as_deref().and_then(|p| p.to_str()), Some("/a/BoldItalic.ttf"));
    }
}
