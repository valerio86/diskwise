//! Colours, chosen once at startup from what the terminal supports.
//!
//! Modern terminals get exact colours; 256-colour terminals an indexed
//! approximation; basic ones the 16 standard colours; and `NO_COLOR` (or
//! `--color none`) gets no colour at all, with selection and bars drawn using
//! inverse video and block characters instead.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorMode {
    /// Detect from the environment.
    Auto,
    /// 24-bit colour.
    Truecolor,
    /// The 256-colour palette.
    #[value(name = "256")]
    Indexed256,
    /// The 16 standard colours.
    #[value(name = "16")]
    Basic16,
    /// No colour.
    None,
}

pub struct Theme {
    pub mode: ColorMode,
    pub accent: Color,
    pub on_accent: Color,
    pub track: Color,
    pub muted: Color,
    pub rule: Color,
    pub selected_bg: Color,
    pub selected_fg: Color,
    pub safe: Color,
    pub caution: Color,
    pub review: Color,
}

static THEME: OnceLock<Theme> = OnceLock::new();

/// Pick the theme. Call once before drawing; later calls are ignored.
pub fn init(mode: ColorMode) {
    let mode = match mode {
        ColorMode::Auto => detect(|k| std::env::var(k).ok()),
        m => m,
    };
    let _ = THEME.set(Theme::for_mode(mode));
}

/// The active theme (256 colours if `init` was never called, e.g. in tests).
pub fn th() -> &'static Theme {
    THEME.get_or_init(|| Theme::for_mode(ColorMode::Indexed256))
}

/// Decide the colour mode from environment variables (passed in for testing).
pub fn detect(env: impl Fn(&str) -> Option<String>) -> ColorMode {
    let set = |k: &str| env(k).is_some_and(|v| !v.is_empty());
    if set("NO_COLOR") {
        return ColorMode::None;
    }
    let colorterm = env("COLORTERM").unwrap_or_default().to_ascii_lowercase();
    if colorterm == "truecolor" || colorterm == "24bit" {
        return ColorMode::Truecolor;
    }
    let term = env("TERM").unwrap_or_default();
    if term == "dumb" {
        return ColorMode::None;
    }
    // Terminal apps known to handle 256 colours even when TERM says plain "xterm".
    let program = env("TERM_PROGRAM").unwrap_or_default();
    if term.contains("256color")
        || matches!(
            program.as_str(),
            "Apple_Terminal" | "iTerm.app" | "vscode" | "WezTerm" | "ghostty" | "tmux"
        )
    {
        return ColorMode::Indexed256;
    }
    ColorMode::Basic16
}

impl Theme {
    pub fn for_mode(mode: ColorMode) -> Theme {
        match mode {
            // Close to Claude Code's own lavender-on-slate.
            ColorMode::Truecolor | ColorMode::Auto => Theme {
                mode,
                accent: Color::Rgb(177, 185, 249),
                on_accent: Color::Rgb(22, 24, 48),
                track: Color::Rgb(78, 82, 112),
                muted: Color::Rgb(140, 140, 146),
                rule: Color::Rgb(84, 84, 92),
                selected_bg: Color::Rgb(52, 53, 66),
                selected_fg: Color::Rgb(242, 242, 246),
                safe: Color::Rgb(126, 204, 126),
                caution: Color::Rgb(240, 200, 96),
                review: Color::Rgb(214, 140, 214),
            },
            ColorMode::Indexed256 => Theme {
                mode,
                accent: Color::Indexed(147),
                on_accent: Color::Indexed(17),
                track: Color::Indexed(60),
                muted: Color::Indexed(245),
                rule: Color::Indexed(240),
                selected_bg: Color::Indexed(237),
                selected_fg: Color::Indexed(255),
                safe: Color::Indexed(114),
                caution: Color::Indexed(221),
                review: Color::Indexed(176),
            },
            ColorMode::Basic16 => Theme {
                mode,
                accent: Color::LightBlue,
                on_accent: Color::Black,
                track: Color::DarkGray,
                muted: Color::DarkGray,
                rule: Color::DarkGray,
                selected_bg: Color::Blue,
                selected_fg: Color::White,
                safe: Color::Green,
                caution: Color::Yellow,
                review: Color::Magenta,
            },
            ColorMode::None => Theme {
                mode,
                accent: Color::Reset,
                on_accent: Color::Reset,
                track: Color::Reset,
                muted: Color::Reset,
                rule: Color::Reset,
                selected_bg: Color::Reset,
                selected_fg: Color::Reset,
                safe: Color::Reset,
                caution: Color::Reset,
                review: Color::Reset,
            },
        }
    }

    pub fn has_color(&self) -> bool {
        self.mode != ColorMode::None
    }

    /// Base style for the selected row: cells draw on top, so their own colours
    /// (bars, safety tags) survive, and text without a colour gets `selected_fg`,
    /// which stays readable on light and dark terminals alike.
    pub fn selected(&self) -> Style {
        if self.has_color() {
            Style::new()
                .bg(self.selected_bg)
                .fg(self.selected_fg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        }
    }

    /// The active tab and key "pills": filled with `bg`.
    pub fn filled(&self, bg: Color) -> Style {
        if self.has_color() {
            Style::new()
                .fg(self.on_accent)
                .bg(bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn detection() {
        assert_eq!(
            detect(env(&[
                ("COLORTERM", "truecolor"),
                ("TERM", "xterm-256color")
            ])),
            ColorMode::Truecolor
        );
        assert_eq!(
            detect(env(&[("TERM", "xterm-256color")])),
            ColorMode::Indexed256
        );
        // macOS Terminal often reports plain "xterm" but supports 256 colours.
        assert_eq!(
            detect(env(&[
                ("TERM", "xterm"),
                ("TERM_PROGRAM", "Apple_Terminal")
            ])),
            ColorMode::Indexed256
        );
        assert_eq!(detect(env(&[("TERM", "xterm")])), ColorMode::Basic16);
        assert_eq!(detect(env(&[("TERM", "linux")])), ColorMode::Basic16);
        assert_eq!(detect(env(&[("TERM", "dumb")])), ColorMode::None);
        assert_eq!(
            detect(env(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")])),
            ColorMode::None
        );
        // An empty NO_COLOR doesn't count (per no-color.org).
        assert_eq!(
            detect(env(&[("NO_COLOR", ""), ("TERM", "xterm-256color")])),
            ColorMode::Indexed256
        );
    }
}
