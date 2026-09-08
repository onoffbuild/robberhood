//! The tax clock. The opening tax is deterministic from the launch block timestamp
//! (snipeTaxStartBps at t=0, linear to 0 over snipeTaxSeconds). Nobody needs to poll it.
//! Compute when it crosses your ceiling and fire so the tx arrives at the sequencer then.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy)]
pub struct TaxCurve { pub start_bps: u64, pub seconds: u64 }
impl Default for TaxCurve { fn default() -> Self { Self { start_bps: 9_900, seconds: 3 } } }

impl TaxCurve {
    /// tax in bps at `ms` milliseconds after launch, linear decay
    pub fn at_ms(&self, ms: u64) -> u64 {
        let total = self.seconds * 1000;
        if ms >= total { return 0; }
        self.start_bps - self.start_bps * ms / total
    }
    /// milliseconds after launch at which the tax is at or below `ceiling_bps`
    pub fn crossing_ms(&self, ceiling_bps: u64) -> u64 {
        if ceiling_bps >= self.start_bps { return 0; }
        let total = self.seconds * 1000;
        // smallest ms with start - start*ms/total <= ceiling  ->  ms >= total*(start-ceiling)/start
        (total * (self.start_bps - ceiling_bps)).div_ceil(self.start_bps)
    }
    /// when to write the bytes to the socket: crossing time minus the one-way wire estimate.
    /// Block timestamps are whole seconds, so the launch instant inside the second is unknown;
    /// the feed's local receive time of the launch block is the better anchor and is what we use.
    pub fn fire_at(&self, launch_seen: std::time::Instant, ceiling_bps: u64, wire: Duration) -> std::time::Instant {
        launch_seen + Duration::from_millis(self.crossing_ms(ceiling_bps)) - wire
    }
}

pub fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64 }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crossing() {
        let c = TaxCurve::default();
        assert_eq!(c.at_ms(0), 9_900);
        assert_eq!(c.at_ms(3000), 0);
        let x = c.crossing_ms(300);
        assert!(c.at_ms(x) <= 300 && c.at_ms(x - 1) > 300, "x={x}");
        assert_eq!(c.crossing_ms(9_900), 0);
    }
}
