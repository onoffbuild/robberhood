//! The wire between the Node desk and the engine: a unix socket, one JSON object per line.
//! The desk arms the engine, sets the watch list and the rules, and opens positions it bought
//! (or the engine bought for it). The engine reports launches, marks, decisions and fires.
//! Numbers that are wei are hex strings so nothing passes through a JS float.

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{broadcast, mpsc};

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Inbound {
    /// replace the watch sets
    Watch { #[serde(default)] curves: Vec<Address>, #[serde(default)] tokens: Vec<Address>, #[serde(default)] wallets: Vec<Address> },
    /// exit rules, all in bps and ms; missing fields keep the defaults
    Rules(RulesMsg),
    /// a position to manage: cost and tokens in wei, current reserves, the wallets whose sells are a siren
    Open { curve: Address, token: Address, cost: String, tokens: String, quote_reserve: String, token_reserve: String, fee_bps: u64, creator_tax_bps: u64, #[serde(default)] sellable: Option<String>, #[serde(default)] wallets: Vec<Address> },
    /// correct the local reserve estimate from a node read
    Resync { curve: Address, quote_reserve: String, token_reserve: String },
    /// the curve was swept (halted) or the pool opened (graduated)
    Phase { curve: Address, #[serde(default)] halted: bool, #[serde(default)] graduated: bool },
    /// forget a position (the desk sold it elsewhere)
    Close { curve: Address },
    /// sell now, whatever the rules say
    Sell { curve: Address, #[serde(default = "all")] bps: u64 },
    /// gas parameters for the bank
    Gas { max_fee: String, max_priority: String, limit: u64 },
    /// buy this launch (by its launch tx hash) at the tax crossing. Must arrive before the crossing.
    Enter { hash: alloy::primitives::B256, eth: String, #[serde(default = "ceiling")] ceiling_bps: u64, #[serde(default = "slip")] slippage_bps: u64 },
    /// engine-side entry without the desk: every launch that passes these filters is bought
    Auto { eth: String, #[serde(default = "ceiling")] ceiling_bps: u64, #[serde(default = "slip")] slippage_bps: u64, #[serde(default = "u64max")] max_creator_tax_bps: u64, #[serde(default = "u64max")] max_exemptions: u64, #[serde(default = "one")] max_open: u64, #[serde(default)] off: bool },
    /// the one-way wire estimate to the sequencer, ms, subtracted from every fire time
    Wire { ms: u64 },
    Ping,
}
fn ceiling() -> u64 { 300 }
fn slip() -> u64 { 300 }
fn u64max() -> u64 { u64::MAX }
fn one() -> u64 { 1 }
fn all() -> u64 { 10_000 }

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RulesMsg {
    pub take_profit_bps: Option<u64>, pub stop_loss_bps: Option<u64>, pub tiers: Option<Vec<(u64, u64)>>, pub trail: Option<Vec<(u64, u64)>>,
    pub trail_arm_bps: Option<u64>, pub dead: Option<Option<(u64, u64)>>, pub max_hold_ms: Option<u64>, pub grad_exit_fill_bps: Option<Option<u64>>,
    pub emergency_slippage_bps: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum Outbound {
    Hello { engine: String, address: Option<Address>, paper: bool },
    Pong,
    Launch { seq: u64, hash: alloy::primitives::B256, from: Address, name: String, symbol: String, quote_in: String, creator_tax_bps: u16, exemptions: usize, recipient: Address },
    /// the launch's curve is known and read; the desk has until the crossing to say `enter`
    LaunchReady { hash: alloy::primitives::B256, token: Address, curve: Address, deployer: Address, native: bool, quote_reserve: String, token_reserve: String, fee_bps: u64, creator_tax_bps: u64, crossing_in_ms: i64 },
    /// auto mode looked at a launch and did not enter
    Passed { hash: alloy::primitives::B256, why: String },
    /// a buy is signed and waiting for its fire time
    Planned { curve: Address, spend: String, min_out: String, fire_in_ms: i64, tax_bps: u64 },
    Opened { curve: Address, token: Address, cost: String, tokens: String, hash: String, ms: u128 },
    Block { seq: u64, ts: u64, txs: usize },
    Mark { curve: Address, value: String, gain_bps: i64, peak_bps: i64, halted: bool },
    Siren { curve: Address, kind: String, from: Address, detail: String },
    Decision { curve: Address, bps: u64, why: String, trapped: bool },
    Fired { curve: Address, label: String, hash: String, ms: u128, emergency: bool },
    Error { what: String },
}

pub fn wei(s: &str) -> anyhow::Result<U256> { Ok(s.parse::<U256>()?) }
pub fn hex(u: U256) -> String { format!("0x{u:x}") }

/// Serve the socket. Inbound lines go to `to_engine`; anything on `from_engine` is written to every client.
pub async fn serve(path: &Path, to_engine: mpsc::Sender<Inbound>, from_engine: broadcast::Sender<Outbound>, hello: Outbound) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    tracing::info!("desk socket at {}", path.display());
    loop {
        let (stream, _) = listener.accept().await?;
        let (rd, mut wr) = stream.into_split();
        let to = to_engine.clone(); let mut rx = from_engine.subscribe(); let hello = hello.clone();
        tokio::spawn(async move {
            let _ = wr.write_all((serde_json::to_string(&hello).unwrap() + "\n").as_bytes()).await;
            loop {
                match rx.recv().await {
                    Ok(ev) => { if wr.write_all((serde_json::to_string(&ev).unwrap() + "\n").as_bytes()).await.is_err() { break; } }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
        tokio::spawn(async move {
            let mut lines = BufReader::new(rd).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<Inbound>(&line) {
                    Ok(m) => { if to.send(m).await.is_err() { break; } }
                    Err(e) => tracing::warn!("desk sent something the engine does not read: {e}: {line}"),
                }
            }
        });
    }
}
