//! Turn sends + landings into per-validator answers.
//!
//! Unit of evidence: a *block group* — probes of one batch that landed in the same block. Within
//! a group every pair of probes is compared twice:
//!   * price test — did the probe with the higher compute-unit price come earlier?
//!   * arrival test — did the probe written earlier come earlier?
//! Prices are a random permutation independent of send order, so the two tests are not
//! confounded *on average*. With 4 probes per batch a single batch can still correlate price and
//! send order by chance, so a third test looks only at the pairs where the two rules disagree:
//!   * head-to-head — of the pairs where the higher-priced probe was written *later*, how often
//!     did price win?
//! Each test is summarised as Kendall's tau over all pairs (+1: always agrees, 0: unrelated,
//! -1: always the opposite) with a permutation p-value that shuffles positions inside each group,
//! which respects that pairs from one block are not independent. The p-values are then corrected
//! (Benjamini–Hochberg) within each level of the question — per validator, per client, per client
//! and mode — and verdicts use the corrected q-values.
//!
//! Only blocks produced by the targeted validator count as its evidence: a probe that missed the
//! window and was included by the next leader says nothing about the targeted leader's rule.
//!
//! Around the main table, `RESULTS.md` carries the checks a reader needs to weigh a verdict:
//!   * a block-level bootstrap 95% interval for every tau, and per cell the smallest |tau| it
//!     could detect, so "no rule detected" is not read as "proven random";
//!   * no rule is claimed from fewer than `MIN_BLOCKS` blocks, however clean the pairs look;
//!   * the same tests restricted to the planned window, and split by the slot a probe landed in;
//!   * tau per run, to show whether a sign holds from one run to the next;
//!   * secondary evidence from everybody else's transactions: does the block follow fee per CU?

use crate::collect::{read_jsonl, BlockFile, Landed};
use crate::run::{BatchRecord, Mode, SendRecord};
use crate::txs::BASE_FEE;
use anyhow::Result;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

#[derive(Clone)]
struct Probe {
    seq: usize,
    price: u64,
    index: usize,
}

/// Probes of one batch that landed in one block of the targeted leader.
#[derive(Clone)]
struct Group {
    run_id: String,
    /// Landing slot minus the first slot of the planned window.
    offset: u64,
    block_tx_count: usize,
    probes: Vec<Probe>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct TestResult {
    pub pairs: usize,
    pub agree: usize,
    pub tau: f64,
    pub p_value: f64,
    /// Benjamini–Hochberg q-value across every test of the analysis.
    pub q_value: f64,
    /// Block-level bootstrap 95% interval of tau: blocks are resampled with replacement.
    pub tau_ci: Option<[f64; 2]>,
    /// Smallest |tau| this cell would detect with 80% power at p < 0.05 (see `detectable`).
    pub detectable_tau: f64,
}

#[derive(Clone, Copy)]
enum Criterion {
    Price,
    Arrival,
    /// Pairs where price and send order disagree; "agree" = price won.
    HeadToHead,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Summary {
    pub key: String,
    pub client_id: String,
    pub batches: usize,
    pub sent: usize,
    pub landed: usize,
    /// Landed in a block produced by the targeted validator (the only evidence used).
    pub landed_in_window: usize,
    pub groups: usize,
    pub price: TestResult,
    pub arrival: TestResult,
    pub head_to_head: TestResult,
    /// The larger of the price and arrival detectable |tau|: below it a rule could hide.
    pub detectable_tau: f64,
    pub median_position: Option<f64>,
    pub verdict: String,
}

/// Price and arrival tau of one client and mode within a single run.
#[derive(Debug, Default, Clone, Serialize)]
pub struct RunCell {
    pub run_id: String,
    pub blocks: usize,
    pub pairs: usize,
    pub price_tau: f64,
    pub arrival_tau: f64,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct RunRow {
    pub key: String,
    pub runs: Vec<RunCell>,
}

/// Secondary evidence: how the rest of a targeted leader's block is ordered.
#[derive(Debug, Default, Clone, Serialize)]
pub struct CompetitionRow {
    pub client_id: String,
    /// The targeted leader's blocks that hold at least one of our probes.
    pub blocks: usize,
    pub median_block_txs: usize,
    /// Kendall tau between position and fee per CU over the other transactions of a block
    /// (+1: the block is sorted by fee per CU, highest first). Median and interquartile range.
    pub median_fee_tau: f64,
    pub fee_tau_iqr: [f64; 2],
    /// Median position of our probes in these blocks (0 = top, 1 = bottom).
    pub probe_median_position: f64,
    /// Of the transactions that paid more per CU than one of our probes, the share placed after
    /// it; median over probes. A block sorted by fee per CU would give 0.
    pub outpaid_below_probe: f64,
    pub probes: usize,
}

/// Where the probes of one client went: the path from our machine to a block, not the ordering.
#[derive(Debug, Default, Clone, Serialize)]
pub struct PathRow {
    pub client_id: String,
    /// Every live send attempt; the next four columns add up to it.
    pub sent: usize,
    /// The QUIC stream was refused (e.g. the leader closed our unstaked connection).
    pub rejected_at_send: usize,
    pub own_block: usize,
    pub other_leader: usize,
    pub not_landed: usize,
    /// Median slots between the window start and a landing in another leader's block.
    pub other_leader_median_offset: Option<u64>,
}

fn paths(sends: &[SendRecord], landed: &HashMap<&str, &Landed>) -> Vec<PathRow> {
    let mut by: BTreeMap<&str, (PathRow, Vec<u64>)> = BTreeMap::new();
    for s in sends.iter().filter(|s| s.live) {
        let (r, offsets) = by
            .entry(s.client_id.as_str())
            .or_insert_with(|| (PathRow { client_id: s.client_id.clone(), ..Default::default() }, vec![]));
        r.sent += 1;
        if s.error.is_some() {
            r.rejected_at_send += 1;
            continue;
        }
        match landed.get(s.signature.as_str()) {
            Some(l) if l.leader == s.identity => r.own_block += 1,
            Some(l) => {
                r.other_leader += 1;
                offsets.push(l.slot.saturating_sub(s.window_start));
            }
            None => r.not_landed += 1,
        }
    }
    by.into_values()
        .map(|(mut r, mut o)| {
            o.sort_unstable();
            r.other_leader_median_offset = (!o.is_empty()).then(|| o[o.len() / 2]);
            r
        })
        .collect()
}

/// Pairs of one block that the criterion compares, and how many of them follow it.
fn score_group(g: &[Probe], crit: Criterion) -> (usize, usize) {
    let (mut agree, mut compared) = (0usize, 0usize);
    for i in 0..g.len() {
        for j in 0..g.len() {
            if g[i].index >= g[j].index {
                continue;
            }
            // i is earlier in the block than j.
            let by_price = g[i].price > g[j].price;
            let by_arrival = g[i].seq < g[j].seq;
            let ok = match crit {
                Criterion::Price if g[i].price == g[j].price => continue,
                Criterion::Price => by_price,
                Criterion::Arrival => by_arrival,
                // A pair where both rules predict the same order cannot tell them apart.
                Criterion::HeadToHead if g[i].price == g[j].price || by_price == by_arrival => continue,
                Criterion::HeadToHead => by_price,
            };
            compared += 1;
            agree += ok as usize;
        }
    }
    (agree, compared)
}

fn tau_of(agree: usize, compared: usize) -> f64 {
    if compared == 0 {
        f64::NAN
    } else {
        (2.0 * agree as f64 - compared as f64) / compared as f64
    }
}

/// tau-like score of one ordering criterion over all groups: (agree - disagree) / compared.
fn score(groups: &[Vec<Probe>], crit: Criterion) -> (usize, usize, f64) {
    let (mut agree, mut compared) = (0usize, 0usize);
    for g in groups {
        let (a, c) = score_group(g, crit);
        agree += a;
        compared += c;
    }
    (agree, compared, tau_of(agree, compared))
}

fn test(groups: &[Vec<Probe>], crit: Criterion, perms: usize, rng: &mut StdRng) -> TestResult {
    let (agree, pairs, tau) = score(groups, crit);
    if pairs == 0 {
        return TestResult { pairs, agree, tau, p_value: f64::NAN, q_value: f64::NAN, tau_ci: None, detectable_tau: f64::NAN };
    }
    let mut extreme = 0usize;
    let mut null = Vec::with_capacity(perms);
    let mut shuffled: Vec<Vec<Probe>> = groups.to_vec();
    for _ in 0..perms {
        for (g, orig) in shuffled.iter_mut().zip(groups) {
            let mut idx: Vec<usize> = orig.iter().map(|p| p.index).collect();
            idx.shuffle(rng);
            for (p, i) in g.iter_mut().zip(idx) {
                p.index = i;
            }
        }
        let (_, _, t) = score(&shuffled, crit);
        if t.abs() >= tau.abs() - 1e-12 {
            extreme += 1;
        }
        null.push(t);
    }
    TestResult {
        pairs,
        agree,
        tau,
        p_value: (extreme as f64 + 1.0) / (perms as f64 + 1.0),
        q_value: f64::NAN,
        tau_ci: None,
        detectable_tau: detectable(&null),
    }
}

/// Smallest |tau| a cell detects with 80% power at two-sided p < 0.05, read off the cell's own
/// permutation null (same blocks, same pairs): the 95th percentile of |tau| under the null plus
/// 0.84 null standard deviations. That is the normal approximation of power and assumes the
/// estimate spreads about as much under a real effect as under the null. It ignores the
/// Benjamini–Hochberg correction, so the threshold that a verdict actually needs is a little higher.
fn detectable(null: &[f64]) -> f64 {
    if null.is_empty() {
        return f64::NAN;
    }
    let n = null.len() as f64;
    let mean = null.iter().sum::<f64>() / n;
    let sd = (null.iter().map(|t| (t - mean).powi(2)).sum::<f64>() / n).sqrt();
    let mut abs: Vec<f64> = null.iter().map(|t| t.abs()).collect();
    abs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let critical = abs[((0.95 * n).ceil() as usize).clamp(1, abs.len()) - 1];
    (critical + 0.8416 * sd).min(1.0)
}

const BOOTSTRAP: usize = 2_000;

/// Block-level bootstrap: resample whole blocks with replacement (pairs inside a block are not
/// independent) and take the 2.5th and 97.5th percentile of tau. Per-block counts are added up,
/// so a resample costs no re-scoring.
fn bootstrap_ci(groups: &[Vec<Probe>], crit: Criterion, rng: &mut StdRng) -> Option<[f64; 2]> {
    let counts: Vec<(usize, usize)> = groups.iter().map(|g| score_group(g, crit)).collect();
    if counts.iter().all(|c| c.1 == 0) {
        return None;
    }
    let mut taus = Vec::with_capacity(BOOTSTRAP);
    for _ in 0..BOOTSTRAP {
        let (mut agree, mut compared) = (0usize, 0usize);
        for _ in 0..counts.len() {
            let (a, c) = counts[rng.gen_range(0..counts.len())];
            agree += a;
            compared += c;
        }
        if compared > 0 {
            taus.push(tau_of(agree, compared));
        }
    }
    if taus.is_empty() {
        return None;
    }
    taus.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| taus[(q * (taus.len() - 1) as f64).round() as usize];
    Some([at(0.025), at(0.975)])
}

/// Benjamini–Hochberg: q_(k) = min over j >= k of p_(j) * m / j, over all finite p-values.
fn fill_q_values(tests: &mut [&mut TestResult]) {
    let mut order: Vec<usize> = (0..tests.len()).filter(|&i| tests[i].p_value.is_finite()).collect();
    order.sort_by(|&a, &b| tests[a].p_value.partial_cmp(&tests[b].p_value).unwrap());
    let m = order.len() as f64;
    let mut running = 1.0f64;
    for (rank, &i) in order.iter().enumerate().rev() {
        running = running.min(tests[i].p_value * m / (rank as f64 + 1.0));
        tests[i].q_value = running;
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Family {
    Validator,
    Client,
    ClientMode,
}

impl Family {
    fn of(key: &str) -> Family {
        if key.starts_with("validator ") {
            Family::Validator
        } else if key.contains(" / ") {
            Family::ClientMode
        } else {
            Family::Client
        }
    }
}

const MIN_PAIRS: usize = 10;
/// Pairs from one block share that block's conditions; a rule seen in two or three blocks may be
/// those blocks, not the client. Below this many blocks a cell is shown but not judged.
const MIN_BLOCKS: usize = 4;
const ALPHA: f64 = 0.05;
/// A leader window is a group of 4 consecutive slots; a landing further from the window start is
/// in a later window of the same leader (the planned slot had passed).
const WINDOW_SLOTS: u64 = 4;

fn verdict(r: &Summary) -> String {
    let sig = |t: &TestResult| t.pairs >= MIN_PAIRS && t.q_value < ALPHA;
    let share = |t: &TestResult| 100.0 * t.agree as f64 / t.pairs as f64;
    let (price, arrival, h2h) = (&r.price, &r.arrival, &r.head_to_head);
    if price.pairs < MIN_PAIRS && arrival.pairs < MIN_PAIRS {
        return "not enough co-located probes".into();
    }
    if r.groups < MIN_BLOCKS {
        return format!("too few blocks ({} blocks): not judged", r.groups);
    }
    let price_rule = || {
        format!(
            "priority fee decides ({}: {:.0}% of pairs follow it)",
            if price.tau > 0.9 { "strict" } else { "loose" },
            share(price)
        )
    };
    let arrival_rule = || format!("arrival order decides ({:.0}% of pairs follow it)", share(arrival));
    match (sig(price) && price.tau > 0.0, sig(arrival) && arrival.tau > 0.0) {
        (true, false) => price_rule(),
        (false, true) => arrival_rule(),
        // Both correlate: only the pairs where the rules disagree can say which one is real.
        (true, true) if sig(h2h) && h2h.tau > 0.0 => {
            format!("{}; price beats arrival in {:.0}% of head-to-head pairs", price_rule(), share(h2h))
        }
        (true, true) if sig(h2h) => {
            format!("{}; arrival beats price in {:.0}% of head-to-head pairs", arrival_rule(), 100.0 - share(h2h))
        }
        (true, true) => "both price and arrival correlate; head-to-head pairs do not separate them".into(),
        (false, false) if sig(price) || sig(arrival) => "significant but inverted relation — see tau signs".into(),
        (false, false) => format!("no rule detected (this cell could detect tau of ±{:.2} or stronger)", r.detectable_tau),
    }
}

pub fn analyze(data_dir: &Path, perms: usize) -> Result<Vec<Summary>> {
    let sends: Vec<SendRecord> = read_jsonl(&data_dir.join("sends.jsonl"))?;
    let batches: Vec<BatchRecord> = read_jsonl(&data_dir.join("batches.jsonl"))?;
    let landed: Vec<Landed> = read_jsonl(&data_dir.join("landed.jsonl"))?;
    let landed_by_sig: HashMap<&str, &Landed> = landed.iter().map(|l| (l.signature.as_str(), l)).collect();
    let live_sends: Vec<&SendRecord> = sends.iter().filter(|s| s.live && s.error.is_none()).collect();

    // Per batch: groups of probes that share a block.
    let mut per_batch: BTreeMap<&str, Vec<(&SendRecord, &Landed)>> = BTreeMap::new();
    for s in &live_sends {
        if let Some(l) = landed_by_sig.get(s.signature.as_str()) {
            per_batch.entry(s.batch_id.as_str()).or_default().push((s, l));
        }
    }

    // Per batch: the targeted leader's blocks, one group per block (single probes included, they
    // still count as landed). Batch order, then slot order: the permutation stream depends on it.
    let mut groups_of_batch: HashMap<&str, Vec<Group>> = HashMap::new();
    for (bid, items) in &per_batch {
        let mut by_slot: BTreeMap<u64, Group> = BTreeMap::new();
        for (s, l) in items {
            // The targeted leader's own block (its window can be longer than 4 slots when the
            // schedule repeats it); anything else was included by another leader.
            if l.leader != s.identity {
                continue;
            }
            by_slot
                .entry(l.slot)
                .or_insert_with(|| Group {
                    run_id: s.run_id.clone(),
                    offset: l.slot.saturating_sub(s.window_start),
                    block_tx_count: l.block_tx_count,
                    probes: vec![],
                })
                .probes
                .push(Probe { seq: s.send_seq, price: s.cu_price_micro, index: l.index });
        }
        groups_of_batch.insert(*bid, by_slot.into_values().collect());
    }

    // Aggregation keys: each validator, each client, each client+mode.
    let mut keys: BTreeMap<String, (String, Vec<&str>)> = BTreeMap::new();
    for b in batches.iter().filter(|b| b.live && b.skipped_reason.is_none()) {
        let mode = match b.mode {
            Mode::Burst => "burst",
            Mode::Stagger => "stagger",
            Mode::Conflict => "conflict",
        };
        for k in [
            format!("validator {} ({})", b.identity, b.client_id),
            format!("client {}", b.client_id),
            format!("client {} / {mode}", b.client_id),
        ] {
            keys.entry(k).or_insert_with(|| (b.client_id.clone(), vec![])).1.push(b.batch_id.as_str());
        }
    }

    let cells: Vec<Cell> = keys
        .into_iter()
        .map(|(key, (client_id, batch_ids))| Cell {
            sent: batch_ids.iter().map(|b| live_sends.iter().filter(|s| s.batch_id == *b).count()).sum(),
            landed: batch_ids.iter().map(|b| per_batch.get(b).map_or(0, |v| v.len())).sum(),
            groups: batch_ids.iter().flat_map(|b| groups_of_batch.get(b).cloned().unwrap_or_default()).collect(),
            batches: batch_ids.len(),
            key,
            client_id,
        })
        .collect();

    // The main table keeps its seed and its order of tests, so its p-values do not move when
    // the checks below are added; every check draws from a stream of its own.
    let mut boot = StdRng::seed_from_u64(20261106);
    let out = judge(&cells, |_| true, perms, &mut StdRng::seed_from_u64(20261105), &mut boot);
    // Checks run on clients and client + mode; per validator the cells are too small to split.
    let scoped: Vec<Cell> = cells.iter().filter(|c| Family::of(&c.key) != Family::Validator).cloned().collect();
    let in_window = judge(&scoped, |g| g.offset < WINDOW_SLOTS, perms, &mut StdRng::seed_from_u64(20261107), &mut boot);
    let first_slot = judge(&scoped, |g| g.offset == 0, perms, &mut StdRng::seed_from_u64(20261108), &mut boot);
    let later_slots = judge(&scoped, |g| g.offset > 0, perms, &mut StdRng::seed_from_u64(20261109), &mut boot);
    let runs = per_run(&cells);
    let block_rows = competition(data_dir, &live_sends, &landed_by_sig)?;

    let path_rows = paths(&sends, &landed_by_sig);
    std::fs::write(
        data_dir.join("results.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "ordering": out,
            "paths": path_rows,
            "in_window_only": in_window,
            "first_slot_of_window": first_slot,
            "later_slots": later_slots,
            "per_run": runs,
            "block_competition": block_rows,
        }))?,
    )?;
    std::fs::write(
        data_dir.join("RESULTS.md"),
        render(&out)
            + &render_paths(&path_rows)
            + &render_in_window(&in_window, &out)
            + &render_offsets(&first_slot, &later_slots)
            + &render_runs(&runs)
            + &render_competition(&block_rows),
    )?;
    Ok(out)
}

/// One scope (a validator, a client, or a client and mode) before any test.
#[derive(Clone)]
struct Cell {
    key: String,
    client_id: String,
    batches: usize,
    sent: usize,
    landed: usize,
    groups: Vec<Group>,
}

/// Test every cell on the blocks that pass `keep`, correct within each family, give verdicts.
fn judge(cells: &[Cell], keep: impl Fn(&Group) -> bool, perms: usize, rng: &mut StdRng, boot: &mut StdRng) -> Vec<Summary> {
    let mut out = Vec::new();
    for c in cells {
        let kept: Vec<&Group> = c.groups.iter().filter(|g| keep(g)).collect();
        let mut positions: Vec<f64> = kept
            .iter()
            .filter(|g| g.block_tx_count > 1)
            .flat_map(|g| g.probes.iter().map(move |p| p.index as f64 / (g.block_tx_count - 1) as f64))
            .collect();
        let groups: Vec<Vec<Probe>> = kept.iter().filter(|g| g.probes.len() >= 2).map(|g| g.probes.clone()).collect();
        let mut price = test(&groups, Criterion::Price, perms, rng);
        let mut arrival = test(&groups, Criterion::Arrival, perms, rng);
        let mut head_to_head = test(&groups, Criterion::HeadToHead, perms, rng);
        for (t, crit) in [(&mut price, Criterion::Price), (&mut arrival, Criterion::Arrival), (&mut head_to_head, Criterion::HeadToHead)] {
            t.tau_ci = bootstrap_ci(&groups, crit, boot);
        }
        positions.sort_by(|a, b| a.partial_cmp(b).unwrap());
        out.push(Summary {
            verdict: String::new(),
            key: c.key.clone(),
            client_id: c.client_id.clone(),
            batches: c.batches,
            sent: c.sent,
            landed: c.landed,
            landed_in_window: kept.iter().map(|g| g.probes.len()).sum(),
            groups: groups.len(),
            detectable_tau: price.detectable_tau.max(arrival.detectable_tau),
            price,
            arrival,
            head_to_head,
            median_position: (!positions.is_empty()).then(|| positions[positions.len() / 2]),
        });
    }
    // One correction family per question: per validator, per client, per client and mode.
    for family in [Family::Validator, Family::Client, Family::ClientMode] {
        fill_q_values(
            &mut out
                .iter_mut()
                .filter(|r| Family::of(&r.key) == family)
                .flat_map(|r| [&mut r.price, &mut r.arrival, &mut r.head_to_head])
                .collect::<Vec<_>>(),
        );
    }
    for r in &mut out {
        r.verdict = verdict(r);
    }
    out
}

/// Price and arrival tau of every client and mode, run by run. No tests: one run of one cell is
/// a handful of blocks, the point is whether the sign and rough size repeat.
fn per_run(cells: &[Cell]) -> Vec<RunRow> {
    let runs: BTreeSet<&str> = cells.iter().flat_map(|c| c.groups.iter().map(|g| g.run_id.as_str())).collect();
    cells
        .iter()
        .filter(|c| Family::of(&c.key) == Family::ClientMode)
        .map(|c| RunRow {
            key: c.key.clone(),
            runs: runs
                .iter()
                .map(|r| {
                    let groups: Vec<Vec<Probe>> = c
                        .groups
                        .iter()
                        .filter(|g| g.run_id == *r && g.probes.len() >= 2)
                        .map(|g| g.probes.clone())
                        .collect();
                    let (_, pairs, price_tau) = score(&groups, Criterion::Price);
                    let (_, _, arrival_tau) = score(&groups, Criterion::Arrival);
                    RunCell { run_id: r.to_string(), blocks: groups.len(), pairs, price_tau, arrival_tau }
                })
                .collect(),
        })
        .collect()
}

/// Fee above the base fee per consumed compute unit. An approximation: the price is charged on
/// the requested CU limit, which the block does not record, and a transaction with several
/// signatures pays several base fees. Transactions that consumed nothing have no rate.
fn fee_per_cu(fee: u64, cu: u64) -> Option<f64> {
    (cu > 0).then(|| fee.saturating_sub(BASE_FEE) as f64 / cu as f64)
}

/// Kendall-type tau of a sequence in block order: +1 = every earlier element is larger.
/// Ties are skipped, like equal prices in the probe tests.
fn order_tau(values: &[f64]) -> f64 {
    let (mut agree, mut compared) = (0usize, 0usize);
    for i in 0..values.len() {
        for j in i + 1..values.len() {
            if values[i] != values[j] {
                compared += 1;
                agree += (values[i] > values[j]) as usize;
            }
        }
    }
    tau_of(agree, compared)
}

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// Secondary evidence, not from our own transactions: in every targeted-leader block that holds
/// one of our probes, is the rest of the block ordered by fee per CU, and how many transactions
/// that paid more than a probe ended up below it?
fn competition(data_dir: &Path, live_sends: &[&SendRecord], landed: &HashMap<&str, &Landed>) -> Result<Vec<CompetitionRow>> {
    let ours: HashSet<&str> = live_sends.iter().map(|s| s.signature.as_str()).collect();
    let mut slots: BTreeMap<&str, BTreeSet<u64>> = BTreeMap::new();
    for s in live_sends {
        if let Some(l) = landed.get(s.signature.as_str()) {
            if l.leader == s.identity {
                slots.entry(s.client_id.as_str()).or_default().insert(l.slot);
            }
        }
    }
    let mut out = Vec::new();
    for (client, slots) in slots {
        let (mut taus, mut sizes, mut positions, mut outpaid) = (vec![], vec![], vec![], vec![]);
        for slot in slots {
            let path = data_dir.join("blocks").join(format!("{slot}.json"));
            if !path.exists() {
                continue;
            }
            let block: BlockFile = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
            let others: Vec<(usize, f64)> = block
                .rows
                .iter()
                .filter(|r| !ours.contains(r.signature.as_str()))
                .filter_map(|r| fee_per_cu(r.fee, r.cu_consumed).map(|f| (r.index, f)))
                .collect();
            let values: Vec<f64> = others.iter().map(|o| o.1).collect();
            taus.push(order_tau(&values));
            sizes.push(block.tx_count as f64);
            for r in block.rows.iter().filter(|r| ours.contains(r.signature.as_str())) {
                if block.tx_count > 1 {
                    positions.push(r.index as f64 / (block.tx_count - 1) as f64);
                }
                let Some(own) = fee_per_cu(r.fee, r.cu_consumed) else { continue };
                let richer: Vec<usize> = others.iter().filter(|o| o.1 > own).map(|o| o.0).collect();
                if !richer.is_empty() {
                    outpaid.push(richer.iter().filter(|&&i| i > r.index).count() as f64 / richer.len() as f64);
                }
            }
        }
        taus.retain(|t| t.is_finite());
        if taus.is_empty() {
            continue;
        }
        taus.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let quartile = |q: f64| taus[(q * (taus.len() - 1) as f64).round() as usize];
        out.push(CompetitionRow {
            client_id: client.to_string(),
            blocks: taus.len(),
            median_block_txs: median(&mut sizes) as usize,
            median_fee_tau: quartile(0.5),
            fee_tau_iqr: [quartile(0.25), quartile(0.75)],
            probe_median_position: median(&mut positions),
            probes: positions.len(),
            outpaid_below_probe: median(&mut outpaid),
        });
    }
    Ok(out)
}

fn fmt_ci(t: &TestResult) -> String {
    t.tau_ci.map(|[lo, hi]| format!(" [{lo:+.2}, {hi:+.2}]")).unwrap_or_default()
}

fn fmt_tau(t: &TestResult) -> String {
    if t.pairs == 0 {
        "—".into()
    } else {
        format!("{:+.2}{} (p={:.4}, q={:.3}, n={})", t.tau, fmt_ci(t), t.p_value, t.q_value, t.pairs)
    }
}

/// The short form used by the check tables.
fn fmt_tau_short(t: &TestResult) -> String {
    if t.pairs == 0 {
        "—".into()
    } else {
        format!("{:+.2} (q={:.3}, n={})", t.tau, t.q_value, t.pairs)
    }
}

fn fmt_opt(v: f64) -> String {
    if v.is_finite() {
        format!("{v:.2}")
    } else {
        "—".into()
    }
}

fn render_in_window(rows: &[Summary], all: &[Summary]) -> String {
    let before: HashMap<&str, &str> = all.iter().map(|r| (r.key.as_str(), r.verdict.as_str())).collect();
    let mut s = String::new();
    let _ = writeln!(s, "\n## Check: only probes inside the planned window\n");
    let _ = writeln!(
        s,
        "The main table counts every probe that landed in a block of the targeted leader, including the few that \
         missed their window and landed in a later window of the same leader. Here only landings at most {} slots \
         after the window start are kept (one {WINDOW_SLOTS}-slot leader window). Tests and corrections are redone \
         on this subset.\n",
        WINDOW_SLOTS - 1
    );
    let _ = writeln!(s, "| scope | landed in window | blocks (≥2 probes) | price tau | arrival tau | head-to-head | verdict | verdict on all landings |");
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|");
    for r in rows {
        let old = before.get(r.key.as_str()).copied().unwrap_or("—");
        // Shares and the detectable effect move with every subset; only the kind of verdict is compared.
        let kind = |v: &str| v.split(" (").next().unwrap_or("").to_string();
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {} | {} | {} | {} |",
            r.key,
            r.landed_in_window,
            r.groups,
            fmt_tau_short(&r.price),
            fmt_tau_short(&r.arrival),
            fmt_tau_short(&r.head_to_head),
            r.verdict,
            if kind(old) == kind(&r.verdict) { "same".to_string() } else { old.to_string() }
        );
    }
    s
}

fn render_offsets(first: &[Summary], later: &[Summary]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "\n## Check: first slot of the window versus later slots\n");
    let _ = writeln!(
        s,
        "Probes fired one slot before the window can be waiting at the leader when its first slot starts; probes \
         that land later arrive while the leader is already producing. If the ordering rule depends on what is \
         waiting at the same moment, the two subsets should differ. Each subset is tested and corrected on its own.\n"
    );
    let _ = writeln!(
        s,
        "| scope | slot 0: blocks | slot 0: price tau | slot 0: arrival tau | slot 0: verdict | later: blocks | later: price tau | later: arrival tau | later: verdict |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|---|");
    for (a, b) in first.iter().zip(later) {
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            a.key,
            a.groups,
            fmt_tau_short(&a.price),
            fmt_tau_short(&a.arrival),
            a.verdict,
            b.groups,
            fmt_tau_short(&b.price),
            fmt_tau_short(&b.arrival),
            b.verdict
        );
    }
    s
}

fn render_runs(rows: &[RunRow]) -> String {
    let mut s = String::new();
    let Some(first) = rows.first() else { return s };
    let _ = writeln!(s, "\n## Check: does each run say the same?\n");
    let _ = writeln!(
        s,
        "Price tau / arrival tau of every client and mode, separately for each live run (n = pairs, b = blocks with \
         at least two probes). No tests here: a single run of one cell is a few blocks; what matters is whether the \
         sign repeats.\n"
    );
    let _ = writeln!(s, "| scope | {} |", first.runs.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>().join(" | "));
    let _ = writeln!(s, "|---|{}", "---|".repeat(first.runs.len()));
    for r in rows {
        let cells: Vec<String> = r
            .runs
            .iter()
            .map(|c| {
                if c.pairs == 0 {
                    "—".into()
                } else {
                    format!("{:+.2} / {:+.2} (n={}, b={})", c.price_tau, c.arrival_tau, c.pairs, c.blocks)
                }
            })
            .collect();
        let _ = writeln!(s, "| {} | {} |", r.key, cells.join(" | "));
    }
    s
}

fn render_competition(rows: &[CompetitionRow]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "\n## Secondary evidence: the rest of the block\n");
    let _ = writeln!(
        s,
        "Not from our own transactions, so it supports the main result but cannot replace it. For every block of a \
         targeted leader that holds one of our probes, the other transactions are compared pair by pair: fee per CU \
         tau +1 would mean the block is sorted by fee per compute unit, highest first. Fee per CU is the fee above \
         the {BASE_FEE}-lamport base fee divided by consumed CU — an approximation: the price is charged on the \
         requested CU limit, which a block does not record, multi-signature transactions pay several base fees, and \
         Jito bundle tips are paid by transfer, not by fee. \"Outpaid, placed below\": of the transactions that paid \
         more per CU than one of our probes, the share that sits after it in the block (0 if the block were sorted by \
         fee per CU).\n"
    );
    let _ = writeln!(
        s,
        "| client | blocks | median transactions per block | fee per CU tau, median [IQR] | our probes: median position | outpaid, placed below (median over probes) |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|");
    for r in rows {
        let _ = writeln!(
            s,
            "| {} | {} | {} | {:+.2} [{:+.2}, {:+.2}] | {} (n={}) | {} |",
            r.client_id,
            r.blocks,
            r.median_block_txs,
            r.median_fee_tau,
            r.fee_tau_iqr[0],
            r.fee_tau_iqr[1],
            fmt_opt(r.probe_median_position),
            r.probes,
            fmt_opt(r.outpaid_below_probe)
        );
    }
    s
}

fn render_paths(rows: &[PathRow]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "\n## Where the probes went\n");
    let _ = writeln!(
        s,
        "Every live probe, by the client of the validator it was aimed at. Only \"targeted leader's block\" is \
         ordering evidence; the other columns describe the path.\n"
    );
    let _ = writeln!(
        s,
        "| client | probes sent | refused at send (QUIC) | targeted leader's block | another leader's block | never landed | median slots after window start (another leader) |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|---|");
    for r in rows {
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {} | {} | {} |",
            r.client_id,
            r.sent,
            r.rejected_at_send,
            r.own_block,
            r.other_leader,
            r.not_landed,
            r.other_leader_median_offset.map(|m| m.to_string()).unwrap_or_else(|| "—".into())
        );
    }
    s
}

pub fn render(rows: &[Summary]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# Results (generated by `itb analyze`)\n");
    let _ = writeln!(
        s,
        "tau: +1 = the earlier probe in the block always had the higher price (price) / was written first \
         (arrival); 0 = unrelated. Head-to-head: only pairs where the higher-priced probe was written later; \
         +1 = price always won, -1 = arrival always won. p: permutation test within blocks; q: Benjamini–Hochberg \
         within each level of this table (validators, clients, client + mode); a rule is claimed only at q < {ALPHA} with at least {MIN_PAIRS} pairs \
         from at least {MIN_BLOCKS} blocks — fewer blocks are shown but not judged. [lo, hi]: 95% interval of tau from a \
         block-level bootstrap ({BOOTSTRAP} resamples of whole blocks). Detectable |tau|: the smallest effect the cell \
         would find with 80% power at p < 0.05, from its own permutation null (95th percentile of |tau| plus 0.84 null \
         standard deviations; normal approximation, before the multiple-testing correction) — \"no rule detected\" means \
         no rule at least that strong, not that the order is random. \
         Landed: all located probes (in the targeted leader's own blocks — the only ones used). \
         Position: 0 = top of block, 1 = bottom.\n"
    );
    let _ = writeln!(
        s,
        "| scope | batches | sent | landed (leader's blocks) | blocks with ≥2 | price tau | arrival tau | head-to-head | detectable \\|tau\\| | median position | verdict |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|---|---|---|");
    for r in rows {
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} ({}) | {} | {} | {} | {} | {} | {} | {} |",
            r.key,
            r.batches,
            r.sent,
            r.landed,
            r.landed_in_window,
            r.groups,
            fmt_tau(&r.price),
            fmt_tau(&r.arrival),
            fmt_tau(&r.head_to_head),
            fmt_opt(r.detectable_tau),
            r.median_position.map(|m| format!("{m:.2}")).unwrap_or_else(|| "—".into()),
            r.verdict
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(v: &[(usize, u64, usize)]) -> Vec<Probe> {
        v.iter().map(|&(seq, price, index)| Probe { seq, price, index }).collect()
    }

    #[test]
    fn perfect_price_order_scores_plus_one() {
        // Block index follows descending price, send order is the opposite.
        let groups = vec![g(&[(0, 10, 3), (1, 20, 2), (2, 30, 1), (3, 40, 0)])];
        let (_, n, tau_price) = score(&groups, Criterion::Price);
        let (_, _, tau_send) = score(&groups, Criterion::Arrival);
        let (_, n_h2h, tau_h2h) = score(&groups, Criterion::HeadToHead);
        assert_eq!(n, 6);
        assert!((tau_price - 1.0).abs() < 1e-9);
        assert!((tau_send + 1.0).abs() < 1e-9);
        // Every pair has price and send order disagreeing; price won all of them.
        assert_eq!(n_h2h, 6);
        assert!((tau_h2h - 1.0).abs() < 1e-9);
    }

    #[test]
    fn head_to_head_ignores_pairs_where_rules_agree() {
        // Price and send order coincide, so no pair can separate the two rules.
        let groups = vec![g(&[(0, 40, 0), (1, 30, 1), (2, 20, 2), (3, 10, 3)])];
        let (_, n, _) = score(&groups, Criterion::HeadToHead);
        assert_eq!(n, 0);
    }

    #[test]
    fn benjamini_hochberg_matches_hand_computation() {
        let mk = |p: f64| TestResult { pairs: 10, agree: 0, tau: 0.0, p_value: p, q_value: f64::NAN, tau_ci: None, detectable_tau: f64::NAN };
        let mut ts = vec![mk(0.01), mk(0.04), mk(0.03), mk(f64::NAN)];
        fill_q_values(&mut ts.iter_mut().collect::<Vec<_>>());
        // m = 3: sorted 0.01, 0.03, 0.04 -> 0.03, 0.04, 0.04.
        assert!((ts[0].q_value - 0.03).abs() < 1e-12);
        assert!((ts[2].q_value - 0.04).abs() < 1e-12);
        assert!((ts[1].q_value - 0.04).abs() < 1e-12);
        assert!(ts[3].q_value.is_nan());
    }

    #[test]
    fn bootstrap_is_deterministic_and_brackets_the_estimate() {
        let groups: Vec<Vec<Probe>> = (0..12)
            .map(|k| if k % 3 == 0 { g(&[(0, 1, 0), (1, 2, 1), (2, 3, 2)]) } else { g(&[(0, 1, 2), (1, 2, 1), (2, 3, 0)]) })
            .collect();
        let (_, _, tau) = score(&groups, Criterion::Price);
        let a = bootstrap_ci(&groups, Criterion::Price, &mut StdRng::seed_from_u64(7)).unwrap();
        let b = bootstrap_ci(&groups, Criterion::Price, &mut StdRng::seed_from_u64(7)).unwrap();
        assert_eq!(a, b);
        assert!(a[0] <= tau && tau <= a[1], "{a:?} {tau}");
        assert!(a[0] < a[1]);
        // A single block cannot be resampled into anything but itself.
        let one = bootstrap_ci(&groups[..1], Criterion::Price, &mut StdRng::seed_from_u64(7)).unwrap();
        assert_eq!(one[0], one[1]);
    }

    #[test]
    fn detectable_effect_shrinks_with_more_blocks() {
        let block = || g(&[(0, 1, 0), (1, 2, 1), (2, 3, 2), (3, 4, 3)]);
        let mde = |n: usize| {
            let groups: Vec<Vec<Probe>> = (0..n).map(|_| block()).collect();
            test(&groups, Criterion::Arrival, 2_000, &mut StdRng::seed_from_u64(3)).detectable_tau
        };
        let (few, many) = (mde(3), mde(30));
        assert!(many < few, "{few} {many}");
        assert!(many > 0.0 && few <= 1.0);
        // 30 blocks of 4 probes: the null sd of tau is about 0.08, so the threshold is near 0.2–0.3.
        assert!((0.15..0.4).contains(&many), "{many}");
    }

    #[test]
    fn too_few_blocks_are_not_judged() {
        let mk = |tau: f64, q: f64| TestResult { pairs: 12, agree: 12, tau, p_value: q, q_value: q, tau_ci: None, detectable_tau: 0.5 };
        let mut r = Summary { groups: 2, price: mk(1.0, 0.01), arrival: mk(0.0, 1.0), head_to_head: mk(0.0, 1.0), ..Default::default() };
        assert!(verdict(&r).starts_with("too few blocks (2 blocks)"));
        r.groups = 4;
        assert!(verdict(&r).starts_with("priority fee decides (strict"));
    }

    #[test]
    fn order_tau_reads_a_fee_sorted_block_as_plus_one() {
        assert!((order_tau(&[9.0, 5.0, 5.0, 1.0]) - 1.0).abs() < 1e-12);
        assert!((order_tau(&[1.0, 2.0, 3.0]) + 1.0).abs() < 1e-12);
        assert!(order_tau(&[2.0, 2.0]).is_nan());
        assert_eq!(fee_per_cu(5_600, 200), Some(3.0));
        assert_eq!(fee_per_cu(5_000, 0), None);
    }

    #[test]
    fn permutation_test_finds_strong_signal_and_ignores_noise() {
        let mut rng = StdRng::seed_from_u64(1);
        let strong: Vec<Vec<Probe>> =
            (0..20).map(|_| g(&[(0, 1, 7), (1, 2, 6), (2, 3, 5), (3, 4, 4), (4, 5, 3), (5, 6, 2), (6, 7, 1), (7, 8, 0)])).collect();
        let t = test(&strong, Criterion::Price, 999, &mut rng);
        assert!(t.p_value < 0.01, "{t:?}");
        // Price unrelated to position (alternating pattern gives tau 0 on pairs).
        let flat = vec![g(&[(0, 1, 0), (1, 2, 1)]), g(&[(0, 1, 1), (1, 2, 0)])];
        let t = test(&flat, Criterion::Price, 999, &mut rng);
        assert!(t.tau.abs() < 1e-9);
        assert!(t.p_value > 0.5, "{t:?}");
    }
}
