//! Minimal JSON-RPC client for the handful of methods the experiment needs.
//!
//! Everything goes through one `call` with retries: public mainnet RPC rate-limits
//! bursts, and a measurement run should slow down rather than abort.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
pub struct Rpc {
    url: String,
    http: reqwest::Client,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterNode {
    pub pubkey: String,
    pub version: Option<String>,
    pub client_id: Option<String>,
    pub tpu_quic: Option<String>,
    pub feature_set: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpochInfo {
    pub absolute_slot: u64,
    pub epoch: u64,
    pub slot_index: u64,
    pub slots_in_epoch: u64,
}

impl Rpc {
    pub fn new(url: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("http client");
        Self { url: url.to_string(), http }
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut last = anyhow!("no attempt made");
        // Public mainnet RPC rate-limits heavy methods such as getBlock hard; back off
        // exponentially (up to ~2 minutes in total) rather than abort a collection half way.
        for attempt in 0..10u32 {
            match self.http.post(&self.url).json(&body).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    if status.as_u16() == 429 || status.is_server_error() {
                        last = anyhow!("{method}: HTTP {status}");
                        let wait = (500u64 << attempt.min(6)).min(30_000);
                        tokio::time::sleep(Duration::from_millis(wait)).await;
                        continue;
                    } else {
                        let v: Value = serde_json::from_str(&text)
                            .with_context(|| format!("{method}: bad JSON ({} bytes)", text.len()))?;
                        if let Some(err) = v.get("error") {
                            // Some errors are final (e.g. a skipped slot); let the caller decide.
                            return Err(anyhow!("{method}: RPC error {err}"));
                        }
                        return v.get("result").cloned().ok_or_else(|| anyhow!("{method}: no result"));
                    }
                }
                Err(e) => last = anyhow!("{method}: {e}"),
            }
            tokio::time::sleep(Duration::from_millis(500 * (attempt as u64 + 1))).await;
        }
        Err(last)
    }

    pub async fn cluster_nodes(&self) -> Result<Vec<ClusterNode>> {
        Ok(serde_json::from_value(self.call("getClusterNodes", json!([])).await?)?)
    }

    pub async fn epoch_info(&self) -> Result<EpochInfo> {
        Ok(serde_json::from_value(self.call("getEpochInfo", json!([{"commitment": "processed"}])).await?)?)
    }

    /// Leader schedule of the current epoch: identity -> absolute slots.
    pub async fn leader_schedule(&self, first_slot_of_epoch: u64) -> Result<Vec<(String, Vec<u64>)>> {
        let v = self.call("getLeaderSchedule", json!([])).await?;
        let map = v.as_object().ok_or_else(|| anyhow!("getLeaderSchedule: not an object"))?;
        Ok(map
            .iter()
            .map(|(k, slots)| {
                let abs = slots
                    .as_array()
                    .map(|a| a.iter().filter_map(|s| s.as_u64()).map(|s| s + first_slot_of_epoch).collect())
                    .unwrap_or_default();
                (k.clone(), abs)
            })
            .collect())
    }

    pub async fn slot_leaders(&self, start: u64, limit: u64) -> Result<Vec<String>> {
        Ok(serde_json::from_value(self.call("getSlotLeaders", json!([start, limit])).await?)?)
    }

    pub async fn slot(&self, commitment: &str) -> Result<u64> {
        self.call("getSlot", json!([{"commitment": commitment}]))
            .await?
            .as_u64()
            .ok_or_else(|| anyhow!("getSlot: not a number"))
    }

    pub async fn latest_blockhash(&self) -> Result<(String, u64)> {
        let v = self.call("getLatestBlockhash", json!([{"commitment": "confirmed"}])).await?;
        let hash = v["value"]["blockhash"].as_str().ok_or_else(|| anyhow!("no blockhash"))?.to_string();
        let last_valid = v["value"]["lastValidBlockHeight"].as_u64().unwrap_or(0);
        Ok((hash, last_valid))
    }

    pub async fn balance(&self, pubkey: &str) -> Result<u64> {
        let v = self.call("getBalance", json!([pubkey, {"commitment": "confirmed"}])).await?;
        v["value"].as_u64().ok_or_else(|| anyhow!("getBalance: no value"))
    }

    pub async fn rent_exempt_minimum(&self, data_len: u64) -> Result<u64> {
        self.call("getMinimumBalanceForRentExemption", json!([data_len]))
            .await?
            .as_u64()
            .ok_or_else(|| anyhow!("rent: not a number"))
    }

    /// Full block with v0 and v1 transactions. `None` when the slot was skipped.
    pub async fn block(&self, slot: u64) -> Result<Option<Value>> {
        let params = json!([slot, {
            "encoding": "json",
            "maxSupportedTransactionVersion": 1,
            "transactionDetails": "full",
            "rewards": false,
            "commitment": "confirmed"
        }]);
        match self.call("getBlock", params).await {
            Ok(v) => Ok(Some(v)),
            Err(e) => {
                let s = e.to_string();
                // -32007 skipped, -32009 missing in long-term storage: the block does not exist.
                if s.contains("-32007") || s.contains("-32009") || s.contains("skipped") {
                    Ok(None)
                } else {
                    Err(e)
                }
            }
        }
    }

    pub async fn send_raw(&self, tx_base64: &str) -> Result<String> {
        let params = json!([tx_base64, {"encoding": "base64", "skipPreflight": true, "maxRetries": 0}]);
        let v = self.call("sendTransaction", params).await?;
        v.as_str().map(|s| s.to_string()).ok_or_else(|| anyhow!("sendTransaction: no signature"))
    }

    pub async fn signature_statuses(&self, sigs: &[String]) -> Result<Vec<Value>> {
        if sigs.len() > 256 {
            bail!("too many signatures in one status query");
        }
        let v = self
            .call("getSignatureStatuses", json!([sigs, {"searchTransactionHistory": true}]))
            .await?;
        Ok(v["value"].as_array().cloned().unwrap_or_default())
    }
}
