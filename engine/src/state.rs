//! The state task: the one place where feed events, desk commands, the curve estimate, the exit
//! engine and the signing bank meet. One loop, no RPC inside it, every fire is a pre-signed write.

use crate::abi;
use crate::entry::{self, Plan, Ready};
use crate::clock::TaxCurve;
use alloy::primitives::B256;
use std::time::Duration;
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

/// engine-side entry filters, set by the desk with `auto`
#[derive(Debug, Clone)]
struct Auto { eth: U256, ceiling_bps: u64, slippage_bps: u64, max_creator_tax_bps: u64, max_exemptions: u64, max_open: u64 }

/// messages the engine sends itself from spawned tasks
pub enum Internal {
    Ready(Result<Ready, String>, B256),
    Fire(Address),
    Bought { curve: Address, tokens: U256, spend: U256, ok: bool },
}

struct Pending { ready: Ready, ready_recipient: Address, creator_tax_bps: u16, exemptions: usize, plan: Option<Plan> }

pub struct Engine {
    pub rules: Rules,
    pub emergency_slippage_bps: u64,
    held: HashMap<Address, Held>,
    watch: SharedWatch,
    bank: Option<Bank>,
    sender: Arc<Sender>,
    out: broadcast::Sender<Outbound>,
    launches: HashMap<B256, Pending>,
    by_curve: HashMap<Address, B256>,
    auto: Option<Auto>,
    wire: Duration,
    tax: TaxCurve,
    me: mpsc::Sender<Internal>,
    me_rx: Option<mpsc::Receiver<Internal>>,
}

impl Engine {
    pub fn new(watch: SharedWatch, bank: Option<Bank>, sender: Arc<Sender>, out: broadcast::Sender<Outbound>) -> Self {
        let (me, rx) = mpsc::channel(1024);
        Self { rules: Rules::default(), emergency_slippage_bps: 4_000, held: HashMap::new(), watch, bank, sender, out, launches: HashMap::new(), by_curve: HashMap::new(), auto: None, wire: Duration::from_millis(2), tax: TaxCurve::default(), me, me_rx: Some(rx) }
    }
    pub fn paper(&self) -> bool { self.bank.is_none() }
    fn say(&self, o: Outbound) { let _ = self.out.send(o); }

    pub async fn run(mut self, mut feed: mpsc::Receiver<Event>, mut desk: mpsc::Receiver<Inbound>) {
        let mut me = self.me_rx.take().unwrap();
        loop {
            tokio::select! {
                biased;
                Some(i) = me.recv() => self.on_internal(i).await,
                Some(ev) = feed.recv() => self.on_feed(ev).await,
                Some(m) = desk.recv() => self.on_desk(m).await,
                else => break,
            }
        }
    }

    async fn on_internal(&mut self, i: Internal) {
        match i {
            Internal::Ready(Err(e), hash) => { self.launches.remove(&hash); self.say(Outbound::Error { what: format!("launch {hash:?}: {e}") }); }
            Internal::Ready(Ok(r), hash) => {
                if !self.launches.contains_key(&hash) { return; }
                let crossing = self.tax.crossing_ms(300) as i64 - r.seen.elapsed().as_millis() as i64;
                self.by_curve.insert(r.curve, hash);
                self.say(Outbound::LaunchReady { hash, token: r.token, curve: r.curve, deployer: r.deployer, native: r.native, quote_reserve: hex(r.state.quote_reserve), token_reserve: hex(r.state.token_reserve), fee_bps: r.state.fee_bps, creator_tax_bps: r.state.creator_tax_bps, crossing_in_ms: crossing });
                self.launches.get_mut(&hash).unwrap().ready = r;
                if let Some(a) = self.auto.clone() {
                    let p = &self.launches[&hash];
                    let open = self.held.len() as u64 + self.launches.values().filter(|x| x.plan.is_some()).count() as u64;
                    if p.creator_tax_bps as u64 <= a.max_creator_tax_bps && p.exemptions as u64 <= a.max_exemptions && open < a.max_open && p.ready.native {
                        self.enter(hash, a.eth, a.ceiling_bps, a.slippage_bps).await;
                    }
                }
            }
            Internal::Fire(curve) => self.fire_buy(curve).await,
            Internal::Bought { curve, tokens, spend, ok } => {
                if !ok { self.held.remove(&curve); self.say(Outbound::Error { what: format!("buy on {curve:?} reverted") }); return; }
                if let Some(h) = self.held.get_mut(&curve) { h.pos.tokens = tokens; h.pos.cost = spend; }
            }
        }
    }

    async fn enter(&mut self, hash: B256, eth: U256, ceiling_bps: u64, slippage_bps: u64) {
        let Some(p) = self.launches.get(&hash) else { self.say(Outbound::Error { what: format!("enter: {hash:?} is not a launch I have seen") }); return };
        if p.ready.curve == Address::ZERO { self.say(Outbound::Error { what: format!("enter: {hash:?} is not read yet") }); return; }
        if p.plan.is_some() { return; }
        match entry::plan(&p.ready, eth, ceiling_bps, slippage_bps, self.wire, &self.tax) {
            Err(e) => self.say(Outbound::Error { what: format!("enter: {e}") }),
            Ok(plan) => {
                let curve = plan.curve; let fire_at = plan.fire_at;
                let fire_in = fire_at.saturating_duration_since(Instant::now());
                self.say(Outbound::Planned { curve, spend: hex(plan.spend), min_out: hex(plan.min_out), fire_in_ms: fire_in.as_millis() as i64, tax_bps: plan.tax_bps });
                // sign now, under the current nonce, so the fire is a write
                let mut err = None;
                if let Some(b) = self.bank.as_mut() {
                    let input = abi::encode_buy(plan.spend, plan.min_out, b.address());
                    if let Err(e) = b.arm(&format!("buy:{curve:?}"), curve, plan.spend, input) { err = Some(format!("sign buy: {e}")); }
                }
                if let Some(e) = err { self.say(Outbound::Error { what: e }); }
                self.launches.get_mut(&hash).unwrap().plan = Some(plan);
                let me = self.me.clone();
                tokio::spawn(async move { tokio::time::sleep_until(tokio::time::Instant::from_std(fire_at)).await; let _ = me.send(Internal::Fire(curve)).await; });
            }
        }
    }

    async fn fire_buy(&mut self, curve: Address) {
        let Some(hash) = self.by_curve.get(&curve).copied() else { return };
        let Some(p) = self.launches.remove(&hash) else { return };
        let Some(plan) = p.plan else { return };
        let label = format!("buy:{curve:?}");
        let t0 = Instant::now();
        let (hash_str, took, ok) = match self.bank.as_mut() {
            None => ("paper".to_string(), 0u128, true),
            Some(bank) => {
                let armed = match bank.fire(&label) {
                    Some(a) => a,
                    None => {
                        // a sell went out first and took the nonce: sign again, tens of microseconds
                        let input = abi::encode_buy(plan.spend, plan.min_out, bank.address());
                        match bank.arm(&label, curve, plan.spend, input) { Ok(_) => bank.fire(&label).unwrap(), Err(e) => { self.say(Outbound::Error { what: format!("sign buy: {e}") }); return; } }
                    }
                };
                match self.sender.send_raw(&armed.raw, false).await {
                    Ok((h, d)) => (format!("{h:?}"), d.as_millis(), true),
                    Err(e) => { self.say(Outbound::Error { what: format!("buy {curve:?}: {e}") }); if let Ok(n) = self.sender.nonce(self.bank.as_ref().unwrap().address()).await { self.bank.as_mut().unwrap().nonce = n; } return; }
                }
            }
        };
        let _ = (t0, ok);
        let r = p.ready;
        let mut wallets = vec![r.deployer];
        if p.ready_recipient != r.deployer { wallets.push(p.ready_recipient); }
        let mut c = r.state; c.apply_buy(plan.spend, plan.tax_bps);
        { let mut w = self.watch.write().await; w.curves.insert(curve); w.tokens.insert(r.token); for a in &wallets { w.wallets.insert(*a); } }
        self.held.insert(curve, Held { token: r.token, curve: c, pos: Position::open(plan.spend, plan.tokens_est, crate::clock::now_ms()), wallets, siren: None, pending: None });
        self.say(Outbound::Opened { curve, token: r.token, cost: hex(plan.spend), tokens: hex(plan.tokens_est), hash: hash_str.clone(), ms: took });
        self.rearm().await;
        // confirm in the background: the real tokensOut is in the CurveBuy log
        if let Ok(h) = hash_str.parse::<B256>() {
            let (s, me, cost) = (self.sender.clone(), self.me.clone(), plan.spend);
            tokio::spawn(async move {
                for _ in 0..400 {
                    if let Ok(Some(rc)) = s.receipt(h).await {
                        let ok = rc["status"].as_str() == Some("0x1");
                        let tokens = rc["logs"].as_array().into_iter().flatten()
                            .find(|l| l["topics"][0].as_str().and_then(|t| t.parse::<B256>().ok()) == Some(abi::T_BUY))
                            .and_then(|l| { let d = hex::decode(l["data"].as_str()?.trim_start_matches("0x")).ok()?; if d.len() >= 64 { Some(U256::from_be_slice(&d[32..64])) } else { None } });
                        let _ = me.send(Internal::Bought { curve, tokens: tokens.unwrap_or(U256::ZERO), spend: cost, ok: ok && tokens.is_some() }).await;
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            });
        }
    }

    async fn on_feed(&mut self, ev: Event) {
        match ev {
            Event::Launch { seq, hash, from, call, seen, .. } => {
                self.say(Outbound::Launch { seq, hash, from, name: call.name.clone(), symbol: call.symbol.clone(), quote_in: hex(call.quote_in), creator_tax_bps: call.creator_tax_bps, exemptions: call.exemptions.len(), recipient: call.recipient });
                let blank = Ready { hash, token: Address::ZERO, curve: Address::ZERO, deployer: from, pair: Address::ZERO, native: false, state: Curve::default(), seen };
                self.launches.insert(hash, Pending { ready: blank, ready_recipient: call.recipient, creator_tax_bps: call.creator_tax_bps, exemptions: call.exemptions.len(), plan: None });
                let (s, me) = (self.sender.clone(), self.me.clone());
                tokio::spawn(async move { let r = entry::enrich(s, hash, seen, Duration::from_millis(2_500)).await.map_err(|e| e.to_string()); let _ = me.send(Internal::Ready(r, hash)).await; });
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
                let stale: Vec<B256> = self.launches.iter().filter(|(_, p)| p.plan.is_none() && p.ready.seen.elapsed() > Duration::from_secs(10)).map(|(h, _)| *h).collect();
                for h in stale { if let Some(p) = self.launches.remove(&h) { self.by_curve.remove(&p.ready.curve); } }
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
            Inbound::Wire { ms } => { self.wire = Duration::from_millis(ms); }
            Inbound::Enter { hash, eth, ceiling_bps, slippage_bps } => match wei(&eth) { Ok(e) => self.enter(hash, e, ceiling_bps, slippage_bps).await, Err(_) => self.say(Outbound::Error { what: "enter: bad eth".into() }) },
            Inbound::Auto { eth, ceiling_bps, slippage_bps, max_creator_tax_bps, max_exemptions, max_open, off } => {
                if off { self.auto = None; } else { match wei(&eth) { Ok(e) => self.auto = Some(Auto { eth: e, ceiling_bps, slippage_bps, max_creator_tax_bps, max_exemptions, max_open }), Err(_) => self.say(Outbound::Error { what: "auto: bad eth".into() }) } }
            }
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
