//! Modal and toast overlays, drawn last so they sit above the tab
//! content. Confirm modals always carry the node name in the prompt
//! (§5 — never act on the wrong box); toasts are transient
//! action-result lines bottom-right, above the footer.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::model::{App, InputModal, Modal};

use super::Theme;

pub fn draw(frame: &mut Frame, app: &App, theme: &Theme) {
    match &app.modal {
        None => {}
        Some(Modal::Confirm { prompt, .. }) => draw_confirm(frame, theme, prompt),
        Some(Modal::Input(form)) => draw_form(frame, app, theme, form),
    }
    draw_toasts(frame, app, theme);
}

/// A centered rect `width`×`height`, clamped to the frame.
fn centered(frame: &Frame, width: u16, height: u16) -> Rect {
    let area = frame.area();
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn draw_confirm(frame: &mut Frame, theme: &Theme, prompt: &str) {
    let width = (prompt.len() as u16 + 6).max(30);
    let area = centered(frame, width, 5);
    frame.render_widget(Clear, area);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(theme.text().fg(theme.warn))
        .title(Span::styled(" confirm ", theme.title()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::default(),
            Line::styled(prompt.to_string(), theme.text()).centered(),
        ]),
        inner,
    );
}

fn draw_form(frame: &mut Frame, app: &App, theme: &Theme, form: &InputModal) {
    let height = (form.fields.len() as u16) + 4;
    let area = centered(frame, 56, height);
    frame.render_widget(Clear, area);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(theme.text().fg(theme.accent))
        .title(Span::styled(
            form.title(&app.nodes[form.node].name),
            theme.title(),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines = Vec::with_capacity(form.fields.len() + 2);
    if let Some(call_id) = &form.call_id {
        lines.push(Line::from(vec![
            Span::styled("        call  ", theme.dim_text()),
            Span::styled(call_id.as_str(), theme.text()),
        ]));
    }
    for (i, field) in form.fields.iter().enumerate() {
        let active = i == form.active;
        let cursor = if active { "▏" } else { " " };
        let label_style = if active {
            theme.title()
        } else {
            theme.dim_text()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{:>12}  ", field.label), label_style),
            Span::styled(field.value.clone(), theme.text()),
            Span::styled(cursor, theme.title()),
        ]));
    }
    lines.push(Line::styled(
        "enter submit · tab next · esc cancel",
        theme.dim_text(),
    ));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_toasts(frame: &mut Frame, app: &App, theme: &Theme) {
    if app.toasts.is_empty() {
        return;
    }
    let area = frame.area();
    // Stack bottom-up, right-aligned, above the footer line.
    for (i, toast) in app.toasts.iter().rev().enumerate() {
        let y = area.height.saturating_sub(2).saturating_sub(i as u16);
        if y == 0 {
            break;
        }
        let text = format!(" {} ", toast.text);
        let w = (text.len() as u16).min(area.width.saturating_sub(2));
        let rect = Rect {
            x: area.width.saturating_sub(w + 1),
            y,
            width: w,
            height: 1,
        };
        let color = if toast.ok { theme.ok } else { theme.err };
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(Span::styled(text, theme.text().fg(color).bg(theme.surface))),
            rect,
        );
    }
}
