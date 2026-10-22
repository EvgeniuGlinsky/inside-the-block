//! The measurement run: for every planned leader window, send one batch of probes and record
//! exactly what left this machine, when, and what slot our RPC node reported at that moment.
//!
//! Dry-run is the default. Probes are still built and signed (with throwaway payers), the real
//! leader schedule is followed and the trigger is fired at the real moment, so a dry run shows
//! whether the timing lands inside the window — it only skips the network write. Live mode
//! needs `--live` and is bounded by `SpendGuard`.

use crate::plan::{Plan, Target};
use crate::quic::TpuSender;
use crate::rpc::Rpc;
use crate::slots::{now_ns, SlotTracker};
use crate::txs::{self, Probe};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// All probes of the batch written back to back (well under a millisecond apart).
    Burst,
    /// Probes written `stagger_ms` apart, in an order independent of their price.
    Stagger,
    /// Like burst, but every probe also writes the same account, so the scheduler must serialise them.
    Conflict,
}

impl FromStr for Mode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "burst" => Ok(Mode::Burst),
            "stagger" => Ok(Mode::Stagger),
            "conflict" => Ok(Mode::Conflict),
            _ => bail!("unknown mode {s} (burst|stagger|conflict)"),
        }
    }
}

pub struct RunArgs {
    pub live: bool,
    pub keypair: Option<String>,
    pub keys_dir: PathBuf,
    pub max_spend_lamports: u64,
    pub batch_size: usize,
    pub cu_limit: u32,
    pub price_levels: Vec<u64>,
    pub modes: Vec<Mode>,
    pub stagger_ms: u64,
    pub lead_slots: u64,
    pub seed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendRecord {
    pub run_id: String,
    pub live: bool,
    pub batch_id: String,
    pub mode: Mode,
    pub identity: String,
    pub client_id: String,
    pub version: String,
    pub window_start: u64,
    /// Position in which this probe was written (0 = first).
    pub send_seq: usize,
    pub cu_price_micro: u64,
    pub cu_limit: u32,
    pub payer: String,
    pub signature: String,
    pub path: String,
    pub sent_at_ns: u64,
    pub observed_slot: u64,
    pub observed_slot_age_ms: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchRecord {
    pub run_id: String,
    pub live: bool,
    pub batch_id: String,
    pub mode: Mode,
    pub identity: String,
    pub client_id: String,
    pub window_start: u64,
    pub trigger_slot: u64,
    /// Observed slot when the first probe left, and whether it was inside the window.
    pub observed_at_first_send: u64,
    pub in_window_at_send: bool,
    pub send_span_us: u64,
    pub skipped_reason: Option<String>,
}

/// Hard limit on lamports spent on fees. Checked *before* every transaction with its
/// worst-case fee, so the cap holds even if every probe lands and pays in full.
pub struct SpendGuard {
    cap: u64,
    committed: u64,
}

impl SpendGuard {
    pub fn new(cap: u64) -> Self {
        Self { cap, committed: 0 }
    }
    pub fn reserve(&mut self, worst_case_fee: u64) -> Result<()> {
        if self.committed + worst_case_fee > self.cap {
            bail!(
                "spend cap reached: {} + {} > {} lamports",
                self.committed,
                worst_case_fee,
                self.cap
            );
        }
        self.committed += worst_case_fee;
        Ok(())
    }
    pub fn committed(&self) -> u64 {
        self.committed
    }
}

struct Jsonl {
    file: std::fs::File,
}

impl Jsonl {
    fn open(path: &Path) -> Result<Self> {
        let file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file })
    }
    fn write<T: Serialize>(&mut self, v: &T) -> Result<()> {
        writeln!(self.file, "{}", serde_json::to_string(v)?)?;
        self.file.flush()?;
        Ok(())
    }
}

/// Keeps a recent blockhash around: post-Alpenglow slots are ~0.2 s, so a blockhash ages out
/// in well under a minute and must be refreshed in the background.
#[derive(Clone)]
struct Blockhash(Arc<Mutex<Hash>>);

impl Blockhash {
    async fn start(rpc: Rpc) -> Result<Self> {
        let (h, _) = rpc.latest_blockhash().await?;
        let b = Blockhash(Arc::new(Mutex::new(Hash::from_str(&h).map_err(|e| anyhow!("blockhash: {e}"))?)));
        let bg = b.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(4)).await;
                if let Ok((h, _)) = rpc.latest_blockhash().await {
                    if let Ok(h) = Hash::from_str(&h) {
                        *bg.0.lock().unwrap() = h;
                    }
                }
            }
        });
        Ok(b)
    }
    fn get(&self) -> Hash {
        self.0.lock().unwrap().clone()
    }
}

struct Window<'a> {
    target: &'a Target,
    start: u64,
    mode: Mode,
}

pub async fn run(rpc: &Rpc, ws_url: &str, plan: &Plan, a: &RunArgs, data_dir: &Path) -> Result<String> {
    if a.batch_size < 2 || a.batch_size > a.price_levels.len() {
        bail!("batch size must be between 2 and the number of price levels ({})", a.price_levels.len());
    }
    let run_id = format!("{}-{}", if a.live { "live" } else { "dry" }, now_ns() / 1_000_000_000);
    let mut rng = StdRng::seed_from_u64(a.seed);
    let mut sends = Jsonl::open(&data_dir.join("sends.jsonl"))?;
    let mut batches = Jsonl::open(&data_dir.join("batches.jsonl"))?;

    // Windows of all targets in time order; modes rotate over each validator's windows so every
    // validator gets every mode.
    let mut windows: Vec<Window> = Vec::new();
    for t in &plan.targets {
        for (i, w) in t.windows.iter().enumerate() {
            windows.push(Window { target: t, start: *w, mode: a.modes[i % a.modes.len()] });
        }
    }
    windows.sort_by_key(|w| w.start);

    let worst_probe = txs::max_fee(a.cu_limit, *a.price_levels.iter().max().unwrap());
    let probes_planned = (windows.len() * a.batch_size) as u64;
    let mut guard = SpendGuard::new(a.max_spend_lamports);

    // Payers. Live: one funded sub-payer per batch position, so probes of a batch never share a
    // writable account (except the deliberate sink in conflict mode). Dry: throwaway keys.
    // A dry run never touches the wallet: a configured but absent keypair file is fine there.
    let main = match (&a.keypair, a.live) {
        (Some(p), true) => Some(txs::read_keypair(p)?),
        (Some(p), false) if Path::new(p).exists() => Some(txs::read_keypair(p)?),
        (None, true) => bail!("--live needs --keypair (or KEYPAIR) pointing at a funded keypair file"),
        _ => None,
    };
    let sink: Pubkey = main.as_ref().map(|k| k.pubkey()).unwrap_or_else(|| Keypair::new().pubkey());
    let payers: Vec<Keypair> = (0..a.batch_size).map(|_| Keypair::new()).collect();
    let tracker = SlotTracker::start(rpc.clone(), ws_url.to_string()).await;
    let blockhash = Blockhash::start(rpc.clone()).await?;

    let subpayer_file = a.keys_dir.join(format!("subpayers-{run_id}.json"));
    if a.live {
        let main = main.as_ref().unwrap();
        let rent = rpc.rent_exempt_minimum(0).await?;
        let batches_per_payer = windows.len() as u64;
        // Each sub-payer pays for one probe per batch (+1 lamport sink transfer in conflict mode).
        let per_payer = rent + batches_per_payer * (worst_probe + 1) + 10_000;
        let funding_cost = per_payer * a.batch_size as u64 + txs::BASE_FEE * a.batch_size as u64;
        let balance = rpc.balance(&main.pubkey().to_string()).await?;
        println!(
            "live: main {} balance {} lamports; funding {} sub-payers x {} = {} (rent is returned by sweep)",
            main.pubkey(),
            balance,
            a.batch_size,
            per_payer,
            per_payer * a.batch_size as u64
        );
        // The main wallet must stay rent-exempt itself after funding, or the funding transfers fail.
        if balance < funding_cost + rent + txs::BASE_FEE * 4 {
            bail!(
                "main wallet holds {balance} lamports, run needs about {} (funding {funding_cost} + its own rent {rent})",
                funding_cost + rent
            );
        }
        // Save sub-payer keys before funding them, so a crash never strands lamports.
        let keys: Vec<Vec<u8>> = payers.iter().map(|k| k.to_bytes().to_vec()).collect();
        std::fs::create_dir_all(&a.keys_dir)?;
        std::fs::write(&subpayer_file, serde_json::to_string(&keys)?)
            .with_context(|| format!("write {}", subpayer_file.display()))?;
        let bh = blockhash.get();
        let mut funding_sigs = Vec::new();
        for p in &payers {
            guard.reserve(txs::BASE_FEE)?;
            let t = txs::build_transfer(main, &bh, &p.pubkey(), per_payer)?;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&t.wire);
            funding_sigs.push(rpc.send_raw(&b64).await?);
        }
        wait_confirmed(rpc, &funding_sigs, Duration::from_secs(60)).await?;
        println!("live: sub-payers funded");
    } else {
        println!(
            "dry-run: {} windows x {} probes = {} probes would be sent, worst-case fees {} lamports (cap {})",
            windows.len(),
            a.batch_size,
            probes_planned,
            probes_planned * worst_probe,
            a.max_spend_lamports
        );
    }

    let sender = TpuSender::new()?;
    for (bi, w) in windows.iter().enumerate() {
        let batch_id = format!("{run_id}-b{bi:03}");
        let trigger = w.start.saturating_sub(a.lead_slots);
        let now = tracker.current();
        let mut batch = BatchRecord {
            run_id: run_id.clone(),
            live: a.live,
            batch_id: batch_id.clone(),
            mode: w.mode,
            identity: w.target.identity.clone(),
            client_id: w.target.client_id.clone(),
            window_start: w.start,
            trigger_slot: trigger,
            observed_at_first_send: 0,
            in_window_at_send: false,
            send_span_us: 0,
            skipped_reason: None,
        };
        if now >= w.start {
            batch.skipped_reason = Some(format!("window already started (observed slot {now})"));
            batches.write(&batch)?;
            continue;
        }

        // Connect a few slots ahead so the handshake is not part of the measured send.
        while tracker.current() + 12 < trigger {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let conn = match (&w.target.tpu_quic, a.live) {
            (Some(addr), true) => match sender.connect(addr).await {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("{batch_id}: QUIC connect to {addr} failed: {e}; falling back to RPC");
                    None
                }
            },
            _ => None,
        };

        // Prices are a random permutation of the levels, assigned independently of send order.
        let mut prices: Vec<u64> = a.price_levels.clone();
        prices.shuffle(&mut rng);
        prices.truncate(a.batch_size);
        let mut order: Vec<usize> = (0..a.batch_size).collect();
        order.shuffle(&mut rng);

        if !tracker.wait_for(trigger, Duration::from_secs(120)).await {
            batch.skipped_reason = Some("slot tracker did not reach the trigger slot".into());
            batches.write(&batch)?;
            continue;
        }
        let bh = blockhash.get();
        let mut built: Vec<(usize, Probe, u64)> = Vec::with_capacity(a.batch_size);
        for (seq, &payer_idx) in order.iter().enumerate() {
            let price = prices[payer_idx];
            let memo = format!("itb {} s{} p{} {:?}", batch_id, seq, price, w.mode).to_lowercase();
            let sink_ref = (w.mode == Mode::Conflict).then_some(&sink);
            let p = txs::build_probe(&payers[payer_idx], &bh, a.cu_limit, price, sink_ref, &memo)?;
            built.push((seq, p, price));
        }

        // If the tracker shows we are already past the middle of the window, the probes would land
        // with the next leader; skip instead of paying for a batch that measures the wrong node.
        if tracker.current() >= w.start + 2 {
            batch.skipped_reason = Some(format!("late: observed slot {} at send time", tracker.current()));
            batches.write(&batch)?;
            if let Some(c) = conn {
                c.close(0u32.into(), b"late");
            }
            continue;
        }
        let first_ns = now_ns();
        batch.observed_at_first_send = tracker.current();
        batch.in_window_at_send = batch.observed_at_first_send >= w.start.saturating_sub(a.lead_slots)
            && batch.observed_at_first_send < w.start + 4;
        for (seq, probe, price) in &built {
            if w.mode == Mode::Stagger && *seq > 0 {
                tokio::time::sleep(Duration::from_millis(a.stagger_ms)).await;
            }
            let mut rec = SendRecord {
                run_id: run_id.clone(),
                live: a.live,
                batch_id: batch_id.clone(),
                mode: w.mode,
                identity: w.target.identity.clone(),
                client_id: w.target.client_id.clone(),
                version: w.target.version.clone(),
                window_start: w.start,
                send_seq: *seq,
                cu_price_micro: *price,
                cu_limit: a.cu_limit,
                payer: probe.tx.message.account_keys[0].to_string(),
                signature: probe.signature.clone(),
                path: "dry-run".into(),
                sent_at_ns: 0,
                observed_slot: tracker.current(),
                observed_slot_age_ms: tracker.age_ms(),
                error: None,
            };
            if a.live {
                let fee = txs::max_fee(a.cu_limit, *price) + if w.mode == Mode::Conflict { 1 } else { 0 };
                if let Err(e) = guard.reserve(fee) {
                    rec.error = Some(e.to_string());
                    sends.write(&rec)?;
                    batches.write(&batch)?;
                    println!("stopping: {e}");
                    return finish(rpc, a, main.as_ref(), &payers, &subpayer_file, &guard, run_id).await;
                }
                let res = match &conn {
                    Some(c) => {
                        rec.path = "quic-direct".into();
                        TpuSender::send(c, &probe.wire).await
                    }
                    None => {
                        rec.path = "rpc".into();
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&probe.wire);
                        rpc.send_raw(&b64).await.map(|_| ())
                    }
                };
                if let Err(e) = res {
                    rec.error = Some(e.to_string());
                }
            }
            rec.sent_at_ns = now_ns();
            sends.write(&rec)?;
        }
        batch.send_span_us = (now_ns() - first_ns) / 1000;
        batches.write(&batch)?;
        println!(
            "{batch_id} {:<18} {:?} window {} trigger {} observed {} in-window={} span {} us",
            w.target.client_id,
            w.mode,
            w.start,
            trigger,
            batch.observed_at_first_send,
            batch.in_window_at_send,
            batch.send_span_us
        );
        if let Some(c) = conn {
            c.close(0u32.into(), b"done");
        }
    }

    finish(rpc, a, main.as_ref(), &payers, &subpayer_file, &guard, run_id).await
}

async fn finish(
    rpc: &Rpc,
    a: &RunArgs,
    main: Option<&Keypair>,
    payers: &[Keypair],
    subpayer_file: &Path,
    guard: &SpendGuard,
    run_id: String,
) -> Result<String> {
    if a.live {
        println!("live: waiting 20 s for the last probes to settle before sweeping");
        tokio::time::sleep(Duration::from_secs(20)).await;
        sweep(rpc, main.unwrap(), payers).await?;
        let _ = std::fs::remove_file(subpayer_file);
        println!("live: fees committed (worst case) {} lamports", guard.committed());
    }
    Ok(run_id)
}

/// Send every sub-payer's remaining lamports back to the main wallet (the sub-payer pays the fee).
pub async fn sweep(rpc: &Rpc, main: &Keypair, payers: &[Keypair]) -> Result<()> {
    let (h, _) = rpc.latest_blockhash().await?;
    let bh = Hash::from_str(&h).map_err(|e| anyhow!("blockhash: {e}"))?;
    let mut sigs = Vec::new();
    for p in payers {
        let bal = rpc.balance(&p.pubkey().to_string()).await?;
        if bal <= txs::BASE_FEE {
            continue;
        }
        let t = txs::build_transfer(p, &bh, &main.pubkey(), bal - txs::BASE_FEE)?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&t.wire);
        match rpc.send_raw(&b64).await {
            Ok(s) => sigs.push(s),
            Err(e) => eprintln!("sweep {}: {e}", p.pubkey()),
        }
    }
    wait_confirmed(rpc, &sigs, Duration::from_secs(60)).await?;
    println!("swept {} sub-payers back to {}", sigs.len(), main.pubkey());
    Ok(())
}

async fn wait_confirmed(rpc: &Rpc, sigs: &[String], timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let st = rpc.signature_statuses(sigs).await?;
        let done = st
            .iter()
            .filter(|s| matches!(s["confirmationStatus"].as_str(), Some("confirmed") | Some("finalized")))
            .count();
        if done == sigs.len() {
            return Ok(());
        }
        if tokio::time::Instant::now() > deadline {
            bail!("only {done}/{} transactions confirmed in time", sigs.len());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::SpendGuard;

    #[test]
    fn guard_refuses_past_cap() {
        let mut g = SpendGuard::new(10_000);
        assert!(g.reserve(6_000).is_ok());
        assert!(g.reserve(4_000).is_ok());
        assert!(g.reserve(1).is_err());
        assert_eq!(g.committed(), 10_000);
    }
}
