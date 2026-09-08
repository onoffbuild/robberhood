//! The signing bank: nothing is signed on the hot path. Every intent is signed ahead of time
//! and re-signed when the nonce or the gas price moves. Firing is a memcpy and a socket write.

use alloy::consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy::eips::eip2718::Encodable2718;
use alloy::network::TxSignerSync;
use alloy::primitives::{Address, Bytes, TxKind, U256};
use alloy::signers::local::PrivateKeySigner;
use anyhow::Result;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy)]
pub struct Gas { pub max_fee: u128, pub max_priority: u128, pub limit: u64 }

/// One pre-signed transaction, ready to send.
#[derive(Debug, Clone)]
pub struct Armed { pub nonce: u64, pub raw: Bytes, pub label: String }

pub struct Bank {
    signer: PrivateKeySigner,
    chain_id: u64,
    /// the next nonce this wallet will use; advanced on every fire, resynced from the chain on confirm
    pub nonce: u64,
    pub gas: Gas,
    armed: HashMap<String, Armed>,
}

impl Bank {
    pub fn new(signer: PrivateKeySigner, chain_id: u64, nonce: u64, gas: Gas) -> Self {
        Self { signer, chain_id, nonce, gas, armed: HashMap::new() }
    }
    pub fn address(&self) -> Address { self.signer.address() }

    fn sign(&self, to: Address, value: U256, input: Vec<u8>, nonce: u64) -> Result<Bytes> {
        let mut tx = TxEip1559 {
            chain_id: self.chain_id, nonce, gas_limit: self.gas.limit,
            max_fee_per_gas: self.gas.max_fee, max_priority_fee_per_gas: self.gas.max_priority,
            to: TxKind::Call(to), value, input: input.into(), access_list: Default::default(),
        };
        let sig = self.signer.sign_transaction_sync(&mut tx)?;
        let env: TxEnvelope = tx.into_signed(sig).into();
        Ok(env.encoded_2718().into())
    }

    /// Arm an intent under a label. All armed intents share the current nonce: whichever fires
    /// first wins, the rest are invalidated and must be re-armed with the next nonce.
    pub fn arm(&mut self, label: &str, to: Address, value: U256, input: Vec<u8>) -> Result<&Armed> {
        let raw = self.sign(to, value, input, self.nonce)?;
        self.armed.insert(label.to_string(), Armed { nonce: self.nonce, raw, label: label.to_string() });
        Ok(&self.armed[label])
    }

    /// Take an armed intent for firing. Bumps the nonce, so every other armed intent is now stale.
    pub fn fire(&mut self, label: &str) -> Option<Armed> {
        let a = self.armed.remove(label)?;
        if a.nonce != self.nonce { return None; }
        self.nonce += 1;
        self.armed.clear();
        Some(a)
    }

    pub fn stale(&self) -> Vec<String> {
        self.armed.values().filter(|a| a.nonce != self.nonce).map(|a| a.label.clone()).collect()
    }
    pub fn labels(&self) -> Vec<&str> { self.armed.keys().map(String::as_str).collect() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arm_fire_invalidates_siblings() {
        let s = PrivateKeySigner::random();
        let mut b = Bank::new(s, 4663, 7, Gas { max_fee: 100_000_000, max_priority: 0, limit: 300_000 });
        b.arm("buy:5", Address::ZERO, U256::from(1u64), vec![]).unwrap();
        b.arm("buy:10", Address::ZERO, U256::from(1u64), vec![]).unwrap();
        let f = b.fire("buy:5").unwrap();
        assert_eq!(f.nonce, 7);
        assert_eq!(b.nonce, 8);
        assert!(b.fire("buy:10").is_none());
    }
}
