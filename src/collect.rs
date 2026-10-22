//! Fetch the blocks our probes could have landed in and record where each probe sits.
//!
//! For every probed window we read the leader's own slots and a few slots after them (late
//! landings go to whoever leads next, and that is a result too). Every block is saved in full
//! as compact rows — index, signature, fee, compute units, error flag — under `data/blocks/`,
//! so a judge can re-derive every position from the saved file or with their own `getBlock`.

use crate::rpc::Rpc;
use crate::run::SendRecord;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockRow {
    pub index: usize,
    pub signature: String,
    pub fee: u64,
    pub cu_consumed: u64,
    pub failed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockFile {
    pub slot: u64,
    pub leader: String,
    pub parent_slot: u64,
    pub block_time: Option<i64>,
    pub blockhash: String,
    pub tx_count: usize,
    pub rows: Vec<BlockRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Landed {
    pub signature: String,
    pub batch_id: String,
    pub slot: u64,
    pub leader: String,
    pub index: usize,
    pub block_tx_count: usize,
    pub fee: u64,
    pub cu_consumed: u64,
    pub failed: bool,
}

/// How many slots after the leader's window are still searched for late landings.
pub const SPILLOVER_SLOTS: u64 = 8;

pub fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let text = std::fs::read_to_string(path)?;
    Ok(text.lines().filter(|l| !l.trim().is_empty()).map(serde_json::from_str).collect::<Result<_, _>>()?)
}

pub async fn collect(rpc: &Rpc, data_dir: &Path) -> Result<()> {
    let sends: Vec<SendRecord> = read_jsonl(&data_dir.join("sends.jsonl"))?;
    let live: Vec<&SendRecord> = sends.iter().filter(|s| s.live && s.error.is_none()).collect();
    if live.is_empty() {
        println!("collect: no live sends recorded yet (dry runs send nothing, so there is nothing to find)");
        return Ok(());
    }
    let by_sig: HashMap<&str, &SendRecord> = live.iter().map(|s| (s.signature.as_str(), *s)).collect();

    // Ask the cluster where each signature landed and read exactly those blocks. This also finds
    // probes that missed their window and were included many slots later by another leader;
    // a probe with no status never landed (dropped, or expired with its blockhash).
    let sigs: Vec<String> = live.iter().map(|s| s.signature.clone()).collect();
    let mut slots: BTreeSet<u64> = BTreeSet::new();
    let mut late = 0usize;
    for chunk in sigs.chunks(256) {
        let statuses = rpc.signature_statuses(chunk).await?;
        for (st, sig) in statuses.iter().zip(chunk) {
            if let Some(slot) = st["slot"].as_u64() {
                slots.insert(slot);
                let w = by_sig[sig.as_str()].window_start;
                if slot >= w + 4 + SPILLOVER_SLOTS {
                    late += 1;
                }
            }
        }
    }
    if slots.is_empty() {
        println!("collect: none of {} live probes has landed (yet)", sigs.len());
        return Ok(());
    }
    println!("collect: probes landed in {} blocks ({late} probes far after their window)", slots.len());

    let blocks_dir = data_dir.join("blocks");
    std::fs::create_dir_all(&blocks_dir)?;
    let already: HashSet<String> = read_jsonl::<Landed>(&data_dir.join("landed.jsonl"))?
        .into_iter()
        .map(|l| l.signature)
        .collect();
    let mut out = std::fs::OpenOptions::new().create(true).append(true).open(data_dir.join("landed.jsonl"))?;

    let lo = *slots.iter().next().unwrap();
    let hi = *slots.iter().last().unwrap();
    // getSlotLeaders is limited to 5000 slots per call.
    let mut leaders: HashMap<u64, String> = HashMap::new();
    let mut start = lo;
    while start <= hi {
        let n = (hi - start + 1).min(5000);
        for (i, l) in rpc.slot_leaders(start, n).await?.into_iter().enumerate() {
            leaders.insert(start + i as u64, l);
        }
        start += n;
    }

    let mut found = 0usize;
    for slot in slots {
        let path = blocks_dir.join(format!("{slot}.json"));
        let block: Option<BlockFile> = if path.exists() {
            Some(serde_json::from_str(&std::fs::read_to_string(&path)?)?)
        } else {
            // Stay under the public RPC's getBlock rate limit.
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
            match rpc.block(slot).await? {
                None => None,
                Some(v) => {
                    let rows: Vec<BlockRow> = v["transactions"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .enumerate()
                        .map(|(i, t)| BlockRow {
                            index: i,
                            signature: t["transaction"]["signatures"][0].as_str().unwrap_or("").to_string(),
                            fee: t["meta"]["fee"].as_u64().unwrap_or(0),
                            cu_consumed: t["meta"]["computeUnitsConsumed"].as_u64().unwrap_or(0),
                            failed: !t["meta"]["err"].is_null(),
                        })
                        .collect();
                    let b = BlockFile {
                        slot,
                        leader: leaders.get(&slot).cloned().unwrap_or_default(),
                        parent_slot: v["parentSlot"].as_u64().unwrap_or(0),
                        block_time: v["blockTime"].as_i64(),
                        blockhash: v["blockhash"].as_str().unwrap_or("").to_string(),
                        tx_count: rows.len(),
                        rows,
                    };
                    std::fs::write(&path, serde_json::to_string(&b)?)?;
                    Some(b)
                }
            }
        };
        let Some(b) = block else { continue };
        for r in &b.rows {
            if let Some(s) = by_sig.get(r.signature.as_str()) {
                if already.contains(&r.signature) {
                    continue;
                }
                let l = Landed {
                    signature: r.signature.clone(),
                    batch_id: s.batch_id.clone(),
                    slot: b.slot,
                    leader: b.leader.clone(),
                    index: r.index,
                    block_tx_count: b.tx_count,
                    fee: r.fee,
                    cu_consumed: r.cu_consumed,
                    failed: r.failed,
                };
                writeln!(out, "{}", serde_json::to_string(&l)?)?;
                found += 1;
            }
        }
    }
    println!("collect: {found} new probes located; blocks saved under {}", blocks_dir.display());
    Ok(())
}
