//! Pons v2 curve maths, integer, to the wei. Mirrors cli/trade.js exactly so a Rust quote and a
//! JS quote never disagree by a wei. Also tracks reserves locally from feed events between resyncs.

use alloy::primitives::U256;

pub const BPS: u64 = 10_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Curve {
    pub quote_reserve: U256,
    pub token_reserve: U256,
    pub fee_bps: u64,
    pub creator_tax_bps: u64,
    /// tokens still sellable by the curve; None when the contract has no such cap
    pub sellable: Option<U256>,
    /// set from LaunchSwept: the curve is closed, the pool is not there yet. Nothing can be sold.
    pub halted: bool,
    pub graduated: bool,
}

pub fn amount_out(inp: U256, reserve_in: U256, reserve_out: U256) -> U256 {
    if inp.is_zero() { return U256::ZERO; }
    inp * reserve_out / (reserve_in + inp)
}
pub fn amount_in(out: U256, reserve_in: U256, reserve_out: U256) -> U256 {
    out * reserve_in / (reserve_out - out) + U256::from(1u64)
}
fn bps(x: U256, b: u64) -> U256 { x * U256::from(b) / U256::from(BPS) }

#[derive(Debug, Clone, Copy, Default)]
pub struct BuyQuote { pub tokens_out: U256, pub spent: U256, pub net: U256, pub capped: bool }
#[derive(Debug, Clone, Copy, Default)]
pub struct SellQuote { pub quote_out: U256, pub gross: U256, pub fee: U256, pub tax: U256 }

impl Curve {
    pub fn quote_buy(&self, spent: U256, opening_tax_bps: u64) -> BuyQuote {
        let cut = self.fee_bps + self.creator_tax_bps + opening_tax_bps;
        if cut >= BPS { return BuyQuote::default(); }
        let net = spent - bps(spent, self.fee_bps) - bps(spent, self.creator_tax_bps) - bps(spent, opening_tax_bps);
        if net.is_zero() { return BuyQuote::default(); }
        let mut tokens_out = amount_out(net, self.quote_reserve, self.token_reserve);
        let (mut gross, mut capped) = (spent, false);
        if let Some(cap) = self.sellable {
            if tokens_out > cap {
                tokens_out = cap; capped = true;
                let need = amount_in(cap, self.quote_reserve, self.token_reserve);
                let d = U256::from(BPS - cut);
                gross = (need * U256::from(BPS) + d - U256::from(1u64)) / d;
            }
        }
        BuyQuote { tokens_out, spent: gross, net: if capped { amount_in(tokens_out, self.quote_reserve, self.token_reserve) } else { net }, capped }
    }

    pub fn quote_sell(&self, tokens_in: U256) -> SellQuote {
        if tokens_in.is_zero() || self.halted { return SellQuote::default(); }
        let gross = amount_out(tokens_in, self.token_reserve, self.quote_reserve);
        let fee = bps(gross, self.fee_bps); let tax = bps(gross, self.creator_tax_bps);
        let q = gross.saturating_sub(fee).saturating_sub(tax);
        SellQuote { quote_out: q, gross, fee, tax }
    }

    /// Advance the local reserve estimate for a buy seen in the feed. Exact when the opening tax
    /// guess is right; a resync from the node corrects any drift.
    pub fn apply_buy(&mut self, spent: U256, opening_tax_bps: u64) -> BuyQuote {
        let q = self.quote_buy(spent, opening_tax_bps);
        self.quote_reserve += q.net; self.token_reserve -= q.tokens_out;
        if let Some(c) = self.sellable.as_mut() { *c -= q.tokens_out; }
        q
    }
    pub fn apply_sell(&mut self, tokens_in: U256) -> SellQuote {
        let q = self.quote_sell(tokens_in);
        self.quote_reserve -= q.gross; self.token_reserve += tokens_in;
        if let Some(c) = self.sellable.as_mut() { *c += tokens_in; }
        q
    }
    pub fn resync(&mut self, quote_reserve: U256, token_reserve: U256) { self.quote_reserve = quote_reserve; self.token_reserve = token_reserve; }

    /// the largest buy that moves the price less than `impact_bps`: the JS "safe door" size
    pub fn safe_door(&self, impact_bps: u64) -> U256 {
        // price impact of a buy x on x*y=k is x/(R+x); solve x = R*i/(1-i)
        self.quote_reserve * U256::from(impact_bps) / U256::from(BPS - impact_bps)
    }
}

pub fn min_out(amount: U256, slippage_bps: u64) -> U256 { amount * U256::from(BPS - slippage_bps) / U256::from(BPS) }

#[cfg(test)]
mod tests {
    use super::*;
    fn eth(x: u64) -> U256 { U256::from(x) * U256::from(10u64).pow(U256::from(18u64)) }
    fn c() -> Curve { Curve { quote_reserve: eth(10), token_reserve: eth(1_000_000), fee_bps: 100, creator_tax_bps: 200, sellable: None, halted: false, graduated: false } }
    #[test]
    fn buy_then_sell_round_trips_below_cost() {
        let mut k = c();
        let b = k.apply_buy(eth(1), 0);
        assert!(b.tokens_out > U256::ZERO);
        let s = k.apply_sell(b.tokens_out);
        assert!(s.quote_out < eth(1), "fees and impact must lose money on a round trip");
        assert!(s.quote_out > eth(1) * U256::from(90u64) / U256::from(100u64));
    }
    #[test]
    fn halted_curve_quotes_zero() { let mut k = c(); k.halted = true; assert_eq!(k.quote_sell(eth(1)).quote_out, U256::ZERO); }
    #[test]
    fn safe_door_is_under_impact() {
        let k = c(); let x = k.safe_door(500);
        let impact = x * U256::from(BPS) / (k.quote_reserve + x);
        assert!(impact <= U256::from(500u64));
    }
    #[test]
    fn opening_tax_eats_the_buy() { let k = c(); assert_eq!(k.quote_buy(eth(1), 9_900).tokens_out, U256::ZERO); }
}
