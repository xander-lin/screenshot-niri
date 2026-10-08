use std::path::PathBuf;

/// Hardcoded default long-capture frame-rate cap (fps).
pub const DEFAULT_MAX_FRAME_RATE: u32 = 30;
/// Hardcoded default selection-border thickness in logical pixels.
pub const DEFAULT_BORDER_WIDTH: i32 = 3;
/// Upper bound accepted for `--fps`.
pub const MAX_FRAME_RATE_LIMIT: u32 = 240;

/// User configuration loaded from `$XDG_CONFIG_HOME/screenshot/config`
/// (default `~/.config/screenshot/config`). Simple `key = value` lines,
/// `#` comments; unknown keys and invalid values fall back to defaults so a
/// hand-edited file can never break the tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Long-capture frame-rate cap used when `--fps` is not given.
    pub max_fps: u32,
    /// Visible selection-border thickness in logical pixels.
    pub border_width: i32,
}

pub const BORDER_WIDTH_MIN: i32 = 1;
pub const BORDER_WIDTH_MAX: i32 = 16;

impl Default for Config {
    fn default() -> Self {
        Self {
            max_fps: DEFAULT_MAX_FRAME_RATE,
            border_width: DEFAULT_BORDER_WIDTH,
        }
    }
}

impl Config {
    /// Loads the config file, falling back to defaults when it is missing.
    pub fn load() -> Self {
        Self::load_from(&config_path()).unwrap_or_default()
    }

    fn load_from(path: &std::path::Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        Some(Self::parse(&text))
    }

    /// Parses config text. Invalid or unknown entries keep their defaults.
    pub fn parse(text: &str) -> Self {
        let mut config = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            match key {
                "max_fps" => {
                    if let Ok(fps) = value.parse::<u32>() {
                        if (1..=MAX_FRAME_RATE_LIMIT).contains(&fps) {
                            config.max_fps = fps;
                        }
                    }
                }
                "border_width" => {
                    if let Ok(width) = value.parse::<i32>() {
                        if (BORDER_WIDTH_MIN..=BORDER_WIDTH_MAX).contains(&width) {
                            config.border_width = width;
                        }
                    }
                }
                _ => {}
            }
        }
        config
    }
}

fn config_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(dir).join("screenshot").join("config");
    }
    let home = std::env::var_os("HOME").filter(|value| !value.is_empty()).unwrap_or_default();
    PathBuf::from(home).join(".config").join("screenshot").join("config")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_apply_without_a_file() {
        assert_eq!(Config::parse(""), Config::default());
        assert_eq!(Config::default().max_fps, 30);
        assert_eq!(Config::default().border_width, 3);
    }

    #[test]
    fn parses_known_keys_with_comments_and_padding() {
        let config = Config::parse(
            "# screenshot config\n\n  max_fps = 15\nborder_width=5\nunknown = thing\n",
        );
        assert_eq!(config.max_fps, 15);
        assert_eq!(config.border_width, 5);
    }

    #[test]
    fn out_of_range_values_keep_defaults() {
        let config = Config::parse("max_fps = 0\nborder_width = 99\n");
        assert_eq!(config, Config::default());
        let config = Config::parse("max_fps = 241\nborder_width = 0\n");
        assert_eq!(config, Config::default());
    }

    #[test]
    fn non_numeric_and_garbage_lines_keep_defaults() {
        let config = Config::parse("max_fps = fast\nborder_width\n= 3\nno equals\n");
        assert_eq!(config, Config::default());
    }
}
