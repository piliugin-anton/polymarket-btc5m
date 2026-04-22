//! Key event handling.
//!
//! `handle_key` returns an `Action` the runtime should dispatch. Keeping this
//! pure (no I/O) makes the key logic unit-testable.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

use crate::app::{AppState, DepositModalPhase, InputMode, LimitField, Outcome, UiPhase, MIN_LIMIT_ORDER_SHARES};
use crate::market_profile::{MarketProfile, Timeframe};
use crate::trading::Side;

#[derive(Debug)]
pub enum Action {
    None,
    Quit,
    /// Market: USDC notional (BUY spend). SELL dumps full tracked position; USDC only if position is 0.
    PlaceMarket { outcome: Outcome, side: Side, size_usdc: f64 },
    /// Limit: BUY size = USDC notional; SELL size = **shares**.
    PlaceLimit  { outcome: Outcome, side: Side, price: f64, size_usdc: f64 },
    CancelAll,
    ForceMarketRoll,
    /// Hint for claiming resolved positions (Data API claimable positive). Redeem is on-chain / Portfolio, not CLOB HTTP.
    Claim,
    /// `POST` Polymarket Bridge `/deposit` for `POLYMARKET_FUNDER` → Solana address + QR.
    FetchSolanaDeposit,
    /// After wizard: `main` dispatches `AppEvent::StartTrading` and spawns RTDS + Gamma.
    StartTrading(Arc<MarketProfile>),
}

/// When the [Kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/) is
/// enabled (`PushKeyboardEnhancementFlags`), `KeyEvent.state` includes lock-key bits from the
/// terminal. If Caps Lock was toggled while the terminal pane was unfocused, some emulators emit
/// uppercase letters with **no** `CAPS_LOCK` in the mask (stale output) — treat that like lowercase
/// for our `w`/`s`/`a`/`d` quick-trade bindings. When `state` is empty (legacy mode), keep the
/// character as-is; handlers use case-insensitive matching so `W`/`A`/etc. still work.
fn normalize_terminal_key_event(k: KeyEvent) -> KeyEvent {
    let KeyCode::Char(c) = k.code else {
        return k;
    };
    if !c.is_ascii_uppercase() {
        return k;
    }
    if k.modifiers.contains(KeyModifiers::SHIFT) {
        return k;
    }
    if k.state.is_empty() {
        return k;
    }
    if k.state.contains(KeyEventState::CAPS_LOCK) {
        return k;
    }
    KeyEvent::new_with_kind_and_state(
        KeyCode::Char(c.to_ascii_lowercase()),
        k.modifiers,
        k.kind,
        k.state,
    )
}

pub fn handle_key(state: &mut AppState, k: KeyEvent) -> Action {
    let k = normalize_terminal_key_event(k);
    // Ctrl-C / Ctrl-Q always quits
    if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('q')) {
        return Action::Quit;
    }

    if state.ui_phase != UiPhase::Trading {
        return handle_wizard_key(state, k);
    }

    if state.deposit_modal.is_some() {
        return deposit_modal_key(state, k);
    }

    match state.input_mode {
        InputMode::Normal    => normal_mode(state, k),
        InputMode::EditSize  => edit_size_mode(state, k),
        InputMode::LimitModal { outcome, side, field } => limit_mode(state, k, outcome, side, field),
    }
}

/// Leave the trading screen and show the wizard’s 5m / 15m time-window list for the current asset.
fn go_to_wizard_timeframe(state: &mut AppState) {
    if let Some(ref profile) = state.market_profile {
        if let Some(i) = state.wizard_rows.iter().position(|r| r.asset == profile.asset) {
            state.wizard_list_idx = i;
        }
        state.wizard_tf_idx = match profile.timeframe {
            Timeframe::M5 => 0,
            Timeframe::M15 => 1,
            // Wizard only offers rolling 5m / 15m; daily users land on 5m highlight.
            Timeframe::D1 => 0,
        };
    }
    state.ui_phase = UiPhase::WizardPickTimeframe;
    state.status_line = "Timeframe: 5m / 15m (↑/↓ Enter) — B back".into();
}

fn normal_mode(state: &mut AppState, k: KeyEvent) -> Action {
    // Avoid placing repeated market orders when a key is held (we now forward `Repeat`).
    if k.kind == KeyEventKind::Repeat {
        return Action::None;
    }
    let size = state.current_size();
    match k.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Esc => {
            go_to_wizard_timeframe(state);
            Action::None
        }
        // Quick market: WASD-style — W/S buy UP/DOWN, A/D sell UP/DOWN
        KeyCode::Char(c) if c.eq_ignore_ascii_case(&'w') => Action::PlaceMarket { outcome: Outcome::Up,   side: Side::Buy,  size_usdc: size },
        KeyCode::Char(c) if c.eq_ignore_ascii_case(&'s') => Action::PlaceMarket { outcome: Outcome::Down, side: Side::Buy,  size_usdc: size },
        KeyCode::Char(c) if c.eq_ignore_ascii_case(&'a') => Action::PlaceMarket { outcome: Outcome::Up,   side: Side::Sell, size_usdc: size },
        KeyCode::Char(c) if c.eq_ignore_ascii_case(&'d') => Action::PlaceMarket { outcome: Outcome::Down, side: Side::Sell, size_usdc: size },

        // Cancel all
        KeyCode::Char('c') => Action::CancelAll,

        // Open limit modal — default BUY UP; change outcome/side inside modal (←/→, ↑/↓, w/s/a/d).
        KeyCode::Char('l') => {
            state.limit_price_input.clear();
            state.limit_size_input = state.size_input.clone();
            state.input_mode = InputMode::LimitModal {
                outcome: Outcome::Up, side: Side::Buy, field: LimitField::Price,
            };
            Action::None
        }

        // Edit persistent size
        KeyCode::Char(c) if c.eq_ignore_ascii_case(&'e') => {
            state.input_mode = InputMode::EditSize;
            Action::None
        }

        // Manual force-roll (useful if gamma polling hasn't picked up the next market yet)
        KeyCode::Char('r') => Action::ForceMarketRoll,

        // CTF redeem via relayer (Safe) — see `spawn_claim` + `redeem`
        KeyCode::Char('x') | KeyCode::Char('X') => Action::Claim,

        // Polymarket Bridge — Solana USDC deposit address + QR
        KeyCode::Char('f') => {
            state.deposit_modal = Some(DepositModalPhase::Loading);
            Action::FetchSolanaDeposit
        }

        _ => Action::None,
    }
}

fn handle_wizard_key(state: &mut AppState, k: KeyEvent) -> Action {
    if k.kind == KeyEventKind::Repeat {
        return Action::None;
    }
    if k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('q')) {
        return Action::Quit;
    }
    if matches!(k.code, KeyCode::Char('q')) {
        return Action::Quit;
    }
    match state.ui_phase {
        UiPhase::WizardLoading => {
            if matches!(k.code, KeyCode::Esc) {
                return Action::Quit;
            }
            Action::None
        }
        UiPhase::WizardPickAsset => {
            if matches!(k.code, KeyCode::Esc) {
                return Action::Quit;
            }
            if state.wizard_rows.is_empty() {
                return Action::None;
            }
            let n = state.wizard_rows.len();
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    state.wizard_list_idx = state.wizard_list_idx.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    state.wizard_list_idx = (state.wizard_list_idx + 1).min(n - 1);
                }
                KeyCode::Enter | KeyCode::Char('\r') | KeyCode::Char('\n') => {
                    state.ui_phase = UiPhase::WizardPickTimeframe;
                    state.wizard_tf_idx = 0;
                    state.status_line = "Timeframe: 5m / 15m (↑/↓ Enter) — B back".into();
                }
                _ => {}
            }
            Action::None
        }
        UiPhase::WizardPickTimeframe => {
            if state.wizard_rows.is_empty() {
                return Action::None;
            }
            let n_tf = 2;
            match k.code {
                KeyCode::Esc | KeyCode::Char('b') | KeyCode::Char('B') => {
                    state.ui_phase = UiPhase::WizardPickAsset;
                    state.status_line = "Select asset (↑/↓ Enter) — Q quit".into();
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    state.wizard_tf_idx = state.wizard_tf_idx.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    state.wizard_tf_idx = (state.wizard_tf_idx + 1).min(n_tf - 1);
                }
                KeyCode::Enter | KeyCode::Char('\r') | KeyCode::Char('\n') => {
                    let asset = state.wizard_rows[state.wizard_list_idx].asset.clone();
                    let tf = match state.wizard_tf_idx.min(1) {
                        0 => Timeframe::M5,
                        _ => Timeframe::M15,
                    };
                    let p = Arc::new(MarketProfile { asset, timeframe: tf });
                    return Action::StartTrading(p);
                }
                _ => {}
            }
            Action::None
        }
        UiPhase::Trading => Action::None,
    }
}

fn deposit_modal_key(state: &mut AppState, k: KeyEvent) -> Action {
    if k.kind == KeyEventKind::Repeat {
        return Action::None;
    }
    match k.code {
        KeyCode::Esc => {
            state.deposit_modal = None;
            Action::None
        }
        KeyCode::Char('f') => {
            if matches!(state.deposit_modal, Some(DepositModalPhase::Loading)) {
                return Action::None;
            }
            state.deposit_modal = Some(DepositModalPhase::Loading);
            Action::FetchSolanaDeposit
        }
        _ => Action::None,
    }
}

fn edit_size_mode(state: &mut AppState, k: KeyEvent) -> Action {
    match k.code {
        // Many terminals send CR/LF (`\r` / `\n`) for Return instead of `KeyCode::Enter`.
        // With keyboard-event kinds enabled, confirm on `Press` or `Release` (see `spawn_key_reader`).
        KeyCode::Enter | KeyCode::Char('\r') | KeyCode::Char('\n') | KeyCode::Esc => {
            // Validate: keep old value if parse fails
            if state.size_input.parse::<f64>().is_err() {
                state.size_input = format!("{:.2}", state.default_size_usdc);
            }
            state.input_mode = InputMode::Normal;
        }
        KeyCode::Char('x') | KeyCode::Char('X') => {
            if state.size_input.parse::<f64>().is_err() {
                state.size_input = format!("{:.2}", state.default_size_usdc);
            }
            state.input_mode = InputMode::Normal;
            return Action::Claim;
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
        KeyCode::Char('x') | KeyCode::Char('X') => {
            state.input_mode = InputMode::Normal;
            Action::Claim
        }
        KeyCode::Esc => { state.input_mode = InputMode::Normal; Action::None }
        // Same quick keys as normal mode: w/s buy UP/DOWN, a/d sell UP/DOWN
        KeyCode::Char(c) if c.eq_ignore_ascii_case(&'w') => {
            state.input_mode = InputMode::LimitModal {
                outcome: Outcome::Up, side: Side::Buy, field,
            };
            Action::None
        }
        KeyCode::Char(c) if c.eq_ignore_ascii_case(&'s') => {
            state.input_mode = InputMode::LimitModal {
                outcome: Outcome::Down, side: Side::Buy, field,
            };
            Action::None
        }
        KeyCode::Char(c) if c.eq_ignore_ascii_case(&'a') => {
            state.input_mode = InputMode::LimitModal {
                outcome: Outcome::Up, side: Side::Sell, field,
            };
            Action::None
        }
        KeyCode::Char(c) if c.eq_ignore_ascii_case(&'d') => {
            state.input_mode = InputMode::LimitModal {
                outcome: Outcome::Down, side: Side::Sell, field,
            };
            Action::None
        }
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
            if k.kind == KeyEventKind::Repeat {
                return Action::None;
            }
            let price = state.limit_price_input.parse::<f64>();
            let size  = state.limit_size_input.parse::<f64>();
            match (price, size) {
                (Ok(p), Ok(sz)) if (0.01..=0.99).contains(&p) && sz > 0.0 => {
                    let shares = match side {
                        Side::Buy => sz / p,
                        Side::Sell => sz,
                    };
                    if shares + 1e-9 < MIN_LIMIT_ORDER_SHARES {
                        state.status_line = format!(
                            "limit: min {:.0} shares (≈{:.2} now)",
                            MIN_LIMIT_ORDER_SHARES, shares
                        );
                        return Action::None;
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crossterm::event::{KeyEvent, KeyModifiers};

    use crate::app::UiPhase;
    use crate::feeds::user_trade_sync::UserTradeSync;

    fn test_state() -> AppState {
        let mut s = AppState::new(5.0, Arc::new(UserTradeSync::new()));
        s.ui_phase = UiPhase::Trading;
        s
    }

    #[test]
    fn uppercase_without_caps_in_protocol_state_maps_to_lowercase() {
        let mut state = test_state();
        let ev = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('W'),
            KeyModifiers::NONE,
            KeyEventKind::Press,
            KeyEventState::NUM_LOCK,
        );
        assert!(matches!(
            handle_key(&mut state, ev),
            Action::PlaceMarket { outcome: Outcome::Up, side: Side::Buy, .. }
        ));
    }

    #[test]
    fn uppercase_with_caps_lock_unchanged() {
        let mut state = test_state();
        let ev = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('A'),
            KeyModifiers::NONE,
            KeyEventKind::Press,
            KeyEventState::CAPS_LOCK,
        );
        assert!(matches!(
            handle_key(&mut state, ev),
            Action::PlaceMarket { outcome: Outcome::Up, side: Side::Sell, .. }
        ));
    }

    #[test]
    fn normal_mode_ignores_key_repeat() {
        let mut state = test_state();
        let ev = KeyEvent::new_with_kind(KeyCode::Char('w'), KeyModifiers::NONE, KeyEventKind::Repeat);
        assert!(matches!(handle_key(&mut state, ev), Action::None));
    }

    #[test]
    fn edit_size_accepts_enter_release() {
        let mut state = test_state();
        state.input_mode = InputMode::EditSize;
        let ev = KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release);
        assert!(matches!(handle_key(&mut state, ev), Action::None));
        assert!(matches!(state.input_mode, InputMode::Normal));
    }
}
