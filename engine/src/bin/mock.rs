//! A fake sequencer feed and a fake node on localhost, enough to run the engine end to end offline:
//! a launch appears in the feed, the node answers its receipt and curve reads, the engine buys at
//! the crossing, then the deployer sells in the feed and the engine must be out next block.
//!
//!   loxley-mock --feed 127.0.0.1:9101 --rpc 127.0.0.1:9102 --sell-after-ms 5000

use alloy::consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy::eips::eip2718::{Decodable2718, Encodable2718};
use alloy::network::TxSignerSync;
use alloy::primitives::{Address, B256, Bytes, TxKind, U256, address, b256};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use base64::Engine as _;
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const CHAIN_ID: u64 = 4663;
const LAUNCH_ROUTER: Address = address!("e33E9E479dF8802cb0866d5d05258bEc4cF62948");
const T_LAUNCH: B256 = b256!("8d4aad4953d0ca700d468f3753aa14432d1b35b43ec6409f051fb6aa43a89607");
const T_BUY: B256 = b256!("ec36bf571f136799e8dc0b0b8bea4b04d8bd3d43de838aab0d5fc21d4cbfc455");
const TOKEN: Address = address!("00000000000000000000000000000000000000aa");
const CURVE: Address = address!("00000000000000000000000000000000000000cc");

alloy::sol! {
    struct Socials { string twitter; string telegram; string discord; string website; string farcaster; }
    struct TokenParams { string name; string symbol; string logo; string description; Socials socials; address creatorFeeRecipient; uint16 creatorTaxBps; bool buybackEnabled; bytes32 expectedEconomics; bytes32 salt; }
    function launchAndBuy(TokenParams params, uint256 launchConfigId, address pairToken, uint256 quoteIn, uint256 minTokensOut, address recipient, address[] snipeTaxExemptions) payable returns (address token, address curve, uint256 tokensOut);
    function sell(uint256 tokensIn, uint256 minQuoteOut, address recipient) returns (uint256 quoteOut);
}

#[derive(Parser)]
struct Cli {
    #[arg(long, default_value = "127.0.0.1:9101")] feed: String,
    #[arg(long, default_value = "127.0.0.1:9102")] rpc: String,
    /// when the deployer's sell shows up in the feed, ms after the launch
    #[arg(long, default_value_t = 5000)] sell_after_ms: u64,
    #[arg(long, default_value_t = 100)] block_ms: u64,
    /// when the launch shows up in the feed, ms after the client connects
    #[arg(long, default_value_t = 300)] launch_after_ms: u64,
}

struct State { seq: u64, launch_hash: B256, sent: Vec<TxEnvelope> }

fn eth(x: f64) -> U256 { U256::from((x * 1e18) as u128) }
fn word(a: Address) -> String { format!("0x{:0>64}", hex::encode(a.as_slice())) }

fn sign(signer: &PrivateKeySigner, nonce: u64, to: Address, value: U256, input: Vec<u8>) -> TxEnvelope {
    let mut tx = TxEip1559 { chain_id: CHAIN_ID, nonce, gas_limit: 1_000_000, max_fee_per_gas: 200_000_000, max_priority_fee_per_gas: 0, to: TxKind::Call(to), value, input: input.into(), access_list: Default::default() };
    let sig = signer.sign_transaction_sync(&mut tx).unwrap();
    tx.into_signed(sig).into()
}

fn feed_msg(seq: u64, txs: &[TxEnvelope]) -> String {
    // kind 3 = L2Message; inside, kind 4 = one signed tx, kind 3 = batch of [len u64 be][kind 4 || raw]
    let mut l2 = vec![3u8];
    for t in txs { let mut raw = vec![4u8]; raw.extend(t.encoded_2718()); l2.extend((raw.len() as u64).to_be_bytes()); l2.extend(raw); }
    json!({"version": 1, "messages": [{"sequenceNumber": seq, "message": {"message": {"header": {"kind": 3, "sender": "0xa4b000000000000000000073657175656e636572", "blockNumber": 1, "timestamp": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(), "requestId": null, "baseFeeL1": null}, "l2Msg": base64::engine::general_purpose::STANDARD.encode(&l2)}, "delayedMessagesRead": 0}, "signature": null}]}).to_string()
}

async fn rpc(st: Arc<Mutex<State>>, addr: String) {
    let l = TcpListener::bind(&addr).await.unwrap();
    loop {
        let (mut s, _) = l.accept().await.unwrap();
        let st = st.clone();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            loop {
                let mut chunk = [0u8; 4096];
                let n = match s.read(&mut chunk).await { Ok(0) | Err(_) => return, Ok(n) => n };
                buf.extend_from_slice(&chunk[..n]);
                while let Some(h) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..h]).to_string();
                    let len: usize = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0))).unwrap_or(0);
                    if buf.len() < h + 4 + len { break; }
                    let body: Value = serde_json::from_slice(&buf[h + 4..h + 4 + len]).unwrap_or(Value::Null);
                    buf.drain(..h + 4 + len);
                    let res = answer(&st, &body);
                    let out = json!({"jsonrpc": "2.0", "id": body["id"], "result": res}).to_string();
                    let _ = s.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}", out.len(), out).as_bytes()).await;
                }
            }
        });
    }
}

fn answer(st: &Arc<Mutex<State>>, body: &Value) -> Value {
    let p = &body["params"];
    match body["method"].as_str().unwrap_or("") {
        "eth_chainId" => json!(format!("0x{CHAIN_ID:x}")),
        "eth_blockNumber" => json!(format!("0x{:x}", 1_000 + st.lock().unwrap().seq)),
        "eth_getTransactionCount" => json!("0x0"),
        "eth_gasPrice" => json!("0x5f5e100"),
        "eth_call" => {
            let data = p[0]["data"].as_str().unwrap_or("");
            let sel = &data[..10.min(data.len())];
            let u = |x: U256| json!(format!("0x{:0>64x}", x));
            match sel {
                "0x0902f1ac" => json!(format!("0x{:0>64x}{:0>64x}", eth(10.0), U256::from(10u64).pow(U256::from(27u64)))),
                "0x24a9d853" => u(U256::from(100u64)),
                "0xc1bb8901" => u(U256::from(200u64)),
                "0x808bcddc" => u(U256::from(5u64) * U256::from(10u64).pow(U256::from(26u64))),
                "0xdc08e094" => u(U256::from(1u64)),
                _ => json!("0x"),
            }
        }
        "eth_sendRawTransaction" => {
            let raw = hex::decode(p[0].as_str().unwrap_or("").trim_start_matches("0x")).unwrap_or_default();
            match TxEnvelope::decode_2718(&mut &raw[..]) {
                Ok(env) => {
                    let h = *env.tx_hash();
                    let input = alloy::consensus::Transaction::input(&env);
                    let kind = match &input[..4.min(input.len())] { s if s == sellCall::SELECTOR => "SELL", s if s == [0x0c, 0x8c, 0x5e, 0x00] || s == crate_buy_selector() => "BUY", _ => "?" };
                    eprintln!("MOCK sent {kind} to {:?} value {} hash {h:?}", alloy::consensus::Transaction::to(&env).unwrap_or_default(), alloy::consensus::Transaction::value(&env));
                    st.lock().unwrap().sent.push(env);
                    json!(format!("{h:?}"))
                }
                Err(e) => { eprintln!("MOCK bad raw tx: {e}"); Value::Null }
            }
        }
        "eth_getTransactionReceipt" => {
            let h: B256 = p[0].as_str().unwrap_or("").parse().unwrap_or_default();
            let st = st.lock().unwrap();
            if h == st.launch_hash {
                let data = format!("0x{:0>64}{:0>64x}{:0>64x}", "0", 1u64, eth(4.2));
                return json!({"status": "0x1", "blockNumber": "0x3e8", "transactionHash": format!("{h:?}"), "logs": [{"address": format!("{:?}", LAUNCH_ROUTER), "topics": [format!("{T_LAUNCH:?}"), word(TOKEN), word(CURVE), word(Address::repeat_byte(0xde))], "data": data, "blockNumber": "0x3e8", "logIndex": "0x0", "transactionHash": format!("{h:?}")}]});
            }
            if let Some(env) = st.sent.iter().find(|e| *e.tx_hash() == h) {
                let v = alloy::consensus::Transaction::value(env);
                // the curve's own maths: fee 1 %, creator tax 2 %, opening tax ~2.97 % at the crossing, then x*y=k
                let net = v * U256::from(10_000 - 300 - 297) / U256::from(10_000);
                let (rq, rt) = (eth(10.0), U256::from(10u64).pow(U256::from(27u64)));
                let out = net * rt / (rq + net);
                let data = format!("0x{:0>64x}{:0>64x}{:0>64x}{:0>64x}", v, out, U256::ZERO, U256::ZERO);
                return json!({"status": "0x1", "blockNumber": "0x3e9", "transactionHash": format!("{h:?}"), "logs": [{"address": format!("{CURVE:?}"), "topics": [format!("{T_BUY:?}")], "data": data}]});
            }
            Value::Null
        }
        _ => Value::Null,
    }
}
fn crate_buy_selector() -> [u8; 4] { alloy::primitives::keccak256(b"buy(uint256,uint256,address)")[..4].try_into().unwrap() }

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let deployer = PrivateKeySigner::random();
    let params = TokenParams { name: "Mock Coin".into(), symbol: "MOCK".into(), logo: "".into(), description: "".into(), socials: Socials { twitter: "".into(), telegram: "".into(), discord: "".into(), website: "".into(), farcaster: "".into() }, creatorFeeRecipient: deployer.address(), creatorTaxBps: 200, buybackEnabled: false, expectedEconomics: B256::ZERO, salt: B256::ZERO };
    let launch = sign(&deployer, 0, LAUNCH_ROUTER, eth(0.5), launchAndBuyCall { params, launchConfigId: U256::from(1u64), pairToken: Address::ZERO, quoteIn: eth(0.5), minTokensOut: U256::ZERO, recipient: deployer.address(), snipeTaxExemptions: vec![deployer.address()] }.abi_encode());
    let dump = sign(&deployer, 1, CURVE, U256::ZERO, sellCall { tokensIn: U256::from(10u64).pow(U256::from(25u64)), minQuoteOut: U256::ZERO, recipient: deployer.address() }.abi_encode());
    let st = Arc::new(Mutex::new(State { seq: 100, launch_hash: *launch.tx_hash(), sent: vec![] }));
    eprintln!("MOCK deployer {:?} launch tx {:?} curve {CURVE:?} token {TOKEN:?}", deployer.address(), launch.tx_hash());
    tokio::spawn(rpc(st.clone(), cli.rpc.clone()));
    let l = TcpListener::bind(&cli.feed).await.unwrap();
    eprintln!("MOCK feed ws://{} rpc http://{}", cli.feed, cli.rpc);
    let _: Bytes = Bytes::new();
    loop {
        let (s, _) = l.accept().await.unwrap();
        let (st, launch, dump, sell_after, block_ms, launch_after) = (st.clone(), launch.clone(), dump.clone(), cli.sell_after_ms, cli.block_ms, cli.launch_after_ms);
        tokio::spawn(async move {
            let mut ws = match tokio_tungstenite::accept_async(s).await { Ok(w) => w, Err(_) => return };
            let t0 = std::time::Instant::now();
            let mut sent_launch = false; let mut sent_dump = false;
            loop {
                let seq = { let mut g = st.lock().unwrap(); g.seq += 1; g.seq };
                let txs: Vec<TxEnvelope> = if !sent_launch && t0.elapsed().as_millis() >= launch_after as u128 { sent_launch = true; eprintln!("MOCK feed: launch in block {seq}"); vec![launch.clone()] }
                    else if sent_launch && !sent_dump && t0.elapsed().as_millis() >= sell_after as u128 { sent_dump = true; eprintln!("MOCK feed: deployer sells in block {seq}"); vec![dump.clone()] }
                    else { vec![] };
                if ws.send(tokio_tungstenite::tungstenite::Message::Text(feed_msg(seq, &txs).into())).await.is_err() { return; }
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(block_ms)) => {}
                    m = ws.next() => { if matches!(m, None | Some(Err(_))) { return; } }
                }
            }
        });
    }
}
