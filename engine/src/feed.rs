//! The sequencer feed: one message per block, already ordered, ahead of every RPC node.
//! We cannot land in front of anything we see here. We can be first in the next block.

use crate::abi::{self, LaunchCall, SellCall};
use alloy::consensus::Transaction;
use alloy::primitives::{Address, B256, U256};
use anyhow::Result;
use arb_sequencer_consensus::transactions::ArbTxEnvelope;
use futures_util::StreamExt;
use sequencer_client::SequencerReader;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{RwLock, mpsc};

/// What the engine reacts to. Everything else in the feed is dropped before decoding.
#[derive(Debug, Clone)]
pub enum Event {
    /// A launchAndBuy was sequenced. The token and curve addresses are not in the calldata;
    /// they come from the TokenLaunched log one block later, or from a CREATE2 prediction.
    Launch { seq: u64, ts: u64, hash: B256, from: Address, call: LaunchCall, seen: Instant },
    /// Curve.sell from a watched wallet on a curve we hold. Fire the pre-signed exit now.
    WatchedSell { seq: u64, curve: Address, from: Address, call: SellCall, seen: Instant },
    /// ERC20 transfer out of a watched wallet: the second-wallet rug the JS side admits it cannot see.
    WatchedTransfer { seq: u64, token: Address, from: Address, to: Address, amount: U256, seen: Instant },
    /// Any tx into a curve we hold, for per-block marks without an RPC.
    CurveTouch { seq: u64, curve: Address, from: Address, selector: [u8; 4], value: U256 },
    /// A block went by. Carries feed-to-local latency for the probe.
    Block { seq: u64, ts: u64, txs: usize, seen: Instant },
}

/// The set of addresses the matcher cares about. The desk updates it over the socket.
#[derive(Default)]
pub struct Watch {
    pub curves: HashSet<Address>,
    pub tokens: HashSet<Address>,
    pub wallets: HashSet<Address>,
}
pub type SharedWatch = Arc<RwLock<Watch>>;

/// Supervise the reader: the crate panics on a failed connect, so every run is a task whose
/// panic is caught here, then reconnected after a short pause. The feed never dies quietly.
pub async fn supervise(url: String, connections: u8, watch: SharedWatch, out: mpsc::Sender<Event>) {
    let mut backoff = 500u64;
    loop {
        let (u, w, o) = (url.clone(), watch.clone(), out.clone());
        let started = Instant::now();
        let r = tokio::spawn(async move { run(&u, connections, w, o).await }).await;
        match r {
            Ok(Ok(())) => tracing::warn!("feed stream ended, reconnecting"),
            Ok(Err(e)) => tracing::warn!("feed: {e}, reconnecting"),
            Err(e) => tracing::warn!("feed task died: {}, reconnecting", e.into_panic().downcast_ref::<String>().map(String::as_str).unwrap_or("panic")),
        }
        if out.is_closed() { return; }
        backoff = if started.elapsed().as_secs() > 30 { 500 } else { (backoff * 2).min(10_000) };
        tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
    }
}

pub async fn run(url: &str, connections: u8, watch: SharedWatch, out: mpsc::Sender<Event>) -> Result<()> {
    let reader = SequencerReader::new(url, abi::CHAIN_ID, connections).await;
    let mut stream = reader.into_stream();
    while let Some(msg) = stream.next().await {
        let msg = match msg { Ok(m) => m, Err(e) => { tracing::warn!("feed: {e}"); continue; } };
        let seen = msg.received_at;
        let seq = msg.sequence_number;
        let w = watch.read().await;
        for tx in &msg.txs {
            let (to, input, value, from) = match tx {
                ArbTxEnvelope::Eip1559(t) => (t.tx().to(), t.tx().input(), t.tx().value(), t.recover_signer().ok()),
                ArbTxEnvelope::Legacy(t) => (t.tx().to(), t.tx().input(), t.tx().value(), t.recover_signer().ok()),
                ArbTxEnvelope::Eip2930(t) => (t.tx().to(), t.tx().input(), t.tx().value(), t.recover_signer().ok()),
                _ => continue,
            };
            let Some(to) = to else { continue };
            if input.len() < 4 { continue; }
            let sel: [u8; 4] = input[..4].try_into().unwrap();
            let from = from.unwrap_or(Address::ZERO);
            if to == abi::LAUNCH_ROUTER && sel == abi::SEL_LAUNCH_AND_BUY {
                if let Some(call) = abi::decode_launch(input) {
                    let _ = out.send(Event::Launch { seq, ts: msg.timestamp, hash: tx.hash(), from, call, seen }).await;
                }
                continue;
            }
            if w.curves.contains(&to) {
                let _ = out.send(Event::CurveTouch { seq, curve: to, from, selector: sel, value }).await;
                if sel == abi::SEL_SELL && w.wallets.contains(&from) {
                    if let Some(call) = abi::decode_sell(input) {
                        let _ = out.send(Event::WatchedSell { seq, curve: to, from, call, seen }).await;
                    }
                }
                continue;
            }
            if w.tokens.contains(&to) && sel == abi::SEL_TRANSFER && w.wallets.contains(&from) {
                if let Ok(c) = <abi::transferCall as alloy::sol_types::SolCall>::abi_decode(input) {
                    let _ = out.send(Event::WatchedTransfer { seq, token: to, from, to: c.to, amount: c.amount, seen }).await;
                }
            }
        }
        drop(w);
        let _ = out.send(Event::Block { seq, ts: msg.timestamp, txs: msg.txs.len(), seen }).await;
    }
    Ok(())
}
