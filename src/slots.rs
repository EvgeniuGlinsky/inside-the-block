//! Where the cluster is right now.
//!
//! The websocket `slotSubscribe` stream is the low-latency source; a getSlot poll keeps the
//! tracker alive if the socket drops. Both only tell us what *our RPC node* has seen, which
//! lags the real leader by a slot or two — the run records that observed slot next to every
//! send, so the lag is visible in the raw data instead of being assumed away.

use crate::rpc::Rpc;
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone)]
pub struct SlotTracker {
    slot: Arc<AtomicU64>,
    seen_at_ns: Arc<AtomicU64>,
}

pub fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

impl SlotTracker {
    pub fn current(&self) -> u64 {
        self.slot.load(Ordering::SeqCst)
    }

    /// How long ago the current slot was first seen, in milliseconds.
    pub fn age_ms(&self) -> u64 {
        now_ns().saturating_sub(self.seen_at_ns.load(Ordering::SeqCst)) / 1_000_000
    }

    fn observe(&self, slot: u64) {
        if slot > self.slot.load(Ordering::SeqCst) {
            self.slot.store(slot, Ordering::SeqCst);
            self.seen_at_ns.store(now_ns(), Ordering::SeqCst);
        }
    }

    pub async fn start(rpc: Rpc, ws_url: String) -> Self {
        let t = SlotTracker { slot: Arc::new(AtomicU64::new(0)), seen_at_ns: Arc::new(AtomicU64::new(0)) };
        if let Ok(s) = rpc.slot("processed").await {
            t.observe(s);
        }
        let ws = t.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = ws_loop(&ws, &ws_url).await {
                    eprintln!("slot websocket: {e}; reconnecting");
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        let poll = t.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(400)).await;
                // Only poll when the socket has gone quiet; post-Alpenglow slots are ~0.2 s.
                if poll.age_ms() > 1500 {
                    if let Ok(s) = rpc.slot("processed").await {
                        poll.observe(s);
                    }
                }
            }
        });
        t
    }

    /// Wait until the observed slot reaches `target`, or give up after `timeout`.
    pub async fn wait_for(&self, target: u64, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        while self.current() < target {
            if tokio::time::Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        true
    }
}

async fn ws_loop(t: &SlotTracker, url: &str) -> anyhow::Result<()> {
    let (mut sock, _) = tokio_tungstenite::connect_async(url).await?;
    sock.send(Message::Text(r#"{"jsonrpc":"2.0","id":1,"method":"slotSubscribe"}"#.into())).await?;
    while let Some(msg) = sock.next().await {
        let msg = msg?;
        if let Message::Text(txt) = msg {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                if let Some(s) = v["params"]["result"]["slot"].as_u64() {
                    t.observe(s);
                }
            }
        } else if let Message::Ping(p) = msg {
            sock.send(Message::Pong(p)).await?;
        }
    }
    Ok(())
}
