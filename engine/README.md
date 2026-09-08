# loxley-engine

The hot path, in Rust. The Node desk (`cli/`) keeps scoring, the browser desk, the keystore and
the tests. The engine owns the three things that have to be fast on a first-come-first-served,
100 ms block chain: seeing the block, having the transaction already signed, and putting it on
the wire.

## What is here

| module | job |
|---|---|
| `feed.rs` | reads the sequencer feed (`wss://feed.mainnet.chain.robinhood.com`) through the `sequencer_client` crate, matches `launchAndBuy`, curve sells and token transfers from watched wallets, emits typed events |
| `abi.rs` | Pons addresses, selectors and topics, mirrored from `cli/chain.js` and `cli/pons.js`, plus `sol!` decoders |
| `signer.rs` | the pre-signed bank: every intent (buy at ceiling X, emergency sell of position Y) is signed ahead of time under the current nonce; firing one invalidates the rest |
| `sender.rs` | warm keep-alive lanes to the RPC; lane 0 is reserved for emergency exits; raw `eth_sendRawTransaction`, no simulation |
| `clock.rs` | the opening tax is deterministic from the launch instant, so the crossing time is computed, not polled |
| `curve.rs` | Pons v2 curve maths to the wei, mirrored from `cli/trade.js`, plus a local reserve estimate advanced from feed events between node resyncs |
| `exit.rs` | the exit engine, pure and tested: siren, tiered take-profit, tightening trail, stop, dead-launch stop, max hold, graduation exit |
| `state.rs` | the one loop where feed events, desk commands, curve estimates, exit decisions and the bank meet; a siren fires the pre-signed exit without waiting for the block event |
| `ipc.rs` | the unix socket to the Node desk, one JSON object per line, wei as hex strings |

## Facts the design rests on

- The feed message is broadcast after the sequencer has built the block. Nothing seen in the feed
  can be beaten in that block. The prize is being first in the next one, which the RPC pollers
  see hundreds of milliseconds later.
- Both the RPC and the feed resolve to an Offchain Labs origin behind Cloudflare. Colocate near
  the origin, keep lanes warm, and the wire is a millisecond or two.
- The opening tax runs 9 900 bps to 0 over 3 s. Ceiling 300 bps crosses at +2 910 ms after launch.

## Commands

```
cargo build --release
./target/release/loxley-engine clock --ceiling 300
./target/release/loxley-engine warm --lanes 4          # opens lanes, prints round trip per lane
./target/release/loxley-engine probe --blocks 500      # prints launches and per-block decode cost
```

```
./target/release/loxley-engine run --socket /tmp/loxley-engine.sock   # paper mode without PRIVATE_KEY
PRIVATE_KEY=0x… ./target/release/loxley-engine run                    # armed: exits are pre-signed and sent
```

`FEED_URL` and `RPC_URL` override the public endpoints. Point them at your own relay and node.

## The desk protocol

One JSON object per line on the socket. Wei are hex strings.

Desk to engine: `{"op":"watch","curves":[],"tokens":[],"wallets":[]}`, `{"op":"rules",…bps and ms…}`,
`{"op":"open","curve":…,"token":…,"cost":"0x…","tokens":"0x…","quote_reserve":"0x…","token_reserve":"0x…","fee_bps":100,"creator_tax_bps":200,"wallets":[deployer,…]}`,
`{"op":"resync",…}`, `{"op":"phase","curve":…,"halted":true}`, `{"op":"sell","curve":…,"bps":5000}`, `{"op":"close",…}`, `{"op":"gas",…}`, `{"op":"ping"}`.

Engine to desk: `hello`, `launch`, `block`, `mark`, `siren`, `decision`, `fired`, `error`.

Default exit rules, all overridable: take profit +80 %, stop −35 %, one tier (sell half at +100 %),
trail 30 % wide, 15 % past +200 %, 10 % past +900 %, armed once the peak reaches +10 %, dead launch
if not +15 % after 2 min, max hold 45 min, leave when the curve is 90 % full, emergency exits at
40 % slippage.

## Not here yet

- the entry path: score a `launch` event, arm a buy at the tax crossing, fire on the local clock
- the Node side of the socket (`cli/` still runs its own marks; nothing reads the engine yet)
- the graduation task (fire the pool swap on `PoolGraduated`)
- receipt confirmation after a fire (the nonce is resynced only on a send error today)
- curve fill toward graduation in the mark (`fill_bps` is always None)
- a recorded-feed fixture so the matcher can be tested offline

## Tests

```
cargo test
```
