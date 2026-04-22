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

use std::time::Instant;

use crate::app::{AppState, DepositModalPhase, InputMode, LimitField, Outcome, UiPhase};
use crate::bridge_deposit::SOLANA_MAINNET_USDC_MINT;
use crate::market_profile::Timeframe;

pub fn draw(f: &mut Frame, s: &AppState) {
    if s.ui_phase != UiPhase::Trading {
        draw_wizard(f, s);
        if let Some(ref t) = s.order_error_toast {
            if std::time::Instant::now() < t.until {
                let area = f.area();
                draw_order_error_toast(f, area, t.message.as_str());
            }
        }
        return;
    }
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
    if let Some(ref phase) = s.deposit_modal {
        draw_deposit_modal(f, area, phase);
    }
    if let Some(ref t) = s.order_error_toast {
        if Instant::now() < t.until {
            draw_order_error_toast(f, area, t.message.as_str());
        }
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

fn draw_wizard(f: &mut Frame, s: &AppState) {
    let area = f.area();
    let wizard_title = match s.ui_phase {
        UiPhase::WizardLoading => " Select market — Polymarket ".to_string(),
        UiPhase::WizardPickAsset | UiPhase::WizardPickTimeframe => {
            let sym = s
                .wizard_rows
                .get(s.wizard_list_idx)
                .map(|r| r.asset.label)
                .unwrap_or("…");
            format!(" Select market - {sym} ")
        }
        UiPhase::Trading => " Select market — Polymarket ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(wizard_title.as_str());
    f.render_widget(Clear, area);
    let inner = block.inner(area);
    f.render_widget(block, area);

    match s.ui_phase {
        UiPhase::WizardLoading => {
            let t = "Loading series from Polymarket…";
            let p = Paragraph::new(t).style(Style::default().fg(Color::Cyan));
            f.render_widget(p, inner);
        }
        UiPhase::WizardPickAsset => {
            let title = "Choose asset (↑/↓) then Enter  ·  Q quit";
            let header: String = if let Some(ref e) = s.wizard_series_error {
                format!("Warning: {e}\n{title}\n")
            } else {
                format!("{title}\n")
            };
            let p = Paragraph::new(header.as_str())
                .style(Style::default().fg(Color::White));
            f.render_widget(p, inner);
            let start_row = 2u16;
            let visible = (inner.height.saturating_sub(start_row as u16 + 1)) as usize;
            for (i, row) in s.wizard_rows.iter().enumerate() {
                if i >= visible {
                    break;
                }
                let y = start_row as usize + i;
                if y >= inner.y as usize + inner.height as usize {
                    break;
                }
                let is_sel = i == s.wizard_list_idx;
                let vol = row
                    .volume_24h
                    .map(|v| format!("  vol 24h ${:.0}", v))
                    .unwrap_or_default();
                let line = if is_sel {
                    format!(" ▸ {} — {}{}", row.asset.label, row.title, vol)
                } else {
                    format!("   {} — {}{}", row.asset.label, row.title, vol)
                };
                let style = if is_sel {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::default().fg(Color::White)
                };
                let r = Rect {
                    x:      inner.x + 1,
                    y:      inner.y + start_row + i as u16,
                    width:  inner.width.saturating_sub(2),
                    height: 1,
                };
                f.render_widget(Paragraph::new(line).style(style), r);
            }
        }
        UiPhase::WizardPickTimeframe => {
            let p = Paragraph::new("Time window (↑/↓) Enter  ·  Esc/B back  ·  Q quit\n")
                .style(Style::default().fg(Color::White));
            f.render_widget(p, Rect { x: inner.x, y: inner.y, width: inner.width, height: 2 });
            let options: [(usize, &str, Timeframe); 2] = [
                (0, "5 minutes",  Timeframe::M5),
                (1, "15 minutes", Timeframe::M15),
            ];
            for (idx, (i, desc, tf)) in options.iter().enumerate() {
                let is_sel = *i == s.wizard_tf_idx;
                let mark = if is_sel { " ▸ " } else { "   " };
                let line = format!("{mark}{desc}  ·  {}", tf.label());
                let style = if is_sel {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::default().fg(Color::White)
                };
                let r = Rect {
                    x:      inner.x + 1,
                    y:      inner.y + 3 + idx as u16,
                    width:  inner.width.saturating_sub(2),
                    height: 1,
                };
                f.render_widget(Paragraph::new(line).style(style), r);
            }
        }
        UiPhase::Trading => {}
    }
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
    let dec = s.spot_usd_decimal_places();
    let pair = s.spot_pair_label();
    let price_cell = match s.spot_price {
        Some(p) => format!("${}", fmt_money_decimals(p, dec as u32)),
        None    => "—".to_string(),
    };
    let (colour, arrow) = match s.spot_above_target() {
        Some(true)  => (Color::Green, "▲"),
        Some(false) => (Color::Red,   "▼"),
        None        => (Color::Gray,  "·"),
    };
    let target = s.price_to_beat()
        .map(|t| format!("Open: ${}", fmt_money_decimals(t, dec as u32)))
        .unwrap_or_else(|| "Open: —".into());
    let delta = match (s.spot_price, s.price_to_beat()) {
        (Some(p), Some(t)) => format!("{:+.*}", dec, p - t),
        _ => "—".into(),
    };

    // Sentiment: prefer CLOB mid (live book) over top-holder sums — `/holders` can stay skewed
    // to one side (whale positions) and misread as "mood" vs current order book.
    const SENT_EPS: f64 = 1e-6;
    let m_up = s.mark(Outcome::Up);
    let m_down = s.mark(Outcome::Down);
    let sentiment_spans: Vec<Span> = match (m_up, m_down) {
        (Some(m_u), Some(m_d)) if m_u > m_d + SENT_EPS => vec![
            Span::styled("Sentiment: ", Style::default().fg(Color::DarkGray)),
            Span::styled("▲", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        ],
        (Some(m_u), Some(m_d)) if m_d > m_u + SENT_EPS => vec![
            Span::styled("Sentiment: ", Style::default().fg(Color::DarkGray)),
            Span::styled("▼", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        ],
        (Some(_), Some(_)) => vec![
            Span::styled("Sentiment: ", Style::default().fg(Color::DarkGray)),
            Span::styled("·", Style::default().fg(Color::DarkGray)),
        ],
        _ => match (s.top_holders_up_sum, s.top_holders_down_sum) {
            (Some(u), Some(d)) if u > d + SENT_EPS => vec![
                Span::styled("Sentiment: ", Style::default().fg(Color::DarkGray)),
                Span::styled("▲", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            ],
            (Some(u), Some(d)) if d > u + SENT_EPS => vec![
                Span::styled("Sentiment: ", Style::default().fg(Color::DarkGray)),
                Span::styled("▼", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            ],
            (Some(_), Some(_)) => vec![
                Span::styled("Sentiment: ", Style::default().fg(Color::DarkGray)),
                Span::styled("·", Style::default().fg(Color::DarkGray)),
            ],
            _ => vec![
                Span::styled("Sentiment: ", Style::default().fg(Color::DarkGray)),
                Span::styled("—", Style::default().fg(Color::DarkGray)),
            ],
        },
    };

    let cd = s.countdown_secs()
        .map(|c| format!("{:02}:{:02}", c / 60, c % 60))
        .unwrap_or_else(|| "—".into());

    // Line 1: price + oracle arrow + delta, then sentiment (CLOB mid, else top-holders) at end
    let mut line1_parts: Vec<Span> = vec![
        Span::styled(
            format!("{pair} (Chainlink)  "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(price_cell, Style::default().fg(colour).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(arrow, Style::default().fg(colour).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(delta, Style::default().fg(colour)),
        Span::raw("  "),
    ];
    line1_parts.extend(sentiment_spans);
    let line1 = Line::from(line1_parts);
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

    let block_title = s
        .market_profile
        .as_ref()
        .map(|p| format!(" {} {} ", p.asset.label, p.timeframe.tui_phrase()))
        .unwrap_or_else(|| " — ".to_string());
    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded)
        .title(Span::styled(block_title, Style::default().add_modifier(Modifier::BOLD)));
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
        Span::raw("[w]"), Span::styled(" buy UP", Style::default().fg(Color::Green)), sep(),
        Span::raw("[s]"), Span::styled(" buy ", Style::default().fg(Color::Green)),
        Span::styled("DOWN", Style::default().fg(Color::Red)), sep(),
        Span::raw("[a]"), Span::styled(" sell ", Style::default().fg(Color::Red)),
        Span::styled("UP", Style::default().fg(Color::Green)), sep(),
        Span::raw("[d]"), Span::styled(" sell DOWN", Style::default().fg(Color::Red)), sep(),
        key("l", "limit"), sep(),
        key("c", "cancel all"), sep(),
        key("x", "redeem all"), sep(),
        key("f", "SOL USDC dep"), sep(),
        key("e", "resize"), sep(),
        key("Esc", "timeframe"), sep(),
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

// ── Order error toast (bottom-right; non-blocking) ───────────────────
fn draw_order_error_toast(f: &mut Frame, screen: Rect, message: &str) {
    let margin = 1u16;
    let max_box_w = screen.width.saturating_sub(margin * 2).min(50).max(24);
    let inner_w = (max_box_w as usize).saturating_sub(2).max(8);
    let nchars = message.chars().count();
    let nlines = nchars
        .saturating_add(inner_w.saturating_sub(1))
        / inner_w
        .max(1);
    let content_lines = (nlines as u16).max(1).min(5);
    let box_h = (2u16 + content_lines).min(screen.height.saturating_sub(margin * 2));
    let area = bottom_right_rect(screen, max_box_w, box_h, margin);
    f.render_widget(Clear, area);
    let dialog_bg = Color::Rgb(48, 24, 28);
    let border_style = Style::default()
        .fg(Color::Rgb(255, 180, 170))
        .bg(dialog_bg)
        .add_modifier(Modifier::BOLD);
    let text_style = Style::default().fg(Color::Rgb(255, 235, 235)).bg(dialog_bg);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Span::styled(" Order error ", text_style.add_modifier(Modifier::BOLD)))
        .border_style(border_style)
        .style(Style::default().bg(dialog_bg));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let p = Paragraph::new(message)
        .style(text_style)
        .wrap(Wrap { trim: true });
    f.render_widget(p, inner);
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
            Span::styled("w s a d", text_muted),
            Span::styled(" quick", text_main),
        ])),
        rows[4],
    );
}

// ── Solana USDC deposit modal (`f`) ─────────────────────────────────
fn draw_deposit_modal(f: &mut Frame, screen: Rect, phase: &DepositModalPhase) {
    let dialog_bg = Color::Rgb(18, 42, 82);
    let border_style = Style::default()
        .fg(Color::Rgb(160, 210, 255))
        .bg(dialog_bg)
        .add_modifier(Modifier::BOLD);
    let text_main = Style::default().fg(Color::Rgb(235, 244, 255)).bg(dialog_bg);
    let text_muted = Style::default().fg(Color::Rgb(150, 175, 215)).bg(dialog_bg);
    let text_warn = Style::default().fg(Color::Rgb(255, 220, 140)).bg(dialog_bg);

    match phase {
        DepositModalPhase::Loading => {
            let area = centered(screen, 52, 7);
            f.render_widget(Clear, area);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(Span::styled(" Solana USDC deposit ", text_main.add_modifier(Modifier::BOLD)))
                .border_style(border_style)
                .style(Style::default().bg(dialog_bg));
            let inner = block.inner(area);
            f.render_widget(block, area);
            let lines = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Requesting deposit address from bridge…",
                    text_muted,
                )),
            ];
            f.render_widget(
                Paragraph::new(lines).alignment(Alignment::Center),
                inner,
            );
        }
        DepositModalPhase::Failed(msg) => {
            let inner_w = (screen.width.saturating_sub(8).min(58).max(40)) as usize;
            let wrapped = wrap_deposit_error(msg, inner_w);
            let n = wrapped.len().max(1) as u16;
            let h = (6 + n).min(screen.height.saturating_sub(2)).max(8);
            let w = (inner_w as u16) + 4;
            let area = centered(screen, w, h);
            f.render_widget(Clear, area);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(Span::styled(" Solana deposit — error ", text_warn.add_modifier(Modifier::BOLD)))
                .border_style(border_style)
                .style(Style::default().bg(dialog_bg));
            let inner = block.inner(area);
            f.render_widget(block, area);
            let mut lines: Vec<Line> = vec![Line::from("")];
            for line in wrapped {
                lines.push(Line::from(Span::styled(line, text_main)));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("esc  close", text_muted)));
            f.render_widget(
                Paragraph::new(lines)
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: true }),
                inner,
            );
        }
        DepositModalPhase::Ready { svm_address, qr_unicode } => {
            let qr_lines: Vec<&str> = qr_unicode.lines().collect();
            let qr_h = qr_lines.len().max(1) as u16;
            let qr_w = qr_lines
                .iter()
                .map(|l| l.chars().count())
                .max()
                .unwrap_or(20) as u16;
            // Header copy must fit narrow widths; inner width ≈ w − 2 (borders).
            let w = (qr_w + 8)
                .max(58)
                .max(44 + 6 + 4)
                .min(screen.width.saturating_sub(4));
            let inner_w = w.saturating_sub(2).max(12) as usize;
            let mint_with_label = format!("Mint: {SOLANA_MAINNET_USDC_MINT}");
            let send_hint = "Send USDC from Solana to the address below or scan the QR.";
            let header_wrapped_lines: u16 = (word_wrap_line_count("Network: Solana (SPL)", inner_w)
                + word_wrap_line_count("Token: USDC only (Solana SPL)", inner_w)
                + word_wrap_line_count(&mint_with_label, inner_w)
                + word_wrap_line_count(send_hint, inner_w)
                + 1)
                .max(5) as u16; // trailing blank
            let head_h = header_wrapped_lines;
            let foot_h = 3u16;
            let h = (head_h + qr_h + foot_h + 2)
                .min(screen.height.saturating_sub(2))
                .max(12);
            let area = centered(screen, w, h);
            f.render_widget(Clear, area);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Double)
                .title(Span::styled(
                    " Solana blockchain — USDC deposit ",
                    text_main.add_modifier(Modifier::BOLD),
                ))
                .border_style(border_style)
                .style(Style::default().bg(dialog_bg));
            let inner = block.inner(area);
            f.render_widget(block, area);

            let chunks = Layout::vertical([
                Constraint::Length(head_h),
                Constraint::Length(qr_h),
                Constraint::Length(foot_h),
                Constraint::Min(0),
            ])
            .split(inner);

            let mint_style = Style::default()
                .fg(Color::Rgb(190, 230, 255))
                .bg(dialog_bg);
            let header: Vec<Line> = [
                Line::from(vec![
                    Span::styled("Network: ", text_muted),
                    Span::styled("Solana (SPL)", text_warn.add_modifier(Modifier::BOLD)),
                ]),
                Line::from(vec![
                    Span::styled("Token: ", text_muted),
                    Span::styled("USDC only (Solana SPL)", text_main.add_modifier(Modifier::BOLD)),
                ]),
                Line::from(vec![
                    Span::styled("Mint: ", text_muted),
                    Span::styled(SOLANA_MAINNET_USDC_MINT, mint_style),
                ]),
                Line::from(Span::styled(send_hint, text_muted)),
                Line::from(""),
            ]
            .into_iter()
            .collect();
            f.render_widget(
                Paragraph::new(header)
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: true }),
                chunks[0],
            );

            let qr_spans: Vec<Line> = qr_lines
                .iter()
                .map(|l| {
                    Line::from(Span::styled(
                        *l,
                        Style::default()
                            .fg(Color::Rgb(240, 248, 255))
                            .bg(dialog_bg),
                    ))
                })
                .collect();
            f.render_widget(
                Paragraph::new(qr_spans).alignment(Alignment::Center),
                chunks[1],
            );

            let footer = vec![
                Line::from(Span::styled(svm_address.as_str(), text_main.add_modifier(Modifier::BOLD))),
                Line::from(""),
                Line::from(vec![
                    Span::styled("esc ", text_muted),
                    Span::styled("close  ", text_main),
                    Span::styled("f ", text_muted),
                    Span::styled("refresh", text_main),
                ]),
            ];
            f.render_widget(
                Paragraph::new(footer)
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: false }),
                chunks[2],
            );
        }
    }
}

/// Word-wrap for layout height (long tokens split by character width, like ratatui `Wrap`).
fn wrap_deposit_words_to_lines(text: &str, width: usize) -> Vec<String> {
    let w = width.max(1);
    let mut out: Vec<String> = Vec::new();
    for raw in text.split_whitespace() {
        let wl = raw.chars().count();
        if wl > w {
            for chunk in raw.chars().collect::<Vec<char>>().chunks(w) {
                out.push(chunk.iter().collect());
            }
            continue;
        }
        if out.is_empty() {
            out.push(raw.to_string());
            continue;
        }
        let last = out.last_mut().unwrap();
        if last.chars().count() + 1 + wl <= w {
            last.push(' ');
            last.push_str(raw);
        } else {
            out.push(raw.to_string());
        }
    }
    if out.is_empty() {
        vec![String::new()]
    } else {
        out
    }
}

fn word_wrap_line_count(text: &str, width: usize) -> usize {
    wrap_deposit_words_to_lines(text, width).len().max(1)
}

fn wrap_deposit_error(msg: &str, width: usize) -> Vec<String> {
    let w = width.max(12);
    let mut out = Vec::new();
    for raw in msg.split_whitespace() {
        if out.is_empty() {
            out.push(raw.to_string());
            continue;
        }
        let last = out.last_mut().unwrap();
        if last.chars().count() + 1 + raw.chars().count() <= w {
            last.push(' ');
            last.push_str(raw);
        } else {
            out.push(raw.to_string());
        }
    }
    if out.is_empty() {
        out.push(if msg.is_empty() { "Unknown error".into() } else { msg.to_string() });
    }
    out
}

// ── helpers ─────────────────────────────────────────────────────────
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect { x, y, width: w.min(area.width), height: h.min(area.height) }
}
fn bottom_right_rect(screen: Rect, w: u16, h: u16, margin: u16) -> Rect {
    let w = w.min(screen.width.saturating_sub(margin * 2).max(1));
    let h = h.min(screen.height.saturating_sub(margin * 2).max(1));
    let x = screen.x + screen.width.saturating_sub(w + margin);
    let y = screen.y + screen.height.saturating_sub(h + margin);
    Rect { x, y, width: w, height: h }
}
fn fmt_money(v: f64) -> String {
    fmt_money_decimals(v, 2)
}

fn fmt_money_decimals(v: f64, decimal_places: u32) -> String {
    // e.g. 67_432.51 or 2.3456 (decimal_places = 4)
    let whole = v as i64;
    let mult = 10_f64.powi(decimal_places as i32);
    let max_frac = mult as u64;
    let mut frac = ((v - whole as f64).abs() * mult).round() as u64;
    let mut w = whole;
    if frac >= max_frac {
        w += if v >= 0.0 { 1 } else { -1 };
        frac = 0;
    }
    let mut ws = w.to_string().into_bytes();
    let mut with_commas = Vec::with_capacity(ws.len() + ws.len() / 3);
    let skip_sign = if ws.first() == Some(&b'-') { with_commas.push(b'-'); 1 } else { 0 };
    let digits = &ws[skip_sign..];
    for (i, &c) in digits.iter().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 { with_commas.push(b','); }
        with_commas.push(c);
    }
    let _ = &mut ws;
    format!(
        "{}.{:0width$}",
        String::from_utf8_lossy(&with_commas),
        frac,
        width = decimal_places as usize
    )
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
