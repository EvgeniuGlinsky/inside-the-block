//! Probe transactions.
//!
//! Every probe has the same shape and the same requested compute-unit limit, so the only
//! things that differ between the probes of one batch are the compute-unit price and the
//! moment they were sent. That is what lets the analysis attribute an ordering to either.
//!
//! The three instructions are encoded by hand (a few bytes each) instead of pulling the
//! interface crates, which would bring a second, incompatible `solana-instruction`.

use anyhow::{Context, Result};
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_message::Message;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Transaction;
use std::str::FromStr;

pub const COMPUTE_BUDGET: &str = "ComputeBudget111111111111111111111111111111";
pub const SYSTEM: &str = "11111111111111111111111111111111";
pub const MEMO: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

/// Base fee per signature, in lamports.
pub const BASE_FEE: u64 = 5_000;

fn pk(s: &str) -> Pubkey {
    Pubkey::from_str(s).expect("static program id")
}

pub fn set_cu_limit(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction { program_id: pk(COMPUTE_BUDGET), accounts: vec![], data }
}

pub fn set_cu_price(micro_lamports: u64) -> Instruction {
    let mut data = vec![3u8];
    data.extend_from_slice(&micro_lamports.to_le_bytes());
    Instruction { program_id: pk(COMPUTE_BUDGET), accounts: vec![], data }
}

pub fn transfer(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = 2u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: pk(SYSTEM),
        accounts: vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
        data,
    }
}

pub fn memo(text: &str) -> Instruction {
    Instruction { program_id: pk(MEMO), accounts: vec![], data: text.as_bytes().to_vec() }
}

/// Worst-case fee of a transaction with one signature: base fee plus priority fee
/// computed on the *requested* limit (that is what the runtime charges).
pub fn max_fee(cu_limit: u32, cu_price_micro: u64) -> u64 {
    BASE_FEE + (cu_price_micro as u128 * cu_limit as u128).div_ceil(1_000_000) as u64
}

pub struct Probe {
    pub tx: Transaction,
    pub wire: Vec<u8>,
    pub signature: String,
}

/// One probe: compute budget, an optional write to the shared sink (conflict mode), and a
/// memo that makes the probe self-describing on any explorer.
pub fn build_probe(
    payer: &Keypair,
    blockhash: &Hash,
    cu_limit: u32,
    cu_price_micro: u64,
    conflict_sink: Option<&Pubkey>,
    memo_text: &str,
) -> Result<Probe> {
    let mut ixs = vec![set_cu_limit(cu_limit), set_cu_price(cu_price_micro)];
    if let Some(sink) = conflict_sink {
        ixs.push(transfer(&payer.pubkey(), sink, 1));
    }
    ixs.push(memo(memo_text));
    finish(payer, blockhash, &ixs)
}

pub fn build_transfer(payer: &Keypair, blockhash: &Hash, to: &Pubkey, lamports: u64) -> Result<Probe> {
    finish(payer, blockhash, &[transfer(&payer.pubkey(), to, lamports)])
}

fn finish(payer: &Keypair, blockhash: &Hash, ixs: &[Instruction]) -> Result<Probe> {
    let message = Message::new_with_blockhash(ixs, Some(&payer.pubkey()), blockhash);
    let tx = Transaction::new(&[payer], message, blockhash.clone());
    let wire = bincode::serialize(&tx).context("serialize transaction")?;
    let signature = tx.signatures[0].to_string();
    Ok(Probe { tx, wire, signature })
}

pub fn read_keypair(path: &str) -> Result<Keypair> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read keypair {path}"))?;
    let bytes: Vec<u8> = serde_json::from_str(&raw).with_context(|| format!("keypair {path}: expected a JSON byte array"))?;
    Keypair::try_from(bytes.as_slice()).map_err(|e| anyhow::anyhow!("keypair {path}: {e}"))
}

pub fn write_keypair(path: &str, kp: &Keypair) -> Result<()> {
    let bytes = kp.to_bytes().to_vec();
    std::fs::write(path, serde_json::to_string(&bytes)?).with_context(|| format!("write keypair {path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_counts_requested_limit() {
        assert_eq!(max_fee(30_000, 0), 5_000);
        assert_eq!(max_fee(30_000, 100_000), 5_000 + 3_000);
        // Rounds up: 1 micro-lamport over 1 CU is still a whole lamport.
        assert_eq!(max_fee(1, 1), 5_001);
    }

    #[test]
    fn compute_budget_encoding() {
        assert_eq!(set_cu_limit(30_000).data, vec![2, 0x30, 0x75, 0, 0]);
        assert_eq!(set_cu_price(1).data, vec![3, 1, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn probe_is_signed_and_serialised() {
        let kp = Keypair::new();
        let p = build_probe(&kp, &Hash::default(), 30_000, 1_000, None, "itb test").unwrap();
        assert_eq!(p.tx.signatures.len(), 1);
        assert!(p.wire.len() < 1232, "fits a packet: {}", p.wire.len());
        assert_eq!(p.signature, p.tx.signatures[0].to_string());
    }
}
