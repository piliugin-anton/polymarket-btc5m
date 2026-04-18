//! Ratatui draw routine — all in one function for clarity.
//!
//! Layout:
//! ┌─ Header (2 lines) ───────────────────────────────────────┐
//! │ BTC / price to beat / countdown / market title           │
//! ├─ Main (split 60/40) ───────────────────────────────────────┤
//! │   Order book        │   Positions                         │
//! ├─ Fills (8 rows) ──────────────────────────────────────────┤
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
        Constraint::Min(12),   // main
        Constraint::Length(8), // fills
        Constraint::Length(3), // help
    ]).split(area);

    draw_header(f, chunks[0], s);
    draw_main(f, chunks[1], s);
    draw_fills(f, chunks[2], s);
    draw_help(f, chunks[3], s);

    if let InputMode::LimitModal { outcome, side, field } = s.input_mode {
        draw_limit_modal(f, area, s, outcome, side, field);
    }
}

// ── Header ──────────────────────────────────────────────────────────
fn draw_header(f: &mut Frame, area: Rect, s: &AppState) {
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
        Span::raw(" buy$·sell sh "),
        key("u", "buy UP"), sep(),
        key("d", "buy DOWN"), sep(),
        key("U", "sell UP"), sep(),
        key("D", "sell DOWN"), sep(),
        key("l", "limit"), sep(),
        key("c", "cancel all"), sep(),
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

// ── Limit modal ─────────────────────────────────────────────────────
fn draw_limit_modal(f: &mut Frame, screen: Rect, s: &AppState,
    outcome: Outcome, side: crate::trading::Side, field: LimitField)
{
    let area = centered(screen, 50, 8);
    f.render_widget(Clear, area);
    let title = format!(" Limit {} {} ",
        match side { crate::trading::Side::Buy => "BUY", _ => "SELL" },
        outcome.as_str());
    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Double)
        .title(title)
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::vertical([
        Constraint::Length(1), Constraint::Length(1),
        Constraint::Length(1), Constraint::Length(1),
        Constraint::Min(0),
    ]).split(inner);

    let price_hl = matches!(field, LimitField::Price);
    let size_hl  = matches!(field, LimitField::Size);
    let mk = |highlighted: bool, label: &str, val: &str| {
        let style = if highlighted {
            Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD)
        } else { Style::default().fg(Color::Gray) };
        Line::from(vec![
            Span::styled(format!(" {label:10} "), Style::default().fg(Color::DarkGray)),
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
        Paragraph::new(Span::styled(format!("   {size_hint}"), Style::default().fg(Color::DarkGray))),
        rows[2],
    );

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("   tab ", Style::default().fg(Color::DarkGray)),
            Span::raw("switch field   "),
            Span::styled("enter ", Style::default().fg(Color::DarkGray)),
            Span::raw("submit   "),
            Span::styled("esc ", Style::default().fg(Color::DarkGray)),
            Span::raw("cancel"),
        ])),
        rows[3],
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
