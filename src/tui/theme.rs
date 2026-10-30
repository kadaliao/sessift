//! Colours. 256-colour indices so the palette looks the same in any modern terminal while
//! keeping the terminal's own background.

use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Indexed(173);
pub const MUTED: Color = Color::Indexed(245);
pub const FAINT: Color = Color::Indexed(240);
pub const SELECTED_BG: Color = Color::Indexed(236);
pub const OK: Color = Color::Indexed(108);
pub const WARN: Color = Color::Indexed(179);
pub const ERR: Color = Color::Indexed(167);
pub const CODE: Color = Color::Indexed(110);
pub const USER: Color = Color::Indexed(108);
pub const ASSISTANT: Color = Color::Indexed(110);
pub const TOOL: Color = Color::Indexed(179);

pub fn muted() -> Style {
    Style::default().fg(MUTED)
}

pub fn faint() -> Style {
    Style::default().fg(FAINT)
}

pub fn accent() -> Style {
    Style::default().fg(ACCENT)
}

pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

pub fn highlight() -> Style {
    Style::default()
        .fg(ACCENT)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
}

pub fn source_color(source: &str) -> Color {
    match source {
        "claude" => Color::Indexed(173),
        "codex" => Color::Indexed(110),
        "pi" | "omp" => Color::Indexed(140),
        "opencode" => Color::Indexed(108),
        "grok" => Color::Indexed(179),
        "cursor" => Color::Indexed(176),
        "copilot" | "antigravity" => Color::Indexed(75),
        "kimi" | "kilocode" => Color::Indexed(117),
        "hermes" | "goose" => Color::Indexed(180),
        _ => Color::Indexed(250),
    }
}
