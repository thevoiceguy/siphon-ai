//! SIP ladder overlay for the focused call (DESIGN_SIP_LADDER.md §5).
//!
//! Toggled with `s` on the calls tab, drawn over the detail pane so a
//! ladder gets the height it needs — a call's exchange is 6–20 lines
//! and squeezing it into a corner of the detail pane would make it
//! useless for the one job it has.
//!
//! Every failure renders as a note *in the pane*: capture disabled on
//! that node (`501`), the token lacking `operator` (`403`), or a
//! daemon predating the endpoint (`404`). None of them is a
//! node-health signal — a node answering everything else is up
//! whether or not it serves a ladder — and none produces a toast,
//! because the pane is already the place the operator is looking.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::model::{App, LadderState};

use super::Theme;

/// Draw the overlay if it is open. Returns without drawing when the
/// pane is closed, so the caller can call this unconditionally.
pub fn draw(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    if !app.ladder.open {
        return;
    }
    frame.render_widget(Clear, area);

    let count = app.ladder.message_count();
    let title = match &app.ladder.key {
        Some((node, call_id)) if app.nodes.len() > 1 => {
            format!(" sip · {} · {call_id} ", app.nodes[*node].name)
        }
        Some((_, call_id)) => format!(" sip · {call_id} "),
        None => " sip ".to_string(),
    };
    let footer = if app.ladder.expanded {
        "⏎ collapse · y copy · esc/s close"
    } else {
        "j/k move · ⏎ expand · y copy · esc/s close"
    };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(theme.title())
        .title(Span::styled(title, theme.title()))
        .title_bottom(Span::styled(footer, theme.dim_text()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    match &app.ladder.state {
        LadderState::Loading => note(frame, theme, inner, "fetching signaling…"),
        LadderState::Disabled => note(
            frame,
            theme,
            inner,
            "SIP ladder capture is off on this node \
             ([observability].sip_ring_size = 0). Homer has the full \
             signaling if [hep] is configured.",
        ),
        LadderState::Forbidden => note(
            frame,
            theme,
            inner,
            "this token may not read raw SIP — the ladder returns \
             messages unredacted, so it needs an operator token \
             (readonly can see everything else on this tab).",
        ),
        LadderState::Unavailable(detail) => note(
            frame,
            theme,
            inner,
            &format!("no ladder for this call: {detail}"),
        ),
        LadderState::Error(detail) => note(frame, theme, inner, detail),
        LadderState::Ready(resp) if resp.messages.is_empty() => note(
            frame,
            theme,
            inner,
            "no messages captured for this call — capture may have \
             started after it did, or its trace has been evicted.",
        ),
        LadderState::Ready(resp) => {
            if app.ladder.expanded {
                draw_expanded(frame, app, theme, inner);
            } else {
                draw_list(frame, app, theme, inner, count, resp.truncated);
            }
        }
    }
}

fn note(frame: &mut Frame, theme: &Theme, area: Rect, text: &str) {
    frame.render_widget(
        Paragraph::new(Span::styled(text.to_string(), theme.dim_text())).wrap(Wrap { trim: true }),
        area,
    );
}

/// One line per message: time relative to the first, direction arrow,
/// start-line, and a body-size hint. Relative rather than wall-clock
/// because what you read a ladder for is the *gaps* — a 32 s pause
/// before a BYE is the finding, and absolute timestamps make you do
/// the subtraction yourself.
fn draw_list(
    frame: &mut Frame,
    app: &App,
    theme: &Theme,
    area: Rect,
    count: usize,
    truncated: bool,
) {
    let LadderState::Ready(resp) = &app.ladder.state else {
        return;
    };
    let base = resp.messages.first().map(|m| m.ts_ms).unwrap_or(0);

    // Keep the cursor on screen without a scrollbar: page the window
    // so the selection is always inside it.
    let height = area.height as usize;
    let first = app.ladder.selected.saturating_sub(height.saturating_sub(1));

    let mut lines: Vec<Line> = Vec::with_capacity(height);
    if truncated && first == 0 {
        lines.push(Line::styled(
            format!("… older messages dropped (per-call cap); showing last {count}"),
            theme.dim_text(),
        ));
    }
    for (i, m) in resp.messages.iter().enumerate().skip(first).take(height) {
        let offset = m.ts_ms.saturating_sub(base);
        let arrow = match m.direction.as_str() {
            "out" => "→",
            "in" => "←",
            _ => "·",
        };
        let style = if i == app.ladder.selected {
            theme.selected_row()
        } else {
            theme.text()
        };
        lines.push(Line::styled(
            format!(
                "{:>7} {arrow} {}{}",
                format!("+{}.{:03}", offset / 1000, offset % 1000),
                start_line(&m.payload),
                body_hint(&m.payload),
            ),
            style,
        ));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The selected message in full, headers and body verbatim — this is
/// the unredacted view §2 decided on, and the reason the endpoint is
/// operator-gated.
fn draw_expanded(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let Some(msg) = app.ladder.selected_message() else {
        return;
    };
    let [head_area, body_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    frame.render_widget(
        Paragraph::new(Line::styled(
            format!("{} → {}", msg.src, msg.dst),
            theme.dim_text(),
        )),
        head_area,
    );
    let body: Vec<Line> = msg
        .payload
        .replace('\r', "")
        .lines()
        .map(|l| Line::styled(l.to_string(), theme.text()))
        .collect();
    frame.render_widget(Paragraph::new(body), body_area);
}

/// The request/status line — everything before the first CRLF.
fn start_line(payload: &str) -> String {
    payload
        .lines()
        .next()
        .unwrap_or("(empty)")
        .trim_end()
        .to_string()
}

/// `" (SDP 214 B)"` when the message carries a body, else empty. The
/// size is what tells you an INVITE actually offered media, which is
/// the question a ladder is usually being read to answer.
fn body_hint(payload: &str) -> String {
    let normalized = payload.replace("\r\n", "\n");
    let Some((headers, body)) = normalized.split_once("\n\n") else {
        return String::new();
    };
    if body.trim().is_empty() {
        return String::new();
    }
    let kind = if headers.to_ascii_lowercase().contains("application/sdp") {
        "SDP"
    } else {
        "body"
    };
    format!(" ({kind} {} B)", body.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_line_is_the_request_line() {
        assert_eq!(
            start_line("INVITE sip:x@y SIP/2.0\r\nVia: z\r\n\r\n"),
            "INVITE sip:x@y SIP/2.0"
        );
        assert_eq!(start_line(""), "(empty)");
    }

    #[test]
    fn body_hint_names_sdp_and_sizes_it() {
        let with_sdp = "INVITE sip:x SIP/2.0\r\n\
             Content-Type: application/SDP\r\n\
             \r\n\
             v=0\r\no=- 1 1 IN IP4 1.2.3.4\r\n";
        let hint = body_hint(with_sdp);
        assert!(hint.starts_with(" (SDP "), "got {hint:?}");
        assert!(hint.ends_with(" B)"));
    }

    #[test]
    fn body_hint_is_empty_for_a_bodiless_message() {
        assert_eq!(body_hint("ACK sip:x SIP/2.0\r\nVia: z\r\n\r\n"), "");
        assert_eq!(body_hint("SIP/2.0 100 Trying\r\n\r\n"), "");
    }

    // A body that isn't SDP still gets sized — a ladder that silently
    // hid, say, a SIP-INFO DTMF payload would be lying by omission.
    #[test]
    fn body_hint_falls_back_to_a_generic_label() {
        let info = "INFO sip:x SIP/2.0\r\n\
             Content-Type: application/dtmf-relay\r\n\
             \r\n\
             Signal=1\r\n";
        assert!(body_hint(info).starts_with(" (body "));
    }
}
