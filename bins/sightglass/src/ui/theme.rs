//! One theme, used everywhere (DESIGN_SIGHTGLASS.md §7).
//!
//! Every color in the UI comes off this palette — no ad-hoc
//! `Color::…` in widget code. Status is always glyph + color, never
//! color alone, and `--ascii` swaps the glyph set for terminals
//! without Unicode fonts.

use ratatui::style::{Color, Modifier, Style};

use crate::model::NodeHealth;

/// Catppuccin-Macchiato-adjacent palette: one accent, semantic
/// green/amber/red, and a dim tone for de-emphasis.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub fg: Color,
    pub accent: Color,
    pub dim: Color,
    pub ok: Color,
    pub warn: Color,
    pub err: Color,
    pub surface: Color,
    pub ascii: bool,
}

impl Theme {
    pub fn new(ascii: bool) -> Self {
        Self {
            fg: Color::Rgb(0xca, 0xd3, 0xf5),
            accent: Color::Rgb(0x8a, 0xad, 0xf4),
            dim: Color::Rgb(0x6e, 0x73, 0x8d),
            ok: Color::Rgb(0xa6, 0xda, 0x95),
            warn: Color::Rgb(0xee, 0xd4, 0x9f),
            err: Color::Rgb(0xed, 0x87, 0x96),
            surface: Color::Rgb(0x36, 0x3a, 0x4f),
            ascii,
        }
    }

    pub fn text(&self) -> Style {
        Style::new().fg(self.fg)
    }

    pub fn dim_text(&self) -> Style {
        Style::new().fg(self.dim)
    }

    pub fn title(&self) -> Style {
        Style::new().fg(self.accent).add_modifier(Modifier::BOLD)
    }

    pub fn selected_row(&self) -> Style {
        Style::new()
            .bg(self.surface)
            .fg(self.fg)
            .add_modifier(Modifier::BOLD)
    }

    /// Status glyph + color for a node's health. Glyph and color move
    /// together so the state survives monochrome terminals.
    pub fn health_glyph(&self, health: &NodeHealth) -> (&'static str, Color) {
        match health {
            NodeHealth::Up => (if self.ascii { "o" } else { "●" }, self.ok),
            NodeHealth::Connecting => (if self.ascii { "~" } else { "◐" }, self.warn),
            NodeHealth::Down { .. } => (if self.ascii { "x" } else { "○" }, self.err),
        }
    }

    /// Registration status → color, keyed on the daemon's status
    /// strings ("registered" is healthy; anything failed-ish is red).
    pub fn registration_color(&self, status: &str) -> Color {
        match status {
            "registered" => self.ok,
            s if s.contains("fail") || s.contains("error") => self.err,
            _ => self.warn,
        }
    }
}
