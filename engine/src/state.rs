//! The state task: the one place where feed events, desk commands, the curve estimate, the exit
//! engine and the signing bank meet. One loop, no RPC inside it, every fire is a pre-signed write.

use crate::abi;
use crate::curve::{self, Curve};
use crate::exit::{self, Decision, Mark, Position, Rules, Siren, Why};
use crate::feed::{Event, SharedWatch};
use crate::ipc::{Inbound, Outbound, hex, wei};
use crate::sender::Sender;
use crate::signer::{Bank, Gas};
use alloy::primitives::{Address, U256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{broadcast, mpsc};

struct Held { token: Address, curve: Curve, pos: Position, wallets: Vec<Address>, siren: Option<(Siren, Address, String)>, pending: Option<Why> }

pub struct Engine {
    pub rules: Rules,
    pub emergency_slippage_bps: u64,
    held: HashMap<Address, Held>,
    watch: SharedWatch,
    bank: Option<Bank>,
    sender: Arc<Sender>,
    out: broadcast::Sender<Outbound>,
    /// the launch instant per launch tx sender, for the tax clock
    launches: HashMap<Address, Instant>,
}

impl Engine {
    pub fn new(watch: SharedWatch, bank: Option<Bank>, sender: Arc<Sender>, out: broadcast::Sender<Outbound>) -> Self {
        Self { rules: Rules::default(), emergency_slippage_bps: 4_000, held: HashMap::new(), watch, bank, sender, out, launches: HashMap::new() }
    }
    pub fn paper(&self) -> bool { self.bank.is_none() }
    fn say(&self, o: Outbound) { let _ = self.out.send(o); }

    pub async fn run(mut self, mut feed: mpsc::Receiver<Event>, mut desk: mpsc::Receiver<Inbound>) {
        loop {
            tokio::select! {
                biased;
                Some(ev) = feed.recv() => self.on_feed(ev).await,
                Some(m) = desk.recv() => self.on_desk(m).await,
                else => break,
            }
        }
    }

    async fn on_feed(&mut self, ev: Event) {
        match ev {
            Event::Launch { seq, from, call, seen, .. } => {
                self.launches.insert(from, seen);
                self.say(Outbound::Launch { seq, from, name: call.name, symbol: call.symbol, quote_in: hex(call.quote_in), creator_tax_bps: call.creator_tax_bps, exemptions: call.exemptions.len(), recipient: call.recipient });
            }
            Event::WatchedSell { curve, from, call, .. } => {
                if let Some(h) = self.held.get_mut(&curve) {
                    h.siren = Some((Siren::WatchedSold, from, format!("sell {} tokens", call.tokens_in)));
                    self.say(Outbound::Siren { curve, kind: "watched_sold".into(), from, detail: hex(call.tokens_in) });
                    // do not wait for the block event: this is the whole point of the engine
                    self.evaluate(curve, Instant::now()).await;
                }
            }
            Event::WatchedTransfer { token, from, to, amount, .. } => {
                let hit: Vec<Address> = self.held.iter().filter(|(_, h)| h.token == token).map(|(c, _)| *c).collect();
                for curve in hit {
                    if let Some(h) = self.held.get_mut(&curve) { h.siren = Some((Siren::WatchedTransfer, from, format!("{amount} to {to:?}"))); }
                    self.say(Outbound::Siren { curve, kind: "watched_transfer".into(), from, detail: format!("{} to {to:?}", hex(amount)) });
                    self.evaluate(curve, Instant::now()).await;
                }
            }
            Event::CurveTouch { curve, selector, value, .. } => {
                if let Some(h) = self.held.get_mut(&curve) {
                    if selector == abi::SEL_BUY { h.curve.apply_buy(value, 0); }
                    // sells: the calldata carries tokensIn, but CurveTouch only carries value; the
                    // WatchedSell path decodes it for watched wallets, everyone else is caught by resync
                }
            }
            Event::Block { seq, ts, txs, .. } => {
                self.say(Outbound::Block { seq, ts, txs });
                let curves: Vec<Address> = self.held.keys().copied().collect();
                for c in curves { self.evaluate(c, Instant::now()).await; }
                self.rearm().await;
            }
        }
    }

    async fn on_desk(&mut self, m: Inbound) {
        match m {
            Inbound::Ping => self.say(Outbound::Pong),
            Inbound::Watch { curves, tokens, wallets } => {
                let mut w = self.watch.write().await;
                w.curves = curves.into_iter().collect(); w.tokens = tokens.into_iter().collect(); w.wallets = wallets.into_iter().collect();
            }
            Inbound::Rules(r) => {
                let x = &mut self.rules;
                if let Some(v) = r.take_profit_bps { x.take_profit_bps = v; } if let Some(v) = r.stop_loss_bps { x.stop_loss_bps = v; }
                if let Some(v) = r.tiers { x.tiers = v; } if let Some(v) = r.trail { x.trail = v; } if let Some(v) = r.trail_arm_bps { x.trail_arm_bps = v; }
                if let Some(v) = r.dead { x.dead = v; } if let Some(v) = r.max_hold_ms { x.max_hold_ms = v; } if let Some(v) = r.grad_exit_fill_bps { x.grad_exit_fill_bps = v; }
                if let Some(v) = r.emergency_slippage_bps { self.emergency_slippage_bps = v; }
            }
            Inbound::Gas { max_fee, max_priority, limit } => {
                if let Some(b) = self.bank.as_mut() {
                    match (wei(&max_fee), wei(&max_priority)) {
                        (Ok(f), Ok(p)) => b.gas = Gas { max_fee: f.to::<u128>(), max_priority: p.to::<u128>(), limit },
                        _ => self.say(Outbound::Error { what: "gas: bad hex".into() }),
                    }
                }
            }
            Inbound::Open { curve, token, cost, tokens, quote_reserve, token_reserve, fee_bps, creator_tax_bps, sellable, wallets } => {
                let parsed = (|| -> anyhow::Result<Held> {
                    let c = Curve { quote_reserve: wei(&quote_reserve)?, token_reserve: wei(&token_reserve)?, fee_bps, creator_tax_bps, sellable: sellable.as_deref().map(wei).transpose()?, halted: false, graduated: false };
                    Ok(Held { token, curve: c, pos: Position::open(wei(&cost)?, wei(&tokens)?, crate::clock::now_ms()), wallets, siren: None, pending: None })
                })();
                match parsed {
                    Ok(h) => {
                        { let mut w = self.watch.write().await; w.curves.insert(curve); w.tokens.insert(token); for a in &h.wallets { w.wallets.insert(*a); } }
                        self.held.insert(curve, h);
                        self.rearm().await;
                    }
                    Err(e) => self.say(Outbound::Error { what: format!("open: {e}") }),
                }
            }
            Inbound::Resync { curve, quote_reserve, token_reserve } => {
                if let (Some(h), Ok(q), Ok(t)) = (self.held.get_mut(&curve), wei(&quote_reserve), wei(&token_reserve)) { h.curve.resync(q, t); }
            }
            Inbound::Phase { curve, halted, graduated } => {
                if let Some(h) = self.held.get_mut(&curve) { h.curve.halted = halted; h.curve.graduated = graduated; }
                if !halted { self.evaluate(curve, Instant::now()).await; }
            }
            Inbound::Close { curve } => { self.held.remove(&curve); let mut w = self.watch.write().await; w.curves.remove(&curve); }
            Inbound::Sell { curve, bps } => { self.fire_sell(curve, bps, "desk".into(), true).await; }
        }
    }

    async fn evaluate(&mut self, curve: Address, _now: Instant) {
        let Some(h) = self.held.get_mut(&curve) else { return };
        let q = h.curve.quote_sell(h.pos.tokens);
        let siren = h.siren.as_ref().map(|s| s.0);
        let m = Mark { value: q.quote_out, now_ms: crate::clock::now_ms(), halted: h.curve.halted, fill_bps: None, siren };
        let d = exit::decide(&self.rules, &mut h.pos, &m);
        let (gain, peak, halted) = (exit::gain_bps(&h.pos, m.value), h.pos.peak_bps, h.curve.halted);
        self.say(Outbound::Mark { curve, value: hex(m.value), gain_bps: gain, peak_bps: peak, halted });
        match d {
            Decision::Hold => {}
            Decision::Trapped { why } => { let w = format!("{why:?}"); if let Some(h) = self.held.get_mut(&curve) { h.pending = Some(why); } self.say(Outbound::Decision { curve, bps: 10_000, why: w, trapped: true }); }
            Decision::Sell { bps, why } => {
                let emergency = matches!(why, Why::Siren(_));
                let w = format!("{why:?}");
                self.say(Outbound::Decision { curve, bps, why: w.clone(), trapped: false });
                self.fire_sell(curve, bps, w, emergency).await;
            }
        }
    }

    /// Sell `bps` of the remaining tokens. A full emergency exit uses the pre-signed bytes; anything
    /// else is signed now (tens of microseconds) with a fresh quote.
    async fn fire_sell(&mut self, curve: Address, bps: u64, why: String, emergency: bool) {
        let Some(h) = self.held.get(&curve) else { return };
        let tokens = h.pos.tokens * U256::from(bps) / U256::from(10_000u64);
        if tokens.is_zero() { return; }
        let label = format!("sell:{curve:?}");
        let Some(bank) = self.bank.as_mut() else { self.say(Outbound::Fired { curve, label, hash: "paper".into(), ms: 0, emergency }); self.book(curve, tokens, bps == 10_000).await; return; };
        let armed = if emergency && bps == 10_000 { bank.fire(&label) } else { None };
        let armed = match armed {
            Some(a) => a,
            None => {
                let q = h.curve.quote_sell(tokens);
                let min = curve::min_out(q.quote_out, if emergency { self.emergency_slippage_bps } else { 300 });
                let input = abi::encode_sell(tokens, min, bank.address());
                let l = format!("{label}:{bps}");
                match bank.arm(&l, curve, U256::ZERO, input) { Ok(_) => bank.fire(&l).unwrap(), Err(e) => { self.say(Outbound::Error { what: format!("sign: {e}") }); return; } }
            }
        };
        match self.sender.send_raw(&armed.raw, emergency).await {
            Ok((hash, took)) => { self.say(Outbound::Fired { curve, label: armed.label, hash: format!("{hash:?}"), ms: took.as_millis(), emergency }); self.book(curve, tokens, bps == 10_000).await; }
            Err(e) => {
                self.say(Outbound::Error { what: format!("send {why}: {e}") });
                // the nonce may or may not have been consumed; resync it before anything else fires
                if let Ok(n) = self.sender.nonce(self.bank.as_ref().unwrap().address()).await { self.bank.as_mut().unwrap().nonce = n; }
            }
        }
    }

    async fn book(&mut self, curve: Address, tokens: U256, full: bool) {
        if full { self.held.remove(&curve); let mut w = self.watch.write().await; w.curves.remove(&curve); return; }
        if let Some(h) = self.held.get_mut(&curve) {
            let q = h.curve.apply_sell(tokens);
            exit::book_sell(&mut h.pos, tokens, q.quote_out, true);
            h.siren = None;
        }
    }

    /// Keep one pre-signed full exit per position under the current nonce, wide slippage.
    async fn rearm(&mut self) {
        let slip = self.emergency_slippage_bps;
        let want: Vec<(Address, U256, U256)> = self.held.iter().map(|(c, h)| (*c, h.pos.tokens, h.curve.quote_sell(h.pos.tokens).quote_out)).collect();
        let mut errors = Vec::new();
        if let Some(bank) = self.bank.as_mut() {
            let have: std::collections::HashSet<String> = bank.labels().into_iter().map(String::from).collect();
            for (c, tokens, out) in want {
                let label = format!("sell:{c:?}");
                if have.contains(&label) { continue; }
                let input = abi::encode_sell(tokens, curve::min_out(out, slip), bank.address());
                if let Err(e) = bank.arm(&label, c, U256::ZERO, input) { errors.push(format!("arm: {e}")); }
            }
        }
        for e in errors { self.say(Outbound::Error { what: e }); }
    }
}
