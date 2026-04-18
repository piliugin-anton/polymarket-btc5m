//! Key event handling.
//!
//! `handle_key` returns an `Action` the runtime should dispatch. Keeping this
//! pure (no I/O) makes the key logic unit-testable.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{AppState, InputMode, LimitField, Outcome};
use crate::trading::Side;

#[derive(Debug)]
pub enum Action {
    None,
    Quit,
    /// Market: BUY `size_usdc` = USDC to spend; SELL = **shares** to close.
    PlaceMarket { outcome: Outcome, side: Side, size_usdc: f64 },
    /// Limit: BUY size = USDC notional; SELL size = **shares**.
    PlaceLimit  { outcome: Outcome, side: Side, price: f64, size_usdc: f64 },
    CancelAll,
    ForceMarketRoll,
}

pub fn handle_key(state: &mut AppState, k: KeyEvent) -> Action {
    // Ctrl-C / Ctrl-Q always quits
    if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('q')) {
        return Action::Quit;
    }

    match state.input_mode {
        InputMode::Normal    => normal_mode(state, k),
        InputMode::EditSize  => edit_size_mode(state, k),
        InputMode::LimitModal { outcome, side, field } => limit_mode(state, k, outcome, side, field),
    }
}

fn normal_mode(state: &mut AppState, k: KeyEvent) -> Action {
    let size = state.current_size();
    match k.code {
        KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
        // Quick market orders — lowercase buys, uppercase sells, matching convention
        KeyCode::Char('u') => Action::PlaceMarket { outcome: Outcome::Up,   side: Side::Buy,  size_usdc: size },
        KeyCode::Char('d') => Action::PlaceMarket { outcome: Outcome::Down, side: Side::Buy,  size_usdc: size },
        KeyCode::Char('U') => Action::PlaceMarket { outcome: Outcome::Up,   side: Side::Sell, size_usdc: size },
        KeyCode::Char('D') => Action::PlaceMarket { outcome: Outcome::Down, side: Side::Sell, size_usdc: size },

        // Cancel all
        KeyCode::Char('c') => Action::CancelAll,

        // Open limit modal — pick an outcome+side by following up with one key
        KeyCode::Char('l') => {
            // Default to BUY UP; user tabs through fields. Starts empty.
            state.limit_price_input.clear();
            state.limit_size_input = state.size_input.clone();
            state.input_mode = InputMode::LimitModal {
                outcome: Outcome::Up, side: Side::Buy, field: LimitField::Price,
            };
            Action::None
        }

        // Edit persistent size
        KeyCode::Char('s') => {
            state.input_mode = InputMode::EditSize;
            Action::None
        }

        // Manual force-roll (useful if gamma polling hasn't picked up the next market yet)
        KeyCode::Char('r') => Action::ForceMarketRoll,

        _ => Action::None,
    }
}

fn edit_size_mode(state: &mut AppState, k: KeyEvent) -> Action {
    match k.code {
        // Many terminals send CR/LF (`\r` / `\n`) for Return instead of `KeyCode::Enter`.
        KeyCode::Enter | KeyCode::Char('\r') | KeyCode::Char('\n') | KeyCode::Esc => {
            // Validate: keep old value if parse fails
            if state.size_input.parse::<f64>().is_err() {
                state.size_input = format!("{:.2}", state.default_size_usdc);
            }
            state.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => { state.size_input.pop(); }
        KeyCode::Char(c) if c.is_ascii_digit() || c == '.' => {
            if state.size_input.len() < 10 { state.size_input.push(c); }
        }
        _ => {}
    }
    Action::None
}

fn limit_mode(state: &mut AppState, k: KeyEvent, outcome: Outcome, side: Side, field: LimitField)
    -> Action
{
    match k.code {
        KeyCode::Esc => { state.input_mode = InputMode::Normal; Action::None }
        KeyCode::Tab => {
            state.input_mode = InputMode::LimitModal {
                outcome, side,
                field: match field { LimitField::Price => LimitField::Size, LimitField::Size => LimitField::Price },
            };
            Action::None
        }
        // Flip side/outcome with arrows so you don't have to close & reopen the modal
        KeyCode::Left | KeyCode::Right => {
            state.input_mode = InputMode::LimitModal {
                outcome: outcome.opposite(), side, field,
            };
            Action::None
        }
        KeyCode::Up | KeyCode::Down => {
            state.input_mode = InputMode::LimitModal {
                outcome,
                side: match side { Side::Buy => Side::Sell, Side::Sell => Side::Buy },
                field,
            };
            Action::None
        }
        KeyCode::Enter | KeyCode::Char('\r') | KeyCode::Char('\n') => {
            let price = state.limit_price_input.parse::<f64>();
            let size  = state.limit_size_input.parse::<f64>();
            match (price, size) {
                (Ok(p), Ok(sz)) if (0.01..=0.99).contains(&p) && sz > 0.0 => {
                    state.input_mode = InputMode::Normal;
                    Action::PlaceLimit { outcome, side, price: p, size_usdc: sz }
                }
                _ => {
                    state.status_line = "limit: need price ∈ (0.01, 0.99) and size > 0".into();
                    Action::None
                }
            }
        }
        KeyCode::Backspace => {
            let buf = match field {
                LimitField::Price => &mut state.limit_price_input,
                LimitField::Size  => &mut state.limit_size_input,
            };
            buf.pop();
            Action::None
        }
        KeyCode::Char(c) if c.is_ascii_digit() || c == '.' => {
            let buf = match field {
                LimitField::Price => &mut state.limit_price_input,
                LimitField::Size  => &mut state.limit_size_input,
            };
            if buf.len() < 10 { buf.push(c); }
            Action::None
        }
        _ => Action::None,
    }
}
