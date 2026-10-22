# Report — how Solana leaders ordered our transactions (22 Oct 2026)

Every number below is produced by `itb analyze` from the raw records in `data/` (see `data/RESULTS.md`, which
`docker compose run --rm reanalyze` regenerates byte for byte). All measurements were taken on mainnet, in epoch
1065, after the Alpenglow upgrade.

## Measurement runs

| run | date and time (UTC) | slots of the probed windows | batches planned / sent / skipped | probes sent | probes located on chain |
|---|---|---|---|---|---|
| `dry-1792702925` (dry run, nothing sent) | 2026-10-22 21:02 | 460 180 720 – 460 182 972 | 36 / 34 / 2 (8 probes each) | 0 (272 built and signed) | — |
| `live-1792705232` | 2026-10-22 21:40 – 22:14 (first / last landing 21:41:25 / 22:13:31) | 460 191 532 – 460 200 704 | 72 / 66 / 6 | 264 | 208 |
| `live-1792707349` | 2026-10-22 22:15 – 23:05 (first / last landing 22:16:42 / 23:04:00) | 460 201 600 – 460 214 960 | 96 / 92 / 1, 3 not reached (run stopped, see below) | 368 | 280 |

Both live runs probed the same 12 validators (`data/plan-live1.json`, `data/plan-live2.json`). Live batches had
4 probes (30 000 CU limit each, prices a random 4 of the 8 levels 0 … 100 000 micro-lamports/CU, send order an
independent permutation); modes rotated burst → stagger → conflict over each validator's windows.

**Spend.** The wallet went from 0.009 000 SOL to 0.006 135 SOL: **0.002 865 SOL** for both live runs together —
fees of the 488 probes that landed (0.002 785 SOL) plus the funding and sweep transfers. Probes that never land
cost nothing. The cap (`--max-spend-sol 0.05`) reserves the worst case before every transaction; the largest
worst-case reservation of a run was 0.0015 SOL.

**Targets** (client from gossip `clientId`, version string and stake share at planning time):

| client | stake share of the client | validators probed |
|---|---|---|
| AgaveBam (4.3.2) | 36.5% | `DRpbCBMx…`, `5Cchr1XG…` |
| HarmonicAgave (4.3.2) | 23.6% | `Fd7btgyS…`, `5ZqveVff…` |
| JitoLabs (4.3.2) | 23.2% | `HEL1USMZ…`, `E1r4Psq8…` |
| Firedancer (26.10.4) | 6.6% | `9eGrDohd…`, `JD549Hsb…` |
| HarmonicFiredancer (26.10.4) | 6.2% | `JUPiTERr…`, `Awes4Tr6…` |
| Agave (4.3.2) | 1.1% | `7QQGNm3p…`, `5Us18hLZ…` |

Together these six clients held 97.2% of stake; FireBAM (1.4%) and Rakurai (1.2%) were not probed.

## What decides the order — the answer

**The rule depends far more on *how* transactions compete than on which client produces the block.** The same
pattern shows up in every one of the six clients:

1. **Conflicting transactions (all write one shared account) are ordered by priority fee** — in every client
   (price tau +0.61 … +1.00, q ≤ 0.024). The strictest is HarmonicFiredancer: 97% of pairs in price order, and
   price wins 96% of the pairs where price and arrival disagree.
2. **Transactions sent 30 ms apart are ordered by arrival** — in every client (76–100% of pairs). Where enough
   pairs separate the two rules, arrival beats price outright: Firedancer (arrival wins 80% of head-to-head
   pairs, q = 0.012) and HarmonicFiredancer (100%, q = 0.023).
3. **Non-conflicting transactions sent together (≈0.3 ms apart) show no rule** — except on **AgaveBam**, where
   the priority fee decides (81% of pairs) and beats arrival in 86% of head-to-head pairs (q = 0.017).

A reading consistent with this: the scheduler applies priority only among transactions that are waiting at the
same moment *and* compete for the same lock. Conflicting transactions must be serialised, so they come out in
priority order; non-conflicting ones are executed in parallel and their recorded order mixes threads; a
transaction that arrives 30 ms later is usually not yet waiting when the earlier one is scheduled. AgaveBam is the
only client that also price-orders simultaneous non-conflicting transactions, which fits an ordering stage in
front of the scheduler — but the measurement shows only the order in the block, not which component made it.

Per client and mode (bold: q < 0.05 with at least 10 pairs; "n" counts pairs of our probes in one block;
"head-to-head" uses only pairs where the higher-priced probe was written later, +1 = price always won):

| client | mode | blocks | price | arrival | head-to-head (price vs arrival) | verdict |
|---|---|---|---|---|---|---|
| Agave | conflict | 4 | **+0.75 (88%, n=24, q=0.012)** | +0.25 (62%, n=24, q=0.518) | +0.50 (75%, n=12, q=0.366) | priority fee decides (loose: 88% of pairs follow it) |
| Agave | stagger | 6 | +0.21 (61%, n=33, q=0.504) | **+0.64 (82%, n=33, q=0.012)** | -0.41 (29%, n=17, q=0.366) | arrival order decides (82% of pairs follow it) |
| Agave | burst | 3 | +0.44 (72%, n=18, q=0.343) | +0.56 (78%, n=18, q=0.187) | -0.14 (43%, n=7, q=1.000) | no rule detected: ordering indistinguishable from random |
| AgaveBam | conflict | 2 | **+1.00 (100%, n=12, q=0.014)** | +0.33 (67%, n=12, q=0.598) | +1.00 (100%, n=4, q=0.469) | priority fee decides (strict: 100% of pairs follow it) |
| AgaveBam | stagger | 3 | +0.56 (78%, n=18, q=0.187) | **+1.00 (100%, n=18, q=0.002)** | -1.00 (0%, n=4, q=0.366) | arrival order decides (100% of pairs follow it) |
| AgaveBam | burst | 6 | **+0.61 (81%, n=36, q=0.012)** | -0.28 (36%, n=36, q=0.366) | **+0.73 (86%, n=22, q=0.017)** | priority fee decides (loose: 81% of pairs follow it) |
| JitoLabs | conflict | 6 | **+0.67 (83%, n=36, q=0.006)** | **+0.61 (81%, n=36, q=0.007)** | +0.08 (54%, n=13, q=1.000) | both price and arrival correlate; head-to-head pairs do not separate them |
| JitoLabs | stagger | 8 | +0.11 (55%, n=38, q=0.803) | **+0.53 (76%, n=38, q=0.026)** | -0.44 (28%, n=18, q=0.348) | arrival order decides (76% of pairs follow it) |
| JitoLabs | burst | 7 | +0.38 (69%, n=42, q=0.152) | +0.10 (55%, n=42, q=0.804) | +0.33 (67%, n=18, q=0.469) | no rule detected: ordering indistinguishable from random |
| HarmonicAgave | conflict | 3 | **+0.78 (89%, n=18, q=0.024)** | +0.11 (56%, n=18, q=0.930) | +0.60 (80%, n=10, q=0.363) | priority fee decides (loose: 89% of pairs follow it) |
| HarmonicAgave | stagger | 10 | +0.27 (63%, n=41, q=0.348) | **+0.71 (85%, n=41, q=0.004)** | -0.47 (26%, n=19, q=0.268) | arrival order decides (85% of pairs follow it) |
| HarmonicAgave | burst | 7 | +0.00 (50%, n=42, q=1.000) | -0.24 (38%, n=42, q=0.394) | +0.33 (67%, n=15, q=0.504) | no rule detected: ordering indistinguishable from random |
| Firedancer | conflict | 6 | **+0.61 (81%, n=36, q=0.012)** | +0.39 (69%, n=36, q=0.187) | +0.25 (62%, n=16, q=0.659) | priority fee decides (loose: 81% of pairs follow it) |
| Firedancer | stagger | 9 | -0.26 (37%, n=54, q=0.305) | **+0.63 (81%, n=54, q=0.003)** | **-0.60 (20%, n=40, q=0.012)** | arrival order decides (81% of pairs follow it) |
| Firedancer | burst | 8 | +0.25 (62%, n=48, q=0.348) | +0.12 (56%, n=48, q=0.665) | +0.11 (56%, n=27, q=0.842) | no rule detected: ordering indistinguishable from random |
| HarmonicFiredancer | conflict | 6 | **+0.94 (97%, n=36, q=0.002)** | -0.22 (39%, n=36, q=0.469) | **+0.91 (96%, n=23, q=0.003)** | priority fee decides (strict: 97% of pairs follow it) |
| HarmonicFiredancer | stagger | 5 | -0.05 (48%, n=21, q=1.000) | **+1.00 (100%, n=21, q=0.002)** | **-1.00 (0%, n=11, q=0.023)** | arrival order decides (100% of pairs follow it) |
| HarmonicFiredancer | burst | 8 | +0.29 (65%, n=48, q=0.268) | +0.04 (52%, n=48, q=0.979) | +0.30 (65%, n=20, q=0.472) | no rule detected: ordering indistinguishable from random |

Pooled over modes, a client's verdict mostly reflects this mix: Agave and JitoLabs show *both* price and arrival
correlating, with head-to-head pairs at about 50% — because conflict pairs pull towards price and stagger pairs
towards arrival.

| client | blocks | price | arrival | head-to-head | verdict |
|---|---|---|---|---|---|
| Agave | 13 | **+0.44 (72%, n=75, q=0.004)** | **+0.49 (75%, n=75, q=0.002)** | -0.06 (47%, n=36, q=0.951) | both price and arrival correlate; head-to-head pairs do not separate them |
| AgaveBam | 11 | **+0.67 (83%, n=66, q=0.001)** | +0.18 (59%, n=66, q=0.312) | **+0.53 (77%, n=30, q=0.040)** | priority fee decides (loose: 83% of pairs follow it) |
| JitoLabs | 21 | **+0.38 (69%, n=116, q=0.002)** | **+0.40 (70%, n=116, q=0.002)** | -0.02 (49%, n=49, q=1.000) | both price and arrival correlate; head-to-head pairs do not separate them |
| HarmonicAgave | 20 | +0.25 (62%, n=101, q=0.092) | +0.21 (60%, n=101, q=0.147) | +0.05 (52%, n=44, q=0.951) | no rule detected: ordering indistinguishable from random |
| Firedancer | 23 | +0.14 (57%, n=138, q=0.247) | **+0.39 (70%, n=138, q=0.001)** | -0.20 (40%, n=83, q=0.223) | arrival order decides (70% of pairs follow it) |
| HarmonicFiredancer | 19 | **+0.45 (72%, n=105, q=0.001)** | +0.14 (57%, n=105, q=0.312) | +0.30 (65%, n=54, q=0.142) | priority fee decides (loose: 72% of pairs follow it) |

## Per validator

Each validator got every mode, so its pooled verdict also depends on the mode mix of the batches that landed; a
per-validator-and-mode split has only 2–4 blocks per cell and is not claimed.

| validator | client | version | stake | blocks | price | arrival | head-to-head | verdict (all modes pooled) |
|---|---|---|---|---|---|---|---|---|
| `5Us18hLZPXJTS4QVuGSsUw137Dyd2tgBaem24Xsf5nBS` | Agave | 4.3.2 | 0.28% | 6 | **+0.58 (79%, n=33, q=0.028)** | +0.39 (70%, n=33, q=0.180) | +0.20 (60%, n=15, q=0.674) | priority fee decides (loose: 79% of pairs follow it) |
| `7QQGNm3ptwinipDCyaCF7jY5katgmFUu1ieP2f7nwLpE` | Agave | 4.3.2 | 0.43% | 7 | +0.33 (67%, n=42, q=0.193) | **+0.57 (79%, n=42, q=0.012)** | -0.24 (38%, n=21, q=0.585) | arrival order decides (79% of pairs follow it) |
| `5Cchr1XGEg7dbBXByV5NY2ad8jfxAM7HA3x8D56rq9Ux` | AgaveBam | 4.3.2 | 1.30% | 7 | **+0.76 (88%, n=42, q=0.001)** | +0.19 (60%, n=42, q=0.490) | **+0.75 (88%, n=16, q=0.043)** | priority fee decides (loose: 88% of pairs follow it) |
| `DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy` | AgaveBam | 4.3.2 | 2.87% | 4 | +0.50 (75%, n=24, q=0.155) | +0.17 (58%, n=24, q=0.645) | +0.29 (64%, n=14, q=0.613) | no rule detected: ordering indistinguishable from random |
| `E1r4Psq84tHfQ6aPTvvDka4U3u8zPVD7gEUrH25RdxHL` | JitoLabs | 4.3.2 | 2.50% | 9 | +0.30 (65%, n=54, q=0.191) | **+0.48 (74%, n=54, q=0.020)** | -0.22 (39%, n=23, q=0.607) | arrival order decides (74% of pairs follow it) |
| `HEL1USMZKAL2odpNBj2oCjffnFGaYwmbGmyewGv1e2TU` | JitoLabs | 4.3.2 | 3.66% | 12 | **+0.45 (73%, n=62, q=0.020)** | +0.32 (66%, n=62, q=0.119) | +0.15 (58%, n=26, q=0.645) | priority fee decides (loose: 73% of pairs follow it) |
| `5ZqveVffQPiUbkjBg4KD9kib1MKHLqiFno4ke9jSq9qk` | HarmonicAgave | 4.3.2 | 0.93% | 11 | +0.21 (61%, n=56, q=0.367) | **+0.46 (73%, n=56, q=0.020)** | -0.33 (33%, n=21, q=0.409) | arrival order decides (73% of pairs follow it) |
| `Fd7btgySsrjuo25CJCj7oE7VPMyezDhnx7pZkj2v69Nk` | HarmonicAgave | 4.3.2 | 3.92% | 9 | +0.29 (64%, n=45, q=0.260) | -0.11 (44%, n=45, q=0.645) | +0.39 (70%, n=23, q=0.299) | no rule detected: ordering indistinguishable from random |
| `9eGrDohdNTAo61DRHyfMuqKWXqYnA3i254Wiszxe8FoY` | Firedancer | 26.10.4 | 1.61% | 11 | **+0.48 (74%, n=66, q=0.009)** | +0.09 (55%, n=66, q=0.645) | +0.28 (64%, n=47, q=0.297) | priority fee decides (loose: 74% of pairs follow it) |
| `JD549HsbJHeEKKUrKgg4Fj2iyv2RGjsV7NTZjZUrHybB` | Firedancer | 26.10.4 | 1.51% | 12 | -0.17 (42%, n=72, q=0.409) | **+0.67 (83%, n=72, q=0.001)** | **-0.83 (8%, n=36, q=0.001)** | arrival order decides (83% of pairs follow it) |
| `Awes4Tr6TX8JDzEhCZY2QVNimT6iD1zWHzf1vNyGvpLM` | HarmonicFiredancer | 26.10.4 | 1.60% | 9 | **+0.45 (73%, n=51, q=0.031)** | +0.18 (59%, n=51, q=0.489) | +0.30 (65%, n=23, q=0.409) | priority fee decides (loose: 73% of pairs follow it) |
| `JUPiTERrZqgf1jUyR7dSkhMx4Kn2qJyekWsg3LT1h4b` | HarmonicFiredancer | 26.10.4 | 2.23% | 10 | **+0.44 (72%, n=54, q=0.028)** | +0.11 (56%, n=54, q=0.645) | +0.29 (65%, n=31, q=0.387) | priority fee decides (loose: 72% of pairs follow it) |

The clearest validator-level contrast is inside **Firedancer 26.10.4**: `JD549Hsb…` is arrival-ordered (arrival
beats price in 92% of 36 head-to-head pairs, q = 0.001), while `9eGrDohd…` leans to price (74% of pairs, q =
0.009). The mode mix does not explain it: `JD549Hsb…` contributed 4 burst, 4 conflict and 4 stagger blocks,
`9eGrDohd…` 4, 2 and 5 — the price-leaning one had *more* stagger blocks. Same client, same version, different
behaviour — configuration or what sits in front of the node; the data cannot say which. The two
HarmonicFiredancer validators agree with each other. The two Agave and the two JitoLabs validators come out once
"price" and once "arrival" pooled, and AgaveBam and HarmonicAgave each have one significant and one weaker result;
at 4–12 blocks per validator, with uneven mode mixes, this is as consistent with sample size as with a real
difference, and is not claimed as one.

## Where the probes went

Not ordering evidence, but it describes the path from our machine to the block (`data/RESULTS.md`, second table):

| client | probes sent | refused at send (QUIC) | targeted leader's block | another leader's block | never landed | median slots after window start (another leader) |
|---|---|---|---|---|---|---|
| Agave | 96 | 0 | 52 | 0 | 44 | — |
| AgaveBam | 104 | 14 | 44 | 0 | 46 | — |
| Firedancer | 100 | 0 | 92 | 0 | 8 | — |
| HarmonicAgave | 112 | 0 | 76 | 36 | 0 | 24 |
| HarmonicFiredancer | 108 | 0 | 76 | 32 | 0 | 32 |
| JitoLabs | 112 | 8 | 80 | 0 | 24 | — |

* **Firedancer** included 92% of what we wrote to it, in its own block.
* **Harmonic** (both variants) included *everything*, but about 30% of it in a block of a *different* leader, a
  median 24–32 slots (5–7 s) after the targeted window. Harmonic validators advertise TPU ports on shared ingress
  hosts; what we see fits an ingress that forwards to whoever produces next rather than to the targeted node.
  Those landings are excluded from the targeted validator's evidence.
* **Agave, AgaveBam and JitoLabs** dropped 21–46% of our probes without a trace, and AgaveBam (`DRpbCBMx…`, 4
  batches) and JitoLabs (2 batches) refused the QUIC streams of our unstaked connection outright. A dropped
  probe is a missing data point, not a reordering, so it cannot bias the order tests — but it does shrink some
  cells.
* Batches landed almost all-or-nothing: of the 114 groups (probes of one batch in one block of the targeted
  leader), 96 held all 4 probes, 11 held 2–3 and 7 a single probe; 9 of 105 batches were split across two of the
  leader's blocks.

## What went wrong

1. **Small batches.** The wallet held 0.009 SOL, and most of a run's funding is rent for the sub-payers (returned
   at the end). Batches were therefore cut from the designed 8 probes to 4: 6 pairs per block instead of 28. This
   is why per-validator-and-mode cells are too small to claim anything and why we ran two live runs.
2. **Dry-run batches fired late.** In the dry run 4 of 34 batches would have gone out 10–19 slots after their
   window had begun (`in_window_at_send: false` in `batches.jsonl`), and 2 were skipped as already started. Nothing
   is sent in a dry run, so this cost nothing, but it is why live runs carry the late guard: a batch whose observed
   slot has reached window start + 2 is skipped instead of sent.
3. **Late batches skipped.** 7 live batches were skipped (live1: 4 late + 2 already started, live2: 1 late),
   mostly when windows of different validators were only a few slots apart: the tool handles one window at a time,
   and the QUIC handshake for the next one ate into it. Skipping is deliberate — it pays no fees for probes that
   would land with the next leader.
4. **Unstaked QUIC refusals.** `closed by peer: dropped (code 1)` from AgaveBam `DRpbCBMx…` (4 batches, live2)
   and JitoLabs (`HEL1USMZ…`, `E1r4Psq8…`, one batch each, live1). These probes are recorded with the error and
   never counted as sent.
5. **Live run 2 was stopped after batch 92 of 96** — the host machine ran low on memory and the process was
   killed; the last three windows (all of `5Us18hLZ…`) were never reached. The sub-payer keys had been written to
   `keys/` before funding, so `itb sweep` returned every lamport (all four sub-payers verified at 0 afterwards).
   This is the recovery path the tool was built with, exercised for real.
6. **An analysis bug caught before publication.** A first version counted probes that missed their window and
   landed in the *next* leader's block as evidence about the targeted validator. For Harmonic this was a third of
   the data. Evidence is now restricted to blocks whose leader is the targeted identity (`leader` in
   `landed.jsonl`), and the multiple-testing correction and head-to-head test were added at the same time.
7. **Public RPC limits.** `getBlock` on the public endpoint returns 429 under bursts; collection backs off
   exponentially and waits 600 ms between blocks. `getSignatureStatuses` is used to find the exact slot of every
   probe, including late ones.
8. **Docker was not run on the measuring machine** (no Docker there). The image is built, the dry-run planner is
   run against mainnet, and the published statistics are re-derived and compared, in GitHub Actions
   (`.github/workflows/ci.yml`).

## Limits of these conclusions

* One evening, one epoch, two hours. Load and validator configuration change; the rule could too.
* "Arrival" means the order in which our machine finished writing the streams. Arrival at the scheduler can
  differ (network, QUIC stream handling, any builder or relay in front of the node).
* The probes are tiny and cheap (≤ 100 000 micro-lamports/CU). The rule among high-value transactions or under
  Jito bundles/tips is out of scope.
* "No rule detected" with 18–48 pairs is a statement about the evidence, not proof of random ordering.

## Verifying a claim yourself

Every located probe has its slot and index in `data/landed.jsonl`. Example (the last probe of live run 2):

```bash
curl -s https://api.mainnet-beta.solana.com -H 'Content-Type: application/json' -d \
  '{"jsonrpc":"2.0","id":1,"method":"getBlock","params":[460214962,{"maxSupportedTransactionVersion":1,"transactionDetails":"signatures","rewards":false}]}' \
  | jq -r '.result.signatures[304]'
# 4FHcA14LrFtyRHGvKkFBhbyYj1632uq7XnhuVqqqhhB1J7NBZCRQKg1MxPMctcWVJiqZDY9tAVjWYXFkKvjZ1zLr
```

That signature is batch `live-1792707349-b092` in `data/sends.jsonl`, where its CU price, send position and send
time are recorded. The same block is saved in `data/blocks/460214962.json`.
