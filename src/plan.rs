//! Choose which leader windows to probe.
//!
//! Client identification uses the `clientId` that each node advertises in gossip
//! (`getClusterNodes`), saved together with the version string at planning time, so a judge
//! can check the label against the same source. Windows are whole 4-slot leader rotations
//! taken from `getLeaderSchedule` of the current epoch.

use crate::rpc::{ClusterNode, Rpc};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub identity: String,
    pub client_id: String,
    pub version: String,
    pub tpu_quic: Option<String>,
    pub stake_share: f64,
    /// First slot of each leader window we will probe, ascending.
    pub windows: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub created_unix: u64,
    pub epoch: u64,
    pub planned_at_slot: u64,
    pub windows_per_validator: usize,
    pub targets: Vec<Target>,
    /// Stake share by client at planning time, for the report.
    pub client_stake: BTreeMap<String, f64>,
}

pub struct PlanArgs {
    pub clients: Vec<String>,
    pub validators_per_client: usize,
    pub windows_per_validator: usize,
    pub warmup_slots: u64,
    pub horizon_slots: u64,
    /// Restrict the choice to these identities (e.g. repeat an earlier plan's validators).
    pub identities: Vec<String>,
}

/// Split a sorted list of slots into windows of consecutive slots; return each window's first slot.
fn window_starts(slots: &[u64]) -> Vec<u64> {
    let mut out = Vec::new();
    let mut prev: Option<u64> = None;
    for &s in slots {
        if prev.map(|p| s != p + 1).unwrap_or(true) {
            out.push(s);
        }
        prev = Some(s);
    }
    out
}

pub async fn make_plan(rpc: &Rpc, a: &PlanArgs) -> Result<Plan> {
    let nodes: HashMap<String, ClusterNode> =
        rpc.cluster_nodes().await?.into_iter().map(|n| (n.pubkey.clone(), n)).collect();
    let epoch = rpc.epoch_info().await?;
    let first = epoch.absolute_slot - epoch.slot_index;
    let schedule = rpc.leader_schedule(first).await?;

    let votes = rpc.call("getVoteAccounts", serde_json::json!([])).await?;
    let mut stake: HashMap<String, u64> = HashMap::new();
    for v in votes["current"].as_array().into_iter().flatten() {
        if let (Some(node), Some(s)) = (v["nodePubkey"].as_str(), v["activatedStake"].as_u64()) {
            *stake.entry(node.to_string()).or_default() += s;
        }
    }
    let total: u64 = stake.values().sum::<u64>().max(1);
    let mut client_stake: BTreeMap<String, f64> = BTreeMap::new();
    for (id, s) in &stake {
        let c = nodes.get(id).and_then(|n| n.client_id.clone()).unwrap_or_else(|| "not-in-gossip".into());
        *client_stake.entry(c).or_default() += *s as f64 / total as f64;
    }

    let lo = epoch.absolute_slot + a.warmup_slots;
    let hi = epoch.absolute_slot + a.horizon_slots;
    let mut candidates: Vec<Target> = Vec::new();
    for (identity, slots) in schedule {
        let Some(node) = nodes.get(&identity) else { continue };
        let Some(client) = node.client_id.clone() else { continue };
        if !a.clients.iter().any(|c| c == &client) {
            continue;
        }
        if !a.identities.is_empty() && !a.identities.contains(&identity) {
            continue;
        }
        let mut s: Vec<u64> = slots;
        s.sort_unstable();
        let windows: Vec<u64> = window_starts(&s).into_iter().filter(|w| *w >= lo && *w <= hi).collect();
        if windows.len() < a.windows_per_validator {
            continue;
        }
        candidates.push(Target {
            stake_share: *stake.get(&identity).unwrap_or(&0) as f64 / total as f64,
            identity,
            client_id: client,
            version: node.version.clone().unwrap_or_default(),
            tpu_quic: node.tpu_quic.clone(),
            windows: windows.into_iter().take(a.windows_per_validator).collect(),
        });
    }

    // Per client: prefer validators that advertise a QUIC TPU and whose windows come soonest.
    let mut targets = Vec::new();
    for client in &a.clients {
        let mut of_client: Vec<Target> = candidates.iter().filter(|t| &t.client_id == client).cloned().collect();
        of_client.sort_by_key(|t| (t.tpu_quic.is_none(), *t.windows.last().unwrap_or(&u64::MAX)));
        targets.extend(of_client.into_iter().take(a.validators_per_client));
    }

    let clients_covered = targets.iter().map(|t| t.client_id.as_str()).collect::<std::collections::BTreeSet<_>>();
    if targets.len() < 6 || clients_covered.len() < 3 {
        bail!(
            "plan covers {} validators across {} clients; the bounty needs at least 6 across 3. \
             Widen --horizon-slots or --clients.",
            targets.len(),
            clients_covered.len()
        );
    }

    Ok(Plan {
        created_unix: crate::slots::now_ns() / 1_000_000_000,
        epoch: epoch.epoch,
        planned_at_slot: epoch.absolute_slot,
        windows_per_validator: a.windows_per_validator,
        targets,
        client_stake,
    })
}

#[cfg(test)]
mod tests {
    use super::window_starts;

    #[test]
    fn windows_split_on_gaps() {
        assert_eq!(window_starts(&[10, 11, 12, 13, 40, 41, 42, 43]), vec![10, 40]);
        assert_eq!(window_starts(&[5]), vec![5]);
        assert!(window_starts(&[]).is_empty());
    }
}
