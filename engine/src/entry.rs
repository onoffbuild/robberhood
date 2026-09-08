//! The entry path. A launch is seen in the feed; the curve address is in the TokenLaunched log of
//! that same tx, which the node has as soon as it executes the block. The buy is signed the moment
//! the curve state is known and written to the socket at the tax crossing, on the local clock.

use crate::abi;
use crate::clock::TaxCurve;
use crate::curve::Curve;
use crate::sender::Sender;
use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::SolCall;
use anyhow::{Result, anyhow};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Everything the desk's scorer and the engine's buy need, gathered in one pass off the node.
#[derive(Debug, Clone)]
pub struct Ready {
    pub hash: B256,
    pub token: Address,
    pub curve: Address,
    pub deployer: Address,
    pub pair: Address,
    pub native: bool,
    pub state: Curve,
    pub seen: Instant,
}

/// Poll the receipt (present within a block on a local node), then read the curve in parallel.
pub async fn enrich(s: Arc<Sender>, hash: B256, seen: Instant, deadline: Duration) -> Result<Ready> {
    let t0 = Instant::now();
    let rc = loop {
        if let Some(r) = s.receipt(hash).await? { break r; }
        if t0.elapsed() > deadline { return Err(anyhow!("receipt of {hash:?} not seen within {deadline:?}")); }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let (token, curve, deployer, pair) = rc["logs"].as_array().into_iter().flatten()
        .find(|l| l["topics"][0].as_str().map(|t| t.parse::<B256>().ok() == Some(abi::T_LAUNCH)).unwrap_or(false))
        .and_then(|l| {
            let t = |i: usize| l["topics"][i].as_str()?.parse::<B256>().ok().map(|h| Address::from_word(h));
            let d = l["data"].as_str()?; let d = hex::decode(d.trim_start_matches("0x")).ok()?;
            let pair = if d.len() >= 32 { Address::from_slice(&d[12..32]) } else { Address::ZERO };
            Some((t(1)?, t(2)?, t(3)?, pair))
        }).ok_or_else(|| anyhow!("no TokenLaunched log in {hash:?}"))?;
    let (d_res, d_fee, d_tax, d_sell, d_nat) = (abi::getReservesCall {}.abi_encode(), abi::feeBpsCall {}.abi_encode(), abi::creatorTaxBpsCall {}.abi_encode(), abi::sellableTokensCall {}.abi_encode(), abi::isNativeQuoteCall {}.abi_encode());
    let (res, fee, tax, sell, native) = tokio::join!(
        s.eth_call(curve, &d_res), s.eth_call(curve, &d_fee), s.eth_call(curve, &d_tax), s.eth_call(curve, &d_sell), s.eth_call(curve, &d_nat),
    );
    let r = abi::getReservesCall::abi_decode_returns(&res?)?;
    let state = Curve {
        quote_reserve: r.quoteReserve, token_reserve: r.tokenReserve,
        fee_bps: abi::feeBpsCall::abi_decode_returns(&fee?)?.to::<u64>(),
        creator_tax_bps: abi::creatorTaxBpsCall::abi_decode_returns(&tax?)?.to::<u64>(),
        sellable: sell.ok().and_then(|b| abi::sellableTokensCall::abi_decode_returns(&b).ok()),
        halted: false, graduated: false,
    };
    let native = native.ok().and_then(|b| abi::isNativeQuoteCall::abi_decode_returns(&b).ok()).unwrap_or(pair == Address::ZERO);
    Ok(Ready { hash, token, curve, deployer, pair, native, state, seen })
}

/// The buy, sized and timed. `wire` is the one-way estimate to the sequencer.
#[derive(Debug, Clone)]
pub struct Plan { pub curve: Address, pub spend: U256, pub min_out: U256, pub tokens_est: U256, pub fire_at: Instant, pub tax_bps: u64 }

pub fn plan(r: &Ready, eth: U256, ceiling_bps: u64, slippage_bps: u64, wire: Duration, tax: &TaxCurve) -> Result<Plan> {
    if !r.native { return Err(anyhow!("paired with {:?}, not ETH", r.pair)); }
    let fire_at = tax.fire_at(r.seen, ceiling_bps, wire);
    let tax_bps = tax.at_ms(tax.crossing_ms(ceiling_bps));
    let q = r.state.quote_buy(eth, tax_bps);
    if q.tokens_out.is_zero() { return Err(anyhow!("that amount buys nothing after fees and tax")); }
    Ok(Plan { curve: r.curve, spend: q.spent, min_out: crate::curve::min_out(q.tokens_out, slippage_bps), tokens_est: q.tokens_out, fire_at, tax_bps })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn plan_fires_at_the_crossing_minus_wire() {
        let seen = Instant::now();
        let r = Ready { hash: B256::ZERO, token: Address::ZERO, curve: Address::ZERO, deployer: Address::ZERO, pair: Address::ZERO, native: true,
            state: Curve { quote_reserve: U256::from(10u64) * U256::from(10u64).pow(U256::from(18u64)), token_reserve: U256::from(10u64).pow(U256::from(27u64)), fee_bps: 100, creator_tax_bps: 200, sellable: None, halted: false, graduated: false }, seen };
        let p = plan(&r, U256::from(10u64).pow(U256::from(17u64)), 300, 300, Duration::from_millis(2), &TaxCurve::default()).unwrap();
        assert_eq!(p.fire_at.duration_since(seen).as_millis(), 2908);
        assert!(p.tax_bps <= 300);
        assert!(p.min_out < p.tokens_est);
    }
}
