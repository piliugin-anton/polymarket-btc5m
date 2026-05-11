const MIN_PROB_PRICE: f64 = 0.01;
const MAX_PROB_PRICE: f64 = 0.99;
const STOP_LOSS_LIMIT_OFFSET: f64 = 0.01;

pub(crate) fn stop_loss_triggered(reference_price: f64, current_price: f64, stop_loss_bps: u32) -> bool {
    if stop_loss_bps == 0
        || !reference_price.is_finite()
        || reference_price <= 0.0
        || !current_price.is_finite()
    {
        return false;
    }

    let drawdown = (reference_price - current_price) / reference_price;
    drawdown + 1e-12 >= stop_loss_bps as f64 / 10_000.0
}

pub(crate) fn stop_loss_sell_limit_price(current_price: f64) -> f64 {
    if !current_price.is_finite() {
        return MIN_PROB_PRICE;
    }
    (current_price - STOP_LOSS_LIMIT_OFFSET).clamp(MIN_PROB_PRICE, MAX_PROB_PRICE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_loss_disabled_at_zero_bps() {
        assert!(!stop_loss_triggered(0.50, 0.40, 0));
    }

    #[test]
    fn stop_loss_triggers_exactly_at_threshold() {
        assert!(stop_loss_triggered(0.50, 0.45, 1_000));
    }

    #[test]
    fn stop_loss_does_not_trigger_above_threshold() {
        assert!(!stop_loss_triggered(0.50, 0.46, 1_000));
    }

    #[test]
    fn stop_loss_sell_limit_subtracts_one_cent_and_clamps() {
        assert!((stop_loss_sell_limit_price(0.55) - 0.54).abs() < 1e-12);
        assert!((stop_loss_sell_limit_price(0.015) - 0.01).abs() < 1e-12);
    }
}
