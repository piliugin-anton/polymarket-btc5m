use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::feeds::clob_ws::{BookSnapshot, ClobTradePrint};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarketDataCoalescerStats {
    pub book_overwrites: u64,
    pub trade_drops: u64,
}

#[derive(Debug, Default)]
pub struct MarketDataBatch {
    pub books: Vec<BookSnapshot>,
    pub trades: Vec<ClobTradePrint>,
}

#[derive(Debug)]
struct Inner {
    latest_books: BTreeMap<String, BookSnapshot>,
    trades: VecDeque<ClobTradePrint>,
    max_trades: usize,
    stats: MarketDataCoalescerStats,
}

#[derive(Debug, Clone)]
pub struct MarketDataCoalescer {
    inner: Arc<Mutex<Inner>>,
}

impl MarketDataCoalescer {
    pub fn new(max_trades: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                latest_books: BTreeMap::new(),
                trades: VecDeque::with_capacity(max_trades),
                max_trades,
                stats: MarketDataCoalescerStats::default(),
            })),
        }
    }

    pub fn submit_book(&self, snapshot: BookSnapshot) {
        let mut inner = self
            .inner
            .lock()
            .expect("market-data coalescer mutex poisoned");
        if inner
            .latest_books
            .insert(snapshot.asset_id.clone(), snapshot)
            .is_some()
        {
            inner.stats.book_overwrites += 1;
        }
    }

    pub fn submit_trade(&self, trade: ClobTradePrint) {
        let mut inner = self
            .inner
            .lock()
            .expect("market-data coalescer mutex poisoned");
        if inner.max_trades == 0 {
            inner.stats.trade_drops += 1;
            return;
        }
        while inner.trades.len() >= inner.max_trades {
            inner.trades.pop_front();
            inner.stats.trade_drops += 1;
        }
        inner.trades.push_back(trade);
    }

    pub fn drain(&self) -> MarketDataBatch {
        let mut inner = self
            .inner
            .lock()
            .expect("market-data coalescer mutex poisoned");
        MarketDataBatch {
            books: std::mem::take(&mut inner.latest_books)
                .into_values()
                .collect(),
            trades: inner.trades.drain(..).collect(),
        }
    }

    #[cfg(test)]
    pub fn stats(&self) -> MarketDataCoalescerStats {
        self.inner
            .lock()
            .expect("market-data coalescer mutex poisoned")
            .stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feeds::clob_ws::{BookLevel, BookSnapshot, ClobTradePrint};

    fn book(asset_id: &str, bid: f64) -> BookSnapshot {
        BookSnapshot {
            asset_id: asset_id.to_string(),
            bids: vec![BookLevel {
                price: bid,
                size: 10.0,
            }],
            asks: vec![BookLevel {
                price: bid + 0.02,
                size: 11.0,
            }],
        }
    }

    fn trade(asset_id: &str, ts_ms: i64) -> ClobTradePrint {
        ClobTradePrint {
            asset_id: asset_id.to_string(),
            price: 0.55,
            size: 7.0,
            ts_ms,
        }
    }

    #[test]
    fn latest_book_per_asset_wins_and_counts_overwrites() {
        let coalescer = MarketDataCoalescer::new(8);

        coalescer.submit_book(book("UP", 0.50));
        coalescer.submit_book(book("DOWN", 0.40));
        coalescer.submit_book(book("UP", 0.51));

        let batch = coalescer.drain();
        assert_eq!(batch.books.len(), 2);
        assert_eq!(
            batch
                .books
                .iter()
                .find(|b| b.asset_id == "UP")
                .unwrap()
                .bids[0]
                .price,
            0.51
        );
        assert_eq!(
            batch
                .books
                .iter()
                .find(|b| b.asset_id == "DOWN")
                .unwrap()
                .bids[0]
                .price,
            0.40
        );
        assert_eq!(coalescer.stats().book_overwrites, 1);
    }

    #[test]
    fn trades_are_batched_and_oldest_are_dropped_when_capacity_is_exceeded() {
        let coalescer = MarketDataCoalescer::new(2);

        coalescer.submit_trade(trade("UP", 1));
        coalescer.submit_trade(trade("UP", 2));
        coalescer.submit_trade(trade("UP", 3));

        let batch = coalescer.drain();
        assert_eq!(
            batch.trades.iter().map(|p| p.ts_ms).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(coalescer.stats().trade_drops, 1);
    }
}
