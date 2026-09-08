#![allow(dead_code)] // the state task and the socket protocol are next; they consume what is unused today
//! loxley-engine: the hot path. Feed in, pre-signed transactions out. The Node desk does the rest.
//!
//!   loxley-engine probe            watch the feed, print launches and per-block latency
//!   loxley-engine warm             open the send lanes and report round trip to the sequencer
//!   loxley-engine clock --ceiling 300   print the tax crossing time for a ceiling

mod abi;
mod clock;
mod curve;
mod entry;
mod exit;
mod feed;
mod ipc;
mod sender;
mod signer;
mod state;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{RwLock, mpsc};

#[derive(Parser)]
#[command(name = "loxley-engine", version)]
struct Cli {
    #[arg(long, env = "FEED_URL", default_value = "wss://feed.mainnet.chain.robinhood.com")]
    feed: String,
    #[arg(long, env = "RPC_URL", default_value = "https://rpc.mainnet.chain.robinhood.com")]
    rpc: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// read the feed, print every launchAndBuy and the feed-to-local latency per block
    Probe {
        #[arg(long, default_value_t = 2)] connections: u8,
        /// stop after this many blocks (0 = forever)
        #[arg(long, default_value_t = 0)] blocks: u64,
    },
    /// open the send lanes and time a round trip on each
    Warm { #[arg(long, default_value_t = 4)] lanes: usize },
    /// print when the opening tax crosses a ceiling
    Clock { #[arg(long, default_value_t = 300)] ceiling: u64 },
    /// the engine proper: feed in, desk socket open, pre-signed exits armed. Paper mode without PRIVATE_KEY.
    Run {
        #[arg(long, env = "ENGINE_SOCKET", default_value = "/tmp/loxley-engine.sock")] socket: std::path::PathBuf,
        #[arg(long, default_value_t = 2)] connections: u8,
        #[arg(long, default_value_t = 4)] lanes: usize,
        #[arg(long, env = "PRIVATE_KEY", hide_env_values = true)] private_key: Option<String>,
        #[arg(long, default_value_t = 400_000)] gas_limit: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?)).init();
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Clock { ceiling } => {
            let c = clock::TaxCurve::default();
            println!("tax {} bps at t=0, 0 at {} s. crosses {} bps at +{} ms", c.start_bps, c.seconds, ceiling, c.crossing_ms(ceiling));
        }
        Cmd::Warm { lanes } => {
            let s = sender::Sender::new(&cli.rpc, lanes)?;
            let t = Instant::now(); s.warm().await?; println!("warm: {} lanes open in {:?}", lanes, t.elapsed());
            for i in 0..lanes {
                let t = Instant::now(); s.call(Some(i), "eth_blockNumber", serde_json::json!([])).await?;
                println!("lane {i}: round trip {:?}", t.elapsed());
            }
        }
        Cmd::Run { socket, connections, lanes, private_key, gas_limit } => {
            let sender = Arc::new(sender::Sender::new(&cli.rpc, lanes)?);
            if let Err(e) = sender.warm().await { tracing::warn!("rpc lanes not warm yet: {e}"); }
            let bank = match private_key {
                Some(k) => {
                    let signer: alloy::signers::local::PrivateKeySigner = k.trim().parse()?;
                    let nonce = sender.nonce(signer.address()).await?;
                    let gp = sender.gas_price().await?;
                    tracing::info!("armed as {:?} nonce {} gas {} wei", signer.address(), nonce, gp);
                    Some(signer::Bank::new(signer, abi::CHAIN_ID, nonce, signer::Gas { max_fee: gp * 2, max_priority: 0, limit: gas_limit }))
                }
                None => { tracing::warn!("no PRIVATE_KEY: paper mode, decisions are reported and never sent"); None }
            };
            let address = bank.as_ref().map(|b| b.address());
            let watch = Arc::new(RwLock::new(feed::Watch::default()));
            let (ftx, frx) = mpsc::channel(4096);
            let (dtx, drx) = mpsc::channel(256);
            let (otx, _) = tokio::sync::broadcast::channel(4096);
            tokio::spawn(feed::supervise(cli.feed.clone(), connections, watch.clone(), ftx));
            let hello = ipc::Outbound::Hello { engine: env!("CARGO_PKG_VERSION").into(), address, paper: bank.is_none() };
            let o2 = otx.clone();
            tokio::spawn(async move { if let Err(e) = ipc::serve(&socket, dtx, o2, hello).await { tracing::error!("socket: {e}"); } });
            state::Engine::new(watch, bank, sender, otx).run(frx, drx).await;
        }
        Cmd::Probe { connections, blocks } => {
            let watch = Arc::new(RwLock::new(feed::Watch::default()));
            let (tx, mut rx) = mpsc::channel(4096);
            tokio::spawn(feed::supervise(cli.feed.clone(), connections, watch, tx));
            let (mut n, mut sum_ms, mut max_ms) = (0u64, 0u128, 0u128);
            while let Some(ev) = rx.recv().await {
                match ev {
                    feed::Event::Launch { seq, hash, from, call, seen, .. } => {
                        println!("LAUNCH  seq={seq} {hash:?} ${} \"{}\" by {from:?} quoteIn={} creatorTax={}bps exemptions={} (+{:?} since receive)",
                            call.symbol, call.name, call.quote_in, call.creator_tax_bps, call.exemptions.len(), seen.elapsed());
                    }
                    feed::Event::Block { seq, ts, txs, seen } => {
                        // feed timestamps are whole seconds; the useful number is receive-to-now, the decode cost
                        let d = seen.elapsed().as_micros();
                        n += 1; sum_ms += d; max_ms = max_ms.max(d);
                        if n % 50 == 0 { println!("block seq={seq} ts={ts} txs={txs} decode+dispatch avg={}us max={}us over {n} blocks", sum_ms / n as u128, max_ms); }
                        if blocks > 0 && n >= blocks { break; }
                    }
                    other => println!("{other:?}"),
                }
            }
        }
    }
    Ok(())
}
