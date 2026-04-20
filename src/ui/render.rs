//! Ratatui draw routine — all in one function for clarity.
//!
//! Layout:
//! ┌─ Header: BTC block (left) + Balance (right) ─────────────┐
//! ├─ Main (split 62/38, min height capped by layout) ────────┤
//! │   Order book        │   Positions                         │
//! ├─ Open orders (8 lines, same as Fills) ────────────────────┤
//! ├─ Fills (8 lines) ─────────────────────────────────────────┤
//! ├─ Help / status (3 rows) ──────────────────────────────────┤
//! └────────────────────────────────────────────────────────────┘

use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph, Row, Table, Wrap, BorderType, Cell, Clear},
};

use crate::app::{AppState, InputMode, LimitField, Outcome};

pub fn draw(f: &mut Frame, s: &AppState) {
    let area = f.area();
    let chunks = Layout::vertical([
        Constraint::Length(4), // header
        Constraint::Min(8),    // main — book + positions (less vertical space than before)
        Constraint::Length(8), // open orders
        Constraint::Length(8), // fills
        Constraint::Length(3), // help
    ]).split(area);

    draw_header_row(f, chunks[0], s);
    draw_main(f, chunks[1], s);
    draw_open_orders(f, chunks[2], s);
    draw_fills(f, chunks[3], s);
    draw_help(f, chunks[4], s);

    if let InputMode::LimitModal { outcome, side, field } = s.input_mode {
        draw_limit_modal(f, area, s, outcome, side, field);
    }
    if let Some(ref err) = s.error_dialog {
        draw_error_dialog(f, area, err);
    }
}

// ── Header (BTC left, Balance right) ────────────────────────────────
fn draw_header_row(f: &mut Frame, area: Rect, s: &AppState) {
    let cols = Layout::horizontal([Constraint::Min(28), Constraint::Length(30)]).split(area);
    draw_header_btc(f, cols[0], s);
    draw_balance_panel(f, cols[1], s);
}

fn draw_balance_panel(f: &mut Frame, area: Rect, s: &AppState) {
    let cash = s
        .collateral_cash_usdc
        .map(|v| format!("${}", fmt_money(v)))
        .unwrap_or_else(|| "—".to_string());
    let claimable = s
        .collateral_claimable_usdc
        .map(|v| format!("${}", fmt_money(v)))
        .unwrap_or_else(|| "—".to_string());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Span::styled(" Balance ", Style::default().add_modifier(Modifier::BOLD)));
    let inner_w = block.inner(area).width.max(1) as usize;
    let line1 = balance_row_right_aligned("Cash ", &cash, inner_w, Color::Cyan, true);
    let line2 = balance_row_right_aligned("Claimable ", &claimable, inner_w, Color::White, false);
    let p = Paragraph::new(vec![line1, line2]).block(block).wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

/// Label left, value flush to the right edge of the inner rect (`inner_width` cells).
fn balance_row_right_aligned<'a>(
    label: &'a str,
    value: &'a str,
    inner_width: usize,
    value_color: Color,
    value_bold: bool,
) -> Line<'a> {
    let gap = inner_width.saturating_sub(label.chars().count() + value.chars().count());
    let mut vs = Style::default().fg(value_color);
    if value_bold {
        vs = vs.add_modifier(Modifier::BOLD);
    }
    Line::from(vec![
        Span::styled(label, Style::default().fg(Color::DarkGray)),
        Span::raw(" ".repeat(gap)),
        Span::styled(value, vs),
    ])
}

fn draw_header_btc(f: &mut Frame, area: Rect, s: &AppState) {
    let price_cell = match s.btc_price {
        Some(p) => format!("${:>12}", fmt_money(p)),
        None    => "        ---     ".to_string(),
    };
    let (colour, arrow) = match s.btc_above_target() {
        Some(true)  => (Color::Green, "▲"),
        Some(false) => (Color::Red,   "▼"),
        None        => (Color::Gray,  "·"),
    };
    let target = s.price_to_beat()
        .map(|t| format!("Price to Beat: ${}", fmt_money(t)))
        .unwrap_or_else(|| "Price to Beat: —".into());
    let delta = match (s.btc_price, s.price_to_beat()) {
        (Some(p), Some(t)) => format!("{:+.2}", p - t),
        _ => "—".into(),
    };

    let cd = s.countdown_secs()
        .map(|c| format!("{:02}:{:02}", c / 60, c % 60))
        .unwrap_or_else(|| "—".into());

    // Line 1: big coloured price
    let line1 = Line::from(vec![
        Span::styled("BTC/USD (Chainlink)  ", Style::default().fg(Color::DarkGray)),
        Span::styled(price_cell, Style::default().fg(colour).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(arrow, Style::default().fg(colour).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(delta, Style::default().fg(colour)),
    ]);
    // Line 2: price to beat + countdown + market
    let line2 = Line::from(vec![
        Span::styled(target, Style::default().fg(Color::Yellow)),
        Span::raw("   "),
        Span::styled(
            format!("Closes in {cd}"),
            Style::default().fg(match s.countdown_secs() {
                Some(c) if c < 30  => Color::Red,
                Some(c) if c < 120 => Color::Yellow,
                _                  => Color::Green,
            }).add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled(
            s.market.as_ref().map(|m| truncate(&m.question, 60)).unwrap_or_default(),
            Style::default().fg(Color::White),
        ),
    ]);

    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded)
        .title(Span::styled(" BTC 5m Predictions ", Style::default().add_modifier(Modifier::BOLD)));
    let p = Paragraph::new(vec![line1, line2]).block(block).wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

// ── Main (book + positions) ─────────────────────────────────────────
fn draw_main(f: &mut Frame, area: Rect, s: &AppState) {
    let cols = Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).split(area);
    draw_book(f, cols[0], s);
    draw_positions(f, cols[1], s);
}

fn draw_book(f: &mut Frame, area: Rect, s: &AppState) {
    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded)
        .title(" Order Book ")
        .title_style(Style::default().add_modifier(Modifier::BOLD));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let split = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(inner);
    draw_book_side(f, split[0], s, Outcome::Up,   Color::Green);
    draw_book_side(f, split[1], s, Outcome::Down, Color::Red);
}

fn draw_book_side(f: &mut Frame, area: Rect, s: &AppState, outcome: Outcome, colour: Color) {
    let book = s.book_for(outcome);

    // rows: up-to-5 asks (top, reversed so best is closest to mid) + mid + up-to-5 bids
    let mut rows: Vec<Row> = Vec::new();
    if let Some(b) = book {
        // Asks: show lowest-at-bottom (closest to mid)
        for l in b.asks.iter().take(5).rev() {
            rows.push(Row::new(vec![
                Cell::from(format!("{:.3}", l.price)).style(Style::default().fg(Color::LightRed)),
                Cell::from(format!("× {:.0}", l.size)).style(Style::default().fg(Color::DarkGray)),
            ]));
        }
        // Mid
        let mid = match (b.bids.first(), b.asks.first()) {
            (Some(bb), Some(ba)) => Some((bb.price + ba.price) / 2.0),
            _ => None,
        };
        if let Some(m) = mid {
            rows.push(Row::new(vec![
                Cell::from(format!("── {:.3} ──", m))
                    .style(Style::default().fg(colour).add_modifier(Modifier::BOLD | Modifier::DIM)),
                Cell::from(""),
            ]));
        }
        // Bids
        for l in b.bids.iter().take(5) {
            rows.push(Row::new(vec![
                Cell::from(format!("{:.3}", l.price)).style(Style::default().fg(Color::LightGreen)),
                Cell::from(format!("× {:.0}", l.size)).style(Style::default().fg(Color::DarkGray)),
            ]));
        }
    } else {
        rows.push(Row::new(vec![Cell::from("—"), Cell::from("")]));
    }

    let header = Row::new(vec![
        Cell::from(format!("{} price", outcome.as_str()))
            .style(Style::default().fg(colour).add_modifier(Modifier::BOLD)),
        Cell::from("size").style(Style::default().fg(Color::DarkGray)),
    ]);
    let table = Table::new(rows, [Constraint::Percentage(55), Constraint::Percentage(45)])
        .header(header)
        .column_spacing(1);
    f.render_widget(table, area);
}

fn draw_positions(f: &mut Frame, area: Rect, s: &AppState) {
    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded)
        .title(" Positions ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::vertical([
        Constraint::Length(3), Constraint::Length(3), Constraint::Length(1),
        Constraint::Length(2), Constraint::Length(2), Constraint::Min(0),
    ]).split(inner);

    render_position_line(f, rows[0], s, Outcome::Up,   Color::Green);
    render_position_line(f, rows[1], s, Outcome::Down, Color::Red);

    let realized = Paragraph::new(Line::from(vec![
        Span::styled("Realized ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("${:+.2}", s.realized_pnl),
            pnl_style(s.realized_pnl).add_modifier(Modifier::BOLD)),
    ]));
    f.render_widget(realized, rows[3]);

    let total_pnl = s.total_pnl();
    let total = Paragraph::new(Line::from(vec![
        Span::styled("Total    ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("${:+.2}", total_pnl),
            pnl_style(total_pnl).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)),
    ]));
    f.render_widget(total, rows[4]);
}

fn render_position_line(f: &mut Frame, area: Rect, s: &AppState, outcome: Outcome, colour: Color) {
    let p = s.position(outcome);
    let upnl = s.unrealized_pnl(outcome);
    let mark = s.mark(outcome).map(|m| format!("{:.3}", m)).unwrap_or_else(|| "—".into());

    let lines = vec![
        Line::from(vec![
            Span::styled(format!("  {}  ", outcome.as_str()),
                Style::default().fg(colour).add_modifier(Modifier::BOLD | Modifier::REVERSED)),
            Span::raw("  "),
            Span::raw(format!("{:.2} sh @ {:.3}", p.shares, p.avg_entry)),
        ]),
        Line::from(vec![
            Span::styled("       mark ", Style::default().fg(Color::DarkGray)),
            Span::raw(mark),
            Span::raw("    "),
            Span::styled(format!("uPnL ${:+.2}", upnl), pnl_style(upnl)),
        ]),
    ];
    f.render_widget(Paragraph::new(lines), area);
}

// ── Open orders ─────────────────────────────────────────────────────
fn draw_open_orders(f: &mut Frame, area: Rect, s: &AppState) {
    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded)
        .title(" Open Orders ");
    let rows: Vec<Row> = s.open_orders.iter().take(6).map(|o| {
        let side_cell = Cell::from(match o.side { crate::trading::Side::Buy => "BUY", _ => "SELL" })
            .style(Style::default().fg(match o.side { crate::trading::Side::Buy => Color::Green, _ => Color::Red }));
        Row::new(vec![
            side_cell,
            Cell::from(o.outcome.as_str()).style(Style::default().fg(
                match o.outcome { Outcome::Up => Color::Green, Outcome::Down => Color::Red }
            )),
            Cell::from(format!("{:.3}", o.price)),
            Cell::from(format!("{:.2}", o.remaining)),
        ])
    }).collect();
    let widths = [
        Constraint::Length(5),  // side
        Constraint::Length(5),  // outcome
        Constraint::Length(8),  // price
        Constraint::Length(10), // remaining
    ];
    let header = Row::new(["side", "out", "price", "left"])
        .style(Style::default().fg(Color::DarkGray));
    let table = Table::new(rows, widths).header(header).block(block).column_spacing(1);
    f.render_widget(table, area);
}

// ── Fills ───────────────────────────────────────────────────────────
fn draw_fills(f: &mut Frame, area: Rect, s: &AppState) {
    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded)
        .title(" Fills ");
    let rows: Vec<Row> = s.fills.iter().take(6).map(|f| {
        let side_cell = Cell::from(match f.side { crate::trading::Side::Buy => "BUY", _ => "SELL" })
            .style(Style::default().fg(match f.side { crate::trading::Side::Buy => Color::Green, _ => Color::Red }));
        Row::new(vec![
            Cell::from(f.ts.format("%H:%M:%S").to_string()).style(Style::default().fg(Color::DarkGray)),
            side_cell,
            Cell::from(f.outcome.as_str()).style(Style::default().fg(
                match f.outcome { Outcome::Up => Color::Green, Outcome::Down => Color::Red }
            )),
            Cell::from(format!("{:.2}", f.qty)),
            Cell::from(format!("@ {:.3}", f.price)),
            Cell::from(format!("= ${:.2}", f.qty * f.price)),
            Cell::from(if f.realized.abs() > 1e-9 { format!("{:+.2}", f.realized) } else { String::new() })
                .style(pnl_style(f.realized)),
        ])
    }).collect();
    let widths = [
        Constraint::Length(8),  // ts
        Constraint::Length(5),  // side
        Constraint::Length(5),  // outcome
        Constraint::Length(8),  // qty
        Constraint::Length(10), // price
        Constraint::Length(12), // notional
        Constraint::Length(10), // realized
    ];
    let header = Row::new(["time", "side", "out", "qty", "px", "notional", "pnl"])
        .style(Style::default().fg(Color::DarkGray));
    let table = Table::new(rows, widths).header(header).block(block).column_spacing(1);
    f.render_widget(table, area);
}

// ── Help / status ───────────────────────────────────────────────────
fn draw_help(f: &mut Frame, area: Rect, s: &AppState) {
    let size_span = match s.input_mode {
        InputMode::EditSize => Span::styled(
            format!("[{}_]", s.size_input),
            Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        _ => Span::styled(
            format!("[{}]", s.size_input),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
    };

    let keys = Line::from(vec![
        Span::styled("size ", Style::default().fg(Color::DarkGray)),
        size_span,
        Span::raw(" "),
        Span::raw("[u]"), Span::styled(" buy UP", Style::default().fg(Color::Green)), sep(),
        Span::raw("[d]"), Span::styled(" buy ", Style::default().fg(Color::Green)),
        Span::styled("DOWN", Style::default().fg(Color::Red)), sep(),
        Span::raw("[U]"), Span::styled(" sell ", Style::default().fg(Color::Red)),
        Span::styled("UP", Style::default().fg(Color::Green)), sep(),
        Span::raw("[D]"), Span::styled(" sell DOWN", Style::default().fg(Color::Red)), sep(),
        key("l", "limit"), sep(),
        key("c", "cancel all"), sep(),
        key("x", "CTF redeem"), sep(),
        key("s", "resize"), sep(),
        key("q", "quit"),
    ]);

    let status = Line::from(vec![
        Span::styled("› ", Style::default().fg(Color::DarkGray)),
        Span::raw(truncate(&s.status_line, 100)),
    ]);

    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded);
    let p = Paragraph::new(vec![keys, status]).block(block);
    f.render_widget(p, area);
}

fn key(k: &str, label: &str) -> Span<'static> {
    Span::from(format!("[{k}] {label}"))
}
fn sep() -> Span<'static> { Span::styled("  ", Style::default()) }

// ── Error dialog (top layer; Enter dismisses in main) ───────────────
fn draw_error_dialog(f: &mut Frame, screen: Rect, message: &str) {
    let max_inner_w = screen.width.saturating_sub(6).max(24);
    let box_w = (max_inner_w + 4).min(screen.width);
    let box_h = (screen.height.saturating_sub(4).min(22)).max(7);
    let area = centered(screen, box_w, box_h);
    f.render_widget(Clear, area);
    let dialog_bg = Color::Rgb(48, 24, 28);
    let border_style = Style::default()
        .fg(Color::Rgb(255, 180, 170))
        .bg(dialog_bg)
        .add_modifier(Modifier::BOLD);
    let text_style = Style::default().fg(Color::Rgb(255, 235, 235)).bg(dialog_bg);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .title(Span::styled(" Error ", text_style.add_modifier(Modifier::BOLD)))
        .border_style(border_style)
        .style(Style::default().bg(dialog_bg));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let hint = Line::from(vec![
        Span::styled(" enter ", Style::default().fg(Color::Rgb(180, 180, 190)).bg(dialog_bg)),
        Span::styled("close", Style::default().fg(Color::Rgb(248, 248, 252)).bg(dialog_bg)),
    ]);
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    let p = Paragraph::new(message)
        .style(text_style)
        .wrap(Wrap { trim: true });
    f.render_widget(p, rows[0]);
    f.render_widget(Paragraph::new(hint).alignment(Alignment::Center), rows[1]);
}

// ── Limit modal ─────────────────────────────────────────────────────
fn draw_limit_modal(f: &mut Frame, screen: Rect, s: &AppState,
    outcome: Outcome, side: crate::trading::Side, field: LimitField)
{
    let area = centered(screen, 52, 10);
    f.render_widget(Clear, area);
    // Darker fills so white/light-gray text and borders stay readable in any 16‑color theme.
    let dialog_bg = match outcome {
        Outcome::Up => Color::Rgb(10, 88, 48),
        Outcome::Down => Color::Rgb(122, 26, 38),
    };
    let border_style = Style::default()
        .fg(Color::Rgb(245, 250, 248))
        .bg(dialog_bg)
        .add_modifier(Modifier::BOLD);
    let text_main = Style::default().fg(Color::Rgb(248, 252, 250)).bg(dialog_bg);
    let text_muted = Style::default().fg(Color::Rgb(185, 198, 192)).bg(dialog_bg);
    let title = format!(" Limit {} {} ",
        match side { crate::trading::Side::Buy => "BUY", _ => "SELL" },
        outcome.as_str());
    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Double)
        .title(Span::styled(
            title,
            text_main.add_modifier(Modifier::BOLD),
        ))
        .border_style(border_style)
        .style(Style::default().bg(dialog_bg).fg(Color::Rgb(248, 252, 250)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::vertical([
        Constraint::Length(1), Constraint::Length(1),
        Constraint::Length(1), Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ]).split(inner);

    let price_hl = matches!(field, LimitField::Price);
    let size_hl  = matches!(field, LimitField::Size);
    let label_style = text_muted;
    let mk = |highlighted: bool, label: &str, val: &str| {
        let style = if highlighted {
            Style::default()
                .bg(Color::Rgb(28, 32, 34))
                .fg(Color::Rgb(255, 255, 255))
                .add_modifier(Modifier::BOLD)
        } else {
            text_main.add_modifier(Modifier::BOLD)
        };
        Line::from(vec![
            Span::styled(format!(" {label:10} "), label_style),
            Span::styled(format!("[{val}]"), style),
        ])
    };
    f.render_widget(Paragraph::new(mk(price_hl, "price", &s.limit_price_input)), rows[0]);
    f.render_widget(Paragraph::new(mk(size_hl,  "size",  &s.limit_size_input )), rows[1]);
    let size_hint = match side {
        crate::trading::Side::Buy => "size = USDC notional",
        crate::trading::Side::Sell => "size = shares",
    };
    f.render_widget(
        Paragraph::new(Span::styled(format!("   {size_hint}"), label_style)),
        rows[2],
    );

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("   tab ", text_muted),
            Span::styled("switch field   ", text_main),
            Span::styled("enter ", text_muted),
            Span::styled("submit   ", text_main),
            Span::styled("esc ", text_muted),
            Span::styled("cancel", text_main),
        ])),
        rows[3],
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("   ←/→", text_muted),
            Span::styled(" UP/DOWN ", text_main),
            Span::styled("↑/↓", text_muted),
            Span::styled(" buy/sell ", text_main),
            Span::styled("u d U D", text_muted),
            Span::styled(" quick", text_main),
        ])),
        rows[4],
    );
}

// ── helpers ─────────────────────────────────────────────────────────
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect { x, y, width: w.min(area.width), height: h.min(area.height) }
}
fn fmt_money(v: f64) -> String {
    // e.g. 67_432.51
    let whole = v as i64;
    let frac  = ((v - whole as f64).abs() * 100.0).round() as u64;
    let mut ws = whole.to_string().into_bytes();
    let mut with_commas = Vec::with_capacity(ws.len() + ws.len() / 3);
    let skip_sign = if ws.first() == Some(&b'-') { with_commas.push(b'-'); 1 } else { 0 };
    let digits = &ws[skip_sign..];
    for (i, &c) in digits.iter().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 { with_commas.push(b','); }
        with_commas.push(c);
    }
    let _ = &mut ws;
    format!("{}.{:02}", String::from_utf8_lossy(&with_commas), frac)
}
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() }
    else { s.chars().take(n.saturating_sub(1)).collect::<String>() + "…" }
}
fn pnl_style(v: f64) -> Style {
    if v > 1e-9       { Style::default().fg(Color::Green) }
    else if v < -1e-9 { Style::default().fg(Color::Red) }
    else              { Style::default().fg(Color::Gray) }
}
