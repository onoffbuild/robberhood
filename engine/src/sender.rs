//! The sender: warm TLS connections to the send endpoint, raw bytes out, no simulation.
//! eth_sendRawTransaction on Nitro returns once the sequencer has placed the tx in a block,
//! so the response time is the true feed-to-inclusion number.

use alloy::primitives::{B256, Bytes};
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub struct Sender { url: String, clients: Vec<reqwest::Client>, next: AtomicU64 }

impl Sender {
    pub fn new(url: &str, lanes: usize) -> Result<Self> {
        let clients = (0..lanes.max(1)).map(|_| reqwest::Client::builder()
            .pool_idle_timeout(None).tcp_nodelay(true).tcp_keepalive(Duration::from_secs(15))
            .timeout(Duration::from_secs(5)).build()).collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Self { url: url.to_string(), clients, next: AtomicU64::new(0) })
    }
    fn lane(&self, i: Option<usize>) -> &reqwest::Client {
        let i = i.unwrap_or_else(|| self.next.fetch_add(1, Ordering::Relaxed) as usize);
        &self.clients[i % self.clients.len()]
    }
    /// Open every lane so the first real send does not pay a TLS handshake.
    pub async fn warm(&self) -> Result<()> {
        for i in 0..self.clients.len() { self.call(Some(i), "eth_chainId", json!([])).await?; }
        Ok(())
    }
    pub async fn call(&self, lane: Option<usize>, method: &str, params: Value) -> Result<Value> {
        let r: Value = self.lane(lane).post(&self.url)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send().await?.json().await?;
        if let Some(e) = r.get("error") { return Err(anyhow!("rpc {method}: {e}")); }
        Ok(r["result"].clone())
    }
    /// Lane 0 is reserved for emergency exits and is never used round-robin.
    pub async fn send_raw(&self, raw: &Bytes, emergency: bool) -> Result<(B256, Duration)> {
        let t0 = Instant::now();
        let lane = if emergency { Some(0) } else { Some(1 + (self.next.fetch_add(1, Ordering::Relaxed) as usize % (self.clients.len().max(2) - 1))) };
        let r = self.call(lane, "eth_sendRawTransaction", json!([format!("0x{}", hex::encode(raw))])).await?;
        let h: B256 = r.as_str().ok_or_else(|| anyhow!("bad hash"))?.parse()?;
        Ok((h, t0.elapsed()))
    }
    pub async fn nonce(&self, who: alloy::primitives::Address) -> Result<u64> {
        let r = self.call(None, "eth_getTransactionCount", json!([format!("{who:?}"), "pending"])).await?;
        Ok(u64::from_str_radix(r.as_str().unwrap_or("0x0").trim_start_matches("0x"), 16)?)
    }
    pub async fn gas_price(&self) -> Result<u128> {
        let r = self.call(None, "eth_gasPrice", json!([])).await?;
        Ok(u128::from_str_radix(r.as_str().unwrap_or("0x0").trim_start_matches("0x"), 16)?)
    }
}
