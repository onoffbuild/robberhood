//! The exit engine, pure. No clock, no chain, no I/O: a position and a mark go in, a decision comes out.
//! Every rule the JS sniper has, plus the three it lacks: tiered take-profit, a trail that tightens
//! with profit, and a dead-launch stop. Everything is in basis points of cost so it stays integer.

use alloy::primitives::U256;

#[derive(Debug, Clone)]
pub struct Rules {
    /// full exit at this gain, in bps of cost (JS: TAKE_PROFIT_PCT 80 -> 8_000)
    pub take_profit_bps: u64,
    /// full exit at this loss (JS: STOP_LOSS_PCT 35 -> 3_500)
    pub stop_loss_bps: u64,
    /// tiers: at gain >= .0 bps, sell .1 bps of what remains. Taken once each, in order.
    pub tiers: Vec<(u64, u64)>,
    /// trail: once gain >= .0 bps, exit when gain falls .1 bps below the peak. Last matching row wins.
    pub trail: Vec<(u64, u64)>,
    /// trail arms only once the peak has reached this gain (JS: peak >= 10%)
    pub trail_arm_bps: u64,
    /// dead launch: if gain < .1 bps after .0 ms, leave
    pub dead: Option<(u64, u64)>,
    pub max_hold_ms: u64,
    /// exit when the curve is this full, to avoid being trapped between the sweep and the pool
    pub grad_exit_fill_bps: Option<u64>,
}

impl Default for Rules {
    fn default() -> Self {
        Self {
            take_profit_bps: 8_000, stop_loss_bps: 3_500,
            tiers: vec![(10_000, 5_000)],
            trail: vec![(0, 3_000), (20_000, 1_500), (90_000, 1_000)],
            trail_arm_bps: 1_000,
            dead: Some((120_000, 1_500)),
            max_hold_ms: 45 * 60 * 1000,
            grad_exit_fill_bps: Some(9_000),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Position {
    pub cost: U256,
    pub tokens: U256,
    pub opened_at_ms: u64,
    /// best gain seen, in bps of cost, signed
    pub peak_bps: i64,
    pub tiers_taken: usize,
    /// quote already realised by tiers
    pub realised: U256,
}

impl Position {
    pub fn open(cost: U256, tokens: U256, now_ms: u64) -> Self { Self { cost, tokens, opened_at_ms: now_ms, peak_bps: i64::MIN, tiers_taken: 0, realised: U256::ZERO } }
}

#[derive(Debug, Clone, Copy)]
pub struct Mark {
    /// what the remaining tokens fetch right now, after fees
    pub value: U256,
    pub now_ms: u64,
    pub halted: bool,
    /// curve fill toward graduation, bps
    pub fill_bps: Option<u64>,
    pub siren: Option<Siren>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Siren { DevSold, WatchedSold, WatchedTransfer, Rugged }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Hold,
    /// sell this many bps of the remaining tokens, and why
    Sell { bps: u64, why: Why },
    /// wants out but the curve is halted; keep the intent, retry when the pool opens
    Trapped { why: Why },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why { Siren(Siren), TakeProfit, Tier(usize), StopLoss, Trail { peak_bps: i64, width_bps: u64 }, Dead, MaxHold, Graduation }

/// gain in bps of cost, counting what tiers already realised
pub fn gain_bps(p: &Position, value: U256) -> i64 {
    if p.cost.is_zero() { return 0; }
    let total = value + p.realised;
    let num = total * U256::from(10_000u64) / p.cost;
    let n: i64 = num.try_into().unwrap_or(i64::MAX);
    n - 10_000
}

fn si(x: u64) -> i64 { i64::try_from(x).unwrap_or(i64::MAX) }

pub fn decide(rules: &Rules, p: &mut Position, m: &Mark) -> Decision {
    let g = gain_bps(p, m.value);
    if g > p.peak_bps { p.peak_bps = g; }
    let held = m.now_ms.saturating_sub(p.opened_at_ms);
    let why = if let Some(s) = m.siren { Some(Why::Siren(s)) }
        else if g >= si(rules.take_profit_bps) && p.tiers_taken >= rules.tiers.len() { Some(Why::TakeProfit) }
        else if let Some(i) = next_tier(rules, p, g) { return sell_bps(m, rules.tiers[i].1, Why::Tier(i)); }
        else if g <= -si(rules.stop_loss_bps) { Some(Why::StopLoss) }
        else if let Some(w) = trail_width(rules, p.peak_bps) { if p.peak_bps >= si(rules.trail_arm_bps) && g <= p.peak_bps.saturating_sub(si(w)) { Some(Why::Trail { peak_bps: p.peak_bps, width_bps: w }) } else { None } }
        else { None };
    let why = why.or_else(|| match rules.dead { Some((ms, min)) if held >= ms && g < si(min) => Some(Why::Dead), _ => None })
        .or_else(|| if held >= rules.max_hold_ms { Some(Why::MaxHold) } else { None })
        .or_else(|| match (rules.grad_exit_fill_bps, m.fill_bps) { (Some(x), Some(f)) if f >= x => Some(Why::Graduation), _ => None });
    match why { Some(w) => sell_bps(m, 10_000, w), None => Decision::Hold }
}

fn sell_bps(m: &Mark, bps: u64, why: Why) -> Decision { if m.halted { Decision::Trapped { why } } else { Decision::Sell { bps, why } } }

fn next_tier(rules: &Rules, p: &Position, g: i64) -> Option<usize> {
    let i = p.tiers_taken;
    rules.tiers.get(i).filter(|(at, _)| g >= si(*at)).map(|_| i)
}

fn trail_width(rules: &Rules, peak: i64) -> Option<u64> {
    rules.trail.iter().filter(|(from, _)| peak >= si(*from)).last().map(|(_, w)| *w)
}

/// Book a partial fill so the next decision sees the new cost basis and tier count.
pub fn book_sell(p: &mut Position, tokens_sold: U256, quote_out: U256, was_tier: bool) {
    p.tokens = p.tokens.saturating_sub(tokens_sold);
    p.realised += quote_out;
    if was_tier { p.tiers_taken += 1; }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn u(x: u64) -> U256 { U256::from(x) }
    fn pos() -> Position { Position::open(u(1_000), u(1_000_000), 0) }
    fn mark(v: u64, t: u64) -> Mark { Mark { value: u(v), now_ms: t, halted: false, fill_bps: None, siren: None } }
    #[test]
    fn tier_takes_half_at_double_then_tp_takes_the_rest() {
        let r = Rules::default(); let mut p = pos();
        assert_eq!(decide(&r, &mut p, &mark(1_500, 1)), Decision::Hold);
        assert_eq!(decide(&r, &mut p, &mark(2_000, 2)), Decision::Sell { bps: 5_000, why: Why::Tier(0) });
        book_sell(&mut p, u(500_000), u(1_000), true);
        // remaining half worth 1_000 + realised 1_000 = +100 %, past +80 % and no tiers left: full exit
        assert_eq!(decide(&r, &mut p, &mark(1_000, 3)), Decision::Sell { bps: 10_000, why: Why::TakeProfit });
    }
    #[test]
    fn trail_tightens_with_profit() {
        let r = Rules::default(); let mut p = pos();
        decide(&r, &mut p, &mark(1_190, 1)); // peak +19 %, armed, width 30 %
        assert_eq!(decide(&r, &mut p, &mark(1_000, 2)), Decision::Hold); // -19 from peak, inside 30
        assert!(matches!(decide(&r, &mut p, &mark(880, 3)), Decision::Sell { why: Why::Trail { width_bps: 3_000, .. }, .. }));
        let r = Rules { take_profit_bps: u64::MAX, tiers: vec![], ..Rules::default() }; // isolate the trail
        let mut p = pos();
        decide(&r, &mut p, &mark(4_000, 1)); // peak +300 %, width 15 %
        assert_eq!(decide(&r, &mut p, &mark(3_900, 2)), Decision::Hold);
        assert!(matches!(decide(&r, &mut p, &mark(3_800, 3)), Decision::Sell { why: Why::Trail { width_bps: 1_500, .. }, .. }));
    }
    #[test]
    fn stop_loss_and_dead_launch() {
        let r = Rules::default(); let mut p = pos();
        assert_eq!(decide(&r, &mut p, &mark(700, 1)), Decision::Hold);
        assert_eq!(decide(&r, &mut p, &mark(640, 2)), Decision::Sell { bps: 10_000, why: Why::StopLoss });
        let mut p = pos();
        assert_eq!(decide(&r, &mut p, &mark(1_050, 119_000)), Decision::Hold);
        assert_eq!(decide(&r, &mut p, &mark(1_050, 121_000)), Decision::Sell { bps: 10_000, why: Why::Dead });
        assert_eq!(decide(&r, &mut p, &mark(1_200, 121_000)), Decision::Hold);
    }
    #[test]
    fn siren_beats_everything_and_halt_traps() {
        let r = Rules::default(); let mut p = pos();
        let mut m = mark(5_000, 1); m.siren = Some(Siren::DevSold);
        assert_eq!(decide(&r, &mut p, &m), Decision::Sell { bps: 10_000, why: Why::Siren(Siren::DevSold) });
        m.halted = true;
        assert_eq!(decide(&r, &mut p, &m), Decision::Trapped { why: Why::Siren(Siren::DevSold) });
    }
    #[test]
    fn graduation_exit() {
        let r = Rules::default(); let mut p = pos();
        let mut m = mark(1_100, 1); m.fill_bps = Some(9_200);
        assert_eq!(decide(&r, &mut p, &m), Decision::Sell { bps: 10_000, why: Why::Graduation });
    }
}
