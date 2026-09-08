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

`FEED_URL` and `RPC_URL` override the public endpoints. Point them at your own relay and node.

## Not here yet

- the state task (per-position marks from `CurveTouch` events, trail and take-profit rules)
- the socket protocol to the Node desk
- the graduation task (fire the pool swap on `PoolGraduated`)
- confirmation and nonce resync after a fire
- a recorded-feed fixture so the matcher can be tested offline

## Tests

```
cargo test
```
