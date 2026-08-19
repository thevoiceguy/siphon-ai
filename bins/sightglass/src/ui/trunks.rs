//! Trunks tab: every `[[register]]` binding **and** every `[[trunk]]`
//! allowlist across the fleet.
//!
//! The two are different things and the table says so. A registration
//! authenticates with credentials and therefore has live state —
//! registered / expiry / last error. A `[[trunk]]` is an IP allowlist:
//! no credentials, nothing to register, and so nothing to poll. Its
//! rows carry `ip-auth` and dashes.
//!
//! They share a tab because an operator asking "is my Twilio trunk
//! configured?" does not care about that distinction and should not
//! have to know it to get an answer. Showing only registrations made a
//! configured trunk indistinguishable from a missing one — reported
//! from a live session, and the reason §6.6's deferral was not enough
//! on its own. Actual reachability probing (OPTIONS toward the peer)
//! remains the deferred part.

use ratatui::layout::{Constraint, Rect};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Cell, Row, Table};
use ratatui::Frame;

use crate::model::{App, NodeHealth};

use super::Theme;

/// Render a trunk's peer list to fit `budget` columns **without ever
/// cutting a CIDR in half**.
///
/// A truncated CIDR is not merely ugly, it is wrong-looking in a
/// specific way: `35.156.191.128/30` clipped to `35.156.191.128/3`
/// reads as a valid `/3`, so the table would state a prefix the config
/// does not contain. Whole values only, and a count of what is not
/// shown — the full list is always available from
/// `GET /admin/v1/trunks`. DESIGN_SIGHTGLASS.md §7: nothing truncates
/// silently.
///
/// Never emits more than `budget` characters, for any `budget` at or
/// above [`MIN_PEER_W`] — the caller clamps to that, and below it no
/// output is meaningful anyway. When even one address plus its "+N
/// more" tail will not fit, it degrades to a bare count (`"8 peers"`)
/// rather than letting the terminal clip either: a clipped tail
/// (`"+7 mo"`) is the same class of bug as a clipped CIDR, just less
/// obvious about it.
/// Narrowest column `peer_summary` promises to respect. Below this
/// even `"NN peers"` does not fit, so there is nothing honest to draw.
const MIN_PEER_W: usize = 12;

fn peer_summary(addrs: &[String], budget: usize) -> String {
    if addrs.is_empty() {
        // Not "none": an empty `peer_addrs` means the IP check is
        // skipped for this trunk, which is the opposite of restrictive.
        return "any source".to_string();
    }
    let mut shown = 0usize;
    let mut len = 0usize;
    for (i, a) in addrs.iter().enumerate() {
        let sep = if i == 0 { 0 } else { 2 }; // ", "
                                              // Reserve room for the "+N more" tail, unless this is the last
                                              // address and it fits exactly — then nothing is being hidden.
        let remaining = addrs.len() - i - 1;
        let tail = if remaining == 0 {
            0
        } else {
            format!(", +{remaining} more").len()
        };
        if shown > 0 && len + sep + a.len() + tail > budget {
            break;
        }
        len += sep + a.len();
        shown += 1;
    }
    let hidden = addrs.len() - shown;
    let mut out = addrs[..shown].join(", ");
    if hidden > 0 {
        out.push_str(&format!(", +{hidden} more"));
    }
    if out.len() > budget {
        // Too narrow for even one whole address and its count. Say how
        // many there are and stop; `GET /admin/v1/trunks` has the list.
        let n = addrs.len();
        return if n == 1 {
            "1 peer".to_string()
        } else {
            format!("{n} peers")
        };
    }
    out
}

pub fn draw(frame: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let multi_node = app.nodes.len() > 1;

    let mut headers = vec!["NAME", "SERVER / PEERS", "STATUS", "EXPIRES", "LAST ERROR"];
    if multi_node {
        headers.insert(0, "NODE");
    }
    let header = Row::new(
        headers
            .into_iter()
            .map(|h| Cell::from(Span::styled(h, theme.dim_text()))),
    );

    // The peers column is given an *exact* width rather than a Min,
    // so the budget handed to `peer_summary` is the real thing rather
    // than a guess at how ratatui will divide leftover space between
    // two Min columns. Guessing is what made the first attempt at this
    // show one CIDR where four fit.
    const NAME_W: usize = 16;
    const STATUS_W: usize = 12;
    const EXPIRES_W: usize = 22;
    const NODE_W: usize = 12;
    const ERROR_W: usize = 10; // LAST ERROR keeps the Min and any slack
    let columns = if multi_node { 6 } else { 5 };
    let fixed = NAME_W + STATUS_W + EXPIRES_W + ERROR_W + if multi_node { NODE_W } else { 0 };
    let peer_budget = (area.width as usize)
        .saturating_sub(2) // borders
        .saturating_sub(columns - 1) // inter-column spacing
        .saturating_sub(fixed)
        .max(MIN_PEER_W);

    let mut rows = Vec::new();
    for (id, n) in app.nodes.iter().enumerate() {
        if app.node_filter.is_some_and(|f| f != id) {
            continue;
        }
        let stale = matches!(n.health, NodeHealth::Down { .. });
        for r in &n.registrations {
            let base = if stale {
                theme.dim_text()
            } else {
                theme.text()
            };
            let status_style = if stale {
                theme.dim_text()
            } else {
                theme.text().fg(theme.registration_color(&r.status))
            };
            let mut cells = vec![
                Cell::from(Span::styled(r.name.as_str(), base)),
                Cell::from(Span::styled(r.server_addr.as_str(), base)),
                Cell::from(Span::styled(r.status.as_str(), status_style)),
                Cell::from(Span::styled(r.expires_at.as_deref().unwrap_or("—"), base)),
                Cell::from(Span::styled(
                    r.last_error.as_deref().unwrap_or(""),
                    theme.dim_text(),
                )),
            ];
            if multi_node {
                cells.insert(0, Cell::from(Span::styled(n.name.as_str(), base)));
            }
            rows.push(Row::new(cells));
        }

        // Then the IP-authenticated trunks. Deliberately after the
        // registrations: those have live state and are what an
        // operator scans first.
        for t in &n.trunks {
            let base = if stale {
                theme.dim_text()
            } else {
                theme.text()
            };
            let peers = peer_summary(&t.peer_addrs, peer_budget);
            let mut cells = vec![
                Cell::from(Span::styled(t.name.clone(), base)),
                Cell::from(Span::styled(peers, base)),
                // Not a health verdict — a statement that this kind of
                // peer has no health to report. Dim so it never reads
                // as "up".
                Cell::from(Span::styled("ip-auth", theme.dim_text())),
                Cell::from(Span::styled("—", theme.dim_text())),
                Cell::from(Span::styled("", theme.dim_text())),
            ];
            if multi_node {
                cells.insert(0, Cell::from(Span::styled(n.name.as_str(), base)));
            }
            rows.push(Row::new(cells));
        }
    }

    let mut widths = vec![
        Constraint::Length(NAME_W as u16),
        Constraint::Length(peer_budget as u16),
        Constraint::Length(STATUS_W as u16),
        Constraint::Length(EXPIRES_W as u16),
        Constraint::Min(ERROR_W as u16),
    ];
    if multi_node {
        widths.insert(0, Constraint::Length(NODE_W as u16));
    }

    let empty = rows.is_empty();
    let table = Table::new(rows, widths).header(header).block(
        Block::new()
            .borders(Borders::ALL)
            .border_style(theme.dim_text())
            .title(Span::styled(" trunks ", theme.title())),
    );
    frame.render_widget(table, area);

    if empty {
        let inner = Rect {
            x: area.x + 2,
            y: area.y + 2,
            width: area.width.saturating_sub(4),
            height: 1,
        };
        frame.render_widget(
            ratatui::widgets::Paragraph::new(Span::styled(
                "no [[register]] bindings or [[trunk]] allowlists on the visible nodes",
                theme.dim_text(),
            )),
            inner,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{peer_summary, MIN_PEER_W};

    fn twilio() -> Vec<String> {
        [
            "54.172.60.0/30",
            "54.244.51.0/30",
            "54.171.127.192/30",
            "35.156.191.128/30",
            "54.65.63.192/30",
            "54.169.127.128/30",
            "54.252.254.64/30",
            "177.71.206.192/30",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// The reported bug: at a realistic width the row clipped to
    /// `…35.156.191.128/3`, which reads as a valid /3 prefix.
    #[test]
    fn never_cuts_a_cidr_in_half() {
        for budget in MIN_PEER_W..120 {
            let out = peer_summary(&twilio(), budget);
            // The count form is not a CIDR list and is checked
            // separately; everything else must be whole addresses.
            if out.ends_with("peers") || out.ends_with("peer") {
                continue;
            }
            for p in out.split(", ").filter(|p| !p.starts_with('+')) {
                assert!(
                    twilio().iter().any(|a| a == p),
                    "emitted a partial CIDR {p:?} at budget {budget} in {out:?}"
                );
            }
        }
    }

    #[test]
    fn says_how_many_it_is_hiding() {
        let out = peer_summary(&twilio(), 40);
        assert!(out.contains("more"), "{out}");
        let shown = out.split(", ").filter(|p| !p.starts_with('+')).count();
        assert!(out.contains(&format!("+{} more", 8 - shown)), "{out}");
    }

    #[test]
    fn a_list_that_fits_gets_no_tail() {
        let one = vec!["10.0.0.0/8".to_string()];
        assert_eq!(peer_summary(&one, 40), "10.0.0.0/8");
        // Exactly-fitting full list must not spend room on a tail it
        // does not need.
        let two = vec!["10.0.0.0/8".to_string(), "10.1.0.0/16".to_string()];
        assert_eq!(peer_summary(&two, 23), "10.0.0.0/8, 10.1.0.0/16");
    }

    /// Width 100 in the reported terminal gave a 21-column budget, and
    /// an earlier fix emitted `54.172.60.0/30, +7 more` (23 chars) into
    /// it — so the *tail* clipped to `+7 mo`. Degrading to a count is
    /// the only form that cannot be cut.
    #[test]
    fn degrades_to_a_count_when_even_one_address_will_not_fit() {
        assert_eq!(peer_summary(&twilio(), 21), "8 peers");
        assert_eq!(peer_summary(&twilio(), MIN_PEER_W), "8 peers");
        // A single address short enough to fit is still shown whole —
        // degrading is a last resort, not a width policy.
        assert_eq!(
            peer_summary(&["10.0.0.0/8".to_string()], MIN_PEER_W),
            "10.0.0.0/8"
        );
        // One that genuinely cannot fit degrades rather than clipping.
        let v6 = vec!["2001:0db8:85a3:0000:0000:8a2e:0370:7334/128".to_string()];
        assert_eq!(peer_summary(&v6, MIN_PEER_W), "1 peer");
    }

    /// The invariant the whole function exists for.
    #[test]
    fn never_exceeds_its_budget() {
        for budget in MIN_PEER_W..120 {
            let out = peer_summary(&twilio(), budget);
            assert!(
                out.len() <= budget,
                "emitted {} chars into a {budget}-wide column: {out:?}",
                out.len()
            );
        }
    }

    /// Empty means the IP check is skipped for this trunk — the
    /// opposite of restrictive, so it must not read as "none".
    #[test]
    fn empty_peer_list_says_any_source() {
        assert_eq!(peer_summary(&[], 40), "any source");
    }
}
