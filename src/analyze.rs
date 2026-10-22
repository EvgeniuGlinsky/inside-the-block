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

use crate::collect::{read_jsonl, Landed};
use crate::run::{BatchRecord, Mode, SendRecord};
use anyhow::Result;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::Path;

#[derive(Clone)]
struct Probe {
    seq: usize,
    price: u64,
    index: usize,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct TestResult {
    pub pairs: usize,
    pub agree: usize,
    pub tau: f64,
    pub p_value: f64,
    /// Benjamini–Hochberg q-value across every test of the analysis.
    pub q_value: f64,
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
    pub median_position: Option<f64>,
    pub verdict: String,
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

/// tau-like score of one ordering criterion over all groups: (agree - disagree) / compared.
fn score(groups: &[Vec<Probe>], crit: Criterion) -> (usize, usize, f64) {
    let (mut agree, mut compared) = (0usize, 0usize);
    for g in groups {
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
    }
    let tau = if compared == 0 { f64::NAN } else { (2.0 * agree as f64 - compared as f64) / compared as f64 };
    (agree, compared, tau)
}

fn test(groups: &[Vec<Probe>], crit: Criterion, perms: usize, rng: &mut StdRng) -> TestResult {
    let (agree, pairs, tau) = score(groups, crit);
    if pairs == 0 {
        return TestResult { pairs, agree, tau, p_value: f64::NAN, q_value: f64::NAN };
    }
    let mut extreme = 0usize;
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
    }
    TestResult { pairs, agree, tau, p_value: (extreme as f64 + 1.0) / (perms as f64 + 1.0), q_value: f64::NAN }
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
const ALPHA: f64 = 0.05;

fn verdict(r: &Summary) -> String {
    let sig = |t: &TestResult| t.pairs >= MIN_PAIRS && t.q_value < ALPHA;
    let share = |t: &TestResult| 100.0 * t.agree as f64 / t.pairs as f64;
    let (price, arrival, h2h) = (&r.price, &r.arrival, &r.head_to_head);
    if price.pairs < MIN_PAIRS && arrival.pairs < MIN_PAIRS {
        return "not enough co-located probes".into();
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
        (false, false) => "no rule detected: ordering indistinguishable from random".into(),
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

    let mut rng = StdRng::seed_from_u64(20261105);
    let mut out = Vec::new();
    for (key, (client, batch_ids)) in keys {
        let mut groups: Vec<Vec<Probe>> = Vec::new();
        let (mut sent, mut got, mut in_window) = (0, 0, 0);
        let mut positions: Vec<f64> = Vec::new();
        for bid in &batch_ids {
            sent += live_sends.iter().filter(|s| s.batch_id == *bid).count();
            let Some(items) = per_batch.get(bid) else { continue };
            got += items.len();
            let mut by_slot: BTreeMap<u64, Vec<Probe>> = BTreeMap::new();
            for (s, l) in items {
                // The targeted leader's own block (its window can be longer than 4 slots when the
                // schedule repeats it); anything else was included by another leader.
                if l.leader != s.identity {
                    continue;
                }
                in_window += 1;
                if l.block_tx_count > 1 {
                    positions.push(l.index as f64 / (l.block_tx_count - 1) as f64);
                }
                by_slot.entry(l.slot).or_default().push(Probe { seq: s.send_seq, price: s.cu_price_micro, index: l.index });
            }
            groups.extend(by_slot.into_values().filter(|g| g.len() >= 2));
        }
        let price = test(&groups, Criterion::Price, perms, &mut rng);
        let arrival = test(&groups, Criterion::Arrival, perms, &mut rng);
        let head_to_head = test(&groups, Criterion::HeadToHead, perms, &mut rng);
        positions.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median_position = (!positions.is_empty()).then(|| positions[positions.len() / 2]);
        out.push(Summary {
            verdict: String::new(),
            key,
            client_id: client,
            batches: batch_ids.len(),
            sent,
            landed: got,
            landed_in_window: in_window,
            groups: groups.len(),
            price,
            arrival,
            head_to_head,
            median_position,
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

    let path_rows = paths(&sends, &landed_by_sig);
    std::fs::write(
        data_dir.join("results.json"),
        serde_json::to_string_pretty(&serde_json::json!({"ordering": out, "paths": path_rows}))?,
    )?;
    std::fs::write(data_dir.join("RESULTS.md"), render(&out) + &render_paths(&path_rows))?;
    Ok(out)
}

fn fmt_tau(t: &TestResult) -> String {
    if t.pairs == 0 {
        "—".into()
    } else {
        format!("{:+.2} (p={:.4}, q={:.3}, n={})", t.tau, t.p_value, t.q_value, t.pairs)
    }
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
         within each level of this table (validators, clients, client + mode); a rule is claimed only at q < {ALPHA} with at least {MIN_PAIRS} pairs. \
         Landed: all located probes (in the targeted leader's own blocks — the only ones used). \
         Position: 0 = top of block, 1 = bottom.\n"
    );
    let _ = writeln!(
        s,
        "| scope | batches | sent | landed (leader's blocks) | blocks with ≥2 | price tau | arrival tau | head-to-head | median position | verdict |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|---|---|");
    for r in rows {
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} ({}) | {} | {} | {} | {} | {} | {} |",
            r.key,
            r.batches,
            r.sent,
            r.landed,
            r.landed_in_window,
            r.groups,
            fmt_tau(&r.price),
            fmt_tau(&r.arrival),
            fmt_tau(&r.head_to_head),
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
        let mk = |p: f64| TestResult { pairs: 10, agree: 0, tau: 0.0, p_value: p, q_value: f64::NAN };
        let mut ts = vec![mk(0.01), mk(0.04), mk(0.03), mk(f64::NAN)];
        fill_q_values(&mut ts.iter_mut().collect::<Vec<_>>());
        // m = 3: sorted 0.01, 0.03, 0.04 -> 0.03, 0.04, 0.04.
        assert!((ts[0].q_value - 0.03).abs() < 1e-12);
        assert!((ts[2].q_value - 0.04).abs() < 1e-12);
        assert!((ts[1].q_value - 0.04).abs() < 1e-12);
        assert!(ts[3].q_value.is_nan());
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
