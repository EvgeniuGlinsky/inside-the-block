# Report — how Solana leaders ordered our transactions (22 and 27 Oct 2026)

Every number below is produced by `itb analyze` from the raw records in `data/` (see `data/RESULTS.md`, which
`docker compose run --rm reanalyze` regenerates byte for byte). All measurements were taken on mainnet after the
Alpenglow cutover (last block with vote transactions: slot 459 652 999, 2026-10-21 13:54 UTC — see README), in
epochs 1065 and 1069.

## Measurement runs

| run | date and time (UTC) | slots of the probed windows | batches planned / sent / skipped | probes sent | probes located on chain |
|---|---|---|---|---|---|
| `dry-1792702925` (dry run, nothing sent) | 2026-10-22 21:02 | 460 180 720 – 460 182 972 | 36 / 34 / 2 (8 probes each) | 0 (272 built and signed) | — |
| `live-1792705232` | 2026-10-22 21:40 – 22:14 (first / last landing 21:41:25 / 22:13:31) | 460 191 532 – 460 200 704 | 72 / 66 / 6 | 264 | 208 |
| `live-1792707349` | 2026-10-22 22:15 – 23:05 (first / last landing 22:16:42 / 23:04:00) | 460 201 600 – 460 214 960 | 96 / 92 / 1, 3 not reached (run stopped, see below) | 368 | 280 |
| `live-1793110054` | 2026-10-27 14:07 – 14:29 (first / last landing 14:08:52 / 14:28:38) | 462 098 832 – 462 104 468 | 72 / 67 / 5 | 268 | 176 |

All three live runs probed the same 12 validators (`data/plan-live1.json`, `plan-live2.json`, `plan-live3.json`;
the third run was planned with `--identities` set to the same list). Live batches had 4 probes (30 000 CU limit
each, prices a random 4 of the 8 levels 0 … 100 000 micro-lamports/CU, send order an independent permutation);
modes rotated burst → stagger → conflict over each validator's windows. "Probes sent" includes the probes whose
QUIC stream the leader refused (see "Where the probes went").

**Spend.** The wallet went from 0.009 000 SOL to 0.005 086 SOL: **0.003 914 SOL** for the three live runs —
fees of the 664 probes that landed (0.003 794 SOL) plus the funding and sweep transfers. Probes that never land
cost nothing. Every run reserved the worst case of each transaction before sending it under `--max-spend-sol
0.05`. Since run 3 the cap is also cumulative: `run --live` counts what earlier live runs in the same data
directory spent (exact fees of located probes, worst case for the rest, funding and sweep fees) and refuses to
start if that plus its own worst case would pass the cap; a cap above 0.05 SOL is an error without
`--allow-over-budget`. Run 3 itself was sent with the per-run cap only.

**Targets** (client from gossip `clientId`, version string and stake share at the first planning; the client
and version of every target were the same at the third planning):

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

**The rule depends more on *how* transactions compete than on which client produces the block.** A rule is
claimed only at q < 0.05 with at least 10 pairs from at least 4 blocks; "head-to-head" counts only the pairs
where price and arrival disagree (the higher-priced probe was written later), so it is the test that tells the
two rules apart.

1. **Conflicting transactions (all write one shared account) are ordered by priority fee in five of the six
   clients**: Agave (75% of pairs), AgaveBam (100%), Firedancer (74%), HarmonicAgave (92%, 4 blocks) and
   HarmonicFiredancer (96%). Price beats arrival head-to-head significantly in two of them — AgaveBam (15 of 15
   pairs, q = 0.009) and HarmonicFiredancer (+0.85, n = 27, q = 0.002); in the other three the head-to-head pairs
   are too few to rule arrival out. **JitoLabs is not shown to be price-ordered**: price and arrival both
   correlate (+0.52 / +0.62) and the head-to-head pairs split evenly (−0.11, n = 18); restricted to landings inside
   the planned window, arrival decides (89% of pairs).
2. **Transactions sent 30 ms apart are ordered by arrival in five of the six clients**: Agave (82%), Firedancer
   (81%), HarmonicAgave (84%), HarmonicFiredancer (100%) and JitoLabs (76%). Arrival beats price head-to-head
   significantly in Firedancer (−0.62, n = 53, q = 0.001), HarmonicAgave (−0.62, n = 26, q = 0.030) and
   HarmonicFiredancer (−1.00, n = 11, q = 0.019). AgaveBam's stagger cell (4 blocks) has both correlating and only 7
   head-to-head pairs — not separable.
3. **Non-conflicting transactions sent together (whole batch within ≈0.3 ms) mostly show no rule**: HarmonicAgave,
   HarmonicFiredancer and JitoLabs — no rule of |tau| ≥ 0.43–0.48 (the strongest these cells could detect). The
   exceptions are **AgaveBam**, where the priority fee decides (81% of pairs) and beats arrival head-to-head (+0.67,
   n = 24, q = 0.019), and, more loosely, **Firedancer** (68% of pairs follow price, q = 0.041, head-to-head not
   significant). Agave's burst cell leans to arrival (83%) from only 4 blocks.

**The same signs repeat run by run** (`data/RESULTS.md`, "does each run say the same?"): AgaveBam conflict has
price tau +1.00 in all three runs, HarmonicFiredancer conflict +1.00 / +0.92 / +0.83, Firedancer stagger arrival
+0.67 / +0.60 / +0.58 with price negative in all three, HarmonicAgave stagger arrival +0.87 / +0.62 / +0.60.

**A reading consistent with this:** the scheduler applies priority only among transactions that are waiting at
the same moment *and* compete for the same lock. Conflicting transactions must be serialised, so they come out in
priority order; non-conflicting ones are executed in parallel and their recorded order mixes threads; a
transaction that arrives 30 ms later is usually not yet waiting when the earlier one is scheduled. The split by
landing slot tests this directly: probes landing in the **first slot** of the window were sent one slot early and
can be waiting together when the leader starts, later ones arrive while it is already producing. Firedancer's
burst and conflict probes in slot 0 are price-ordered (75% and 76% of pairs), its later landings are
arrival-ordered (76%); Agave's later landings are arrival-ordered (94%, 8 blocks). AgaveBam is price-ordered in
both subsets (89% / 86%), which fits an ordering stage in front of the scheduler — but the measurement shows only
the order in the block, not which component made it.

**Secondary evidence — the rest of the block.** Over every targeted-leader block that holds a probe, the other
transactions are *not* sorted by fee per compute unit: median Kendall tau +0.00 … +0.03 for five clients and +0.12
for AgaveBam. Our probes pay at most 0.1 lamport per CU, yet sit high in their blocks (median relative position
0.12–0.26; 0.44 for AgaveBam), above 56–87% of the transactions that paid more per CU. So "priority fee decides"
above is a statement about transactions that compete at the same time for the same lock, not about blocks as a
whole. Fee per CU is approximated from the block (fee above the base fee over consumed CU); see `data/RESULTS.md`
for the caveats.

Per client and mode (all landings in the targeted leader's blocks; "%" is the share of pairs following the rule;
bootstrap intervals and the detectable effect of every cell are in `data/RESULTS.md`):

| client | mode | blocks | price tau | arrival tau | head-to-head | verdict |
|---|---|---|---|---|---|---|
| Agave | conflict | 6 | **+0.50 (q=0.040)** | +0.44 (q=0.076) | +0.06 (n=17, q=1.000) | priority fee decides (75%) |
| Agave | stagger | 9 | +0.25 (q=0.294) | **+0.65 (q=0.002)** | −0.38 (n=26, q=0.243) | arrival decides (82%) |
| Agave | burst | 4 | +0.25 (q=0.565) | **+0.67 (q=0.025)** | −0.45 (n=11, q=0.511) | arrival decides (83%) |
| AgaveBam | conflict | 5 | **+1.00 (q=0.001)** | +0.00 (q=1.000) | **+1.00 (n=15, q=0.009)** | priority fee decides (strict, 100%) |
| AgaveBam | stagger | 4 | **+0.67 (q=0.025)** | **+0.75 (q=0.009)** | −0.14 (n=7, q=1.000) | both correlate, not separated |
| AgaveBam | burst | 7 | **+0.62 (q=0.004)** | −0.14 (q=0.690) | **+0.67 (n=24, q=0.019)** | priority fee decides (81%) |
| JitoLabs | conflict | 7 | **+0.52 (q=0.019)** | **+0.62 (q=0.006)** | −0.11 (n=18, q=0.932) | both correlate, not separated |
| JitoLabs | stagger | 8 | +0.11 (q=0.795) | **+0.53 (q=0.023)** | −0.44 (n=18, q=0.294) | arrival decides (76%) |
| JitoLabs | burst | 8 | +0.42 (q=0.051) | +0.08 (q=0.809) | +0.40 (n=20, q=0.294) | no rule detected (≥ ±0.48) |
| HarmonicAgave | conflict | 4 | **+0.83 (q=0.002)** | +0.17 (q=0.737) | +0.67 (n=12, q=0.181) | priority fee decides (92%) |
| HarmonicAgave | stagger | 13 | +0.11 (q=0.718) | **+0.68 (q=0.001)** | **−0.62 (n=26, q=0.030)** | arrival decides (84%) |
| HarmonicAgave | burst | 8 | −0.08 (q=0.809) | −0.25 (q=0.294) | +0.22 (n=18, q=0.703) | no rule detected (≥ ±0.48) |
| Firedancer | conflict | 9 | **+0.48 (q=0.019)** | +0.26 (q=0.259) | +0.25 (n=24, q=0.565) | priority fee decides (74%) |
| Firedancer | stagger | 13 | −0.23 (q=0.207) | **+0.62 (q=0.001)** | **−0.62 (n=53, q=0.001)** | arrival decides (81%) |
| Firedancer | burst | 11 | **+0.36 (q=0.041)** | +0.15 (q=0.521) | +0.19 (n=37, q=0.569) | priority fee decides (loose, 68%) |
| HarmonicFiredancer | conflict | 8 | **+0.92 (q=0.001)** | −0.04 (q=0.962) | **+0.85 (n=27, q=0.002)** | priority fee decides (strict, 96%) |
| HarmonicFiredancer | stagger | 6 | +0.08 (q=0.932) | **+1.00 (q=0.001)** | **−1.00 (n=11, q=0.019)** | arrival decides (100%) |
| HarmonicFiredancer | burst | 10 | +0.30 (q=0.144) | +0.17 (q=0.511) | +0.17 (n=24, q=0.727) | no rule detected (≥ ±0.43) |

Pooled over modes, a client's verdict mostly reflects the mode mix: AgaveBam is price-ordered (88% of pairs,
head-to-head +0.65, q = 0.001), HarmonicFiredancer price-ordered (74%), Firedancer arrival-ordered (68%), Agave and
JitoLabs show both correlating with head-to-head near zero (conflict pulls towards price, stagger towards
arrival), and HarmonicAgave pooled shows no rule of |tau| ≥ 0.29.

**Robustness checks** (`data/RESULTS.md`): restricting to landings inside the planned window changes the verdict
type of three cells — JitoLabs conflict becomes arrival-ordered (89%), Firedancer pooled becomes "both correlate",
HarmonicAgave pooled becomes arrival-ordered (67%) — and HarmonicAgave conflict drops to 3 blocks (not judged).
No cell flips between price and arrival.

## Per validator

Each validator got every mode, so its pooled verdict also depends on the mode mix of the batches that landed; a
per-validator-and-mode split has only 1–7 blocks per cell and is not claimed.

| validator | client | version | blocks | price tau | arrival tau | head-to-head | verdict (all modes pooled) |
|---|---|---|---|---|---|---|---|
| `5Us18hLZPXJTS4QVuGSsUw137Dyd2tgBaem24Xsf5nBS` | Agave | 4.3.2 | 8 | +0.33 (q=0.166) | **+0.51 (q=0.012)** | −0.17 (n=24, q=0.681) | arrival decides (76%) |
| `7QQGNm3ptwinipDCyaCF7jY5katgmFUu1ieP2f7nwLpE` | Agave | 4.3.2 | 11 | +0.33 (q=0.067) | **+0.64 (q=0.001)** | −0.33 (n=30, q=0.250) | arrival decides (82%) |
| `5Cchr1XGEg7dbBXByV5NY2ad8jfxAM7HA3x8D56rq9Ux` | AgaveBam | 4.3.2 | 10 | **+0.83 (q=0.001)** | +0.07 (q=0.792) | **+0.85 (n=27, q=0.001)** | priority fee decides (92%) |
| `DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy` | AgaveBam | 4.3.2 | 6 | **+0.61 (q=0.010)** | +0.22 (q=0.407) | +0.37 (n=19, q=0.359) | priority fee decides (81%) |
| `E1r4Psq84tHfQ6aPTvvDka4U3u8zPVD7gEUrH25RdxHL` | JitoLabs | 4.3.2 | 9 | +0.30 (q=0.180) | **+0.48 (q=0.012)** | −0.22 (n=23, q=0.571) | arrival decides (74%) |
| `HEL1USMZKAL2odpNBj2oCjffnFGaYwmbGmyewGv1e2TU` | JitoLabs | 4.3.2 | 14 | **+0.41 (q=0.012)** | +0.32 (q=0.067) | +0.09 (n=33, q=0.800) | priority fee decides (70%) |
| `5ZqveVffQPiUbkjBg4KD9kib1MKHLqiFno4ke9jSq9qk` | HarmonicAgave | 4.3.2 | 12 | +0.19 (q=0.372) | **+0.49 (q=0.009)** | −0.39 (n=23, q=0.248) | arrival decides (75%) |
| `Fd7btgySsrjuo25CJCj7oE7VPMyezDhnx7pZkj2v69Nk` | HarmonicAgave | 4.3.2 | 13 | +0.16 (q=0.400) | +0.01 (q=1.000) | +0.15 (n=33, q=0.638) | no rule detected (≥ ±0.40) |
| `9eGrDohdNTAo61DRHyfMuqKWXqYnA3i254Wiszxe8FoY` | Firedancer | 26.10.4 | 16 | **+0.54 (q=0.001)** | +0.21 (q=0.193) | +0.27 (n=60, q=0.193) | priority fee decides (77%) |
| `JD549HsbJHeEKKUrKgg4Fj2iyv2RGjsV7NTZjZUrHybB` | Firedancer | 26.10.4 | 17 | −0.20 (q=0.193) | **+0.51 (q=0.001)** | **−0.67 (n=54, q=0.001)** | arrival decides (75%) |
| `Awes4Tr6TX8JDzEhCZY2QVNimT6iD1zWHzf1vNyGvpLM` | HarmonicFiredancer | 26.10.4 | 11 | **+0.46 (q=0.011)** | +0.21 (q=0.314) | +0.29 (n=28, q=0.359) | priority fee decides (73%) |
| `JUPiTERrZqgf1jUyR7dSkhMx4Kn2qJyekWsg3LT1h4b` | HarmonicFiredancer | 26.10.4 | 13 | **+0.51 (q=0.003)** | +0.28 (q=0.153) | +0.24 (n=34, q=0.400) | priority fee decides (75%) |

The clearest validator-level contrast is still inside **Firedancer 26.10.4**, now over three runs: `JD549Hsb…` is
arrival-ordered (arrival beats price in 83% of 54 head-to-head pairs, q = 0.001), while `9eGrDohd…` leans to price
(77% of pairs, q = 0.001). The mode mix explains part of it but not the head-to-head result: `JD549Hsb…`
contributed 5 burst, 6 stagger and 6 conflict blocks, `9eGrDohd…` 6, 7 and 3 — the price-leaning one has *fewer*
conflict blocks. Same client, same version, different behaviour — configuration or what sits in front of the node;
the data cannot say which. The two HarmonicFiredancer validators agree with each other, and so do the two Agave
validators (both arrival). The two AgaveBam validators agree on price. JitoLabs and HarmonicAgave each have one
arrival-leaning and one other validator; at 9–14 blocks with uneven mode mixes this is not claimed as a real
difference.

## Where the probes went

Not ordering evidence, but it describes the path from our machine to the block (`data/RESULTS.md`, second table):

| client | probes sent | refused at send (QUIC) | targeted leader's block | another leader's block | never landed | median slots after window start (another leader) |
|---|---|---|---|---|---|---|
| Agave | 140 | 4 | 76 | 0 | 60 | — |
| AgaveBam | 152 | 18 | 64 | 0 | 70 | — |
| Firedancer | 148 | 0 | 132 | 0 | 16 | — |
| HarmonicAgave | 156 | 0 | 96 | 60 | 0 | 24 |
| HarmonicFiredancer | 152 | 0 | 96 | 52 | 4 | 32 |
| JitoLabs | 152 | 16 | 88 | 0 | 48 | — |

* **Firedancer** included 89% of what we wrote to it, in its own block.
* **Harmonic** (both variants) included almost everything, but 34–38% of it in a block of a *different*
  leader, a median 24–32 slots (5–7 s) after the targeted window. All four Harmonic targets advertise their TPU on
  one shared host (198.13.140.105); what we see fits an ingress that forwards to whoever produces next rather than
  to the targeted node. Those landings are excluded from the targeted validator's evidence.
* **Agave, AgaveBam and JitoLabs** dropped 35–52% of what they did not refuse, without a trace, and refused the QUIC
  streams of our unstaked connection outright for some batches (AgaveBam `DRpbCBMx…`, JitoLabs `HEL1USMZ…` and
  `E1r4Psq8…`, Agave `5Us18hLZ…`). A dropped probe is a missing data point, not a reordering, so it cannot bias the
  order tests — but it does shrink some cells.
* Batches landed almost all-or-nothing: of the 149 groups (probes of one batch in one block of the targeted
  leader), 127 held all 4 probes, 9 held 3, 4 held 2 and 9 a single probe; 11 of 138 batches were split across two
  of the leader's blocks.

## What went wrong

1. **Small batches.** The wallet held 0.009 SOL, and most of a run's funding is rent for the sub-payers (returned
   at the end). Batches were therefore cut from the designed 8 probes to 4: 6 pairs per block instead of 28. This
   is why per-validator-and-mode cells are too small to claim anything and why we ran three live runs.
2. **Dry-run batches fired late.** In the dry run 4 of 34 batches would have gone out 10–19 slots after their
   window had begun (`in_window_at_send: false` in `batches.jsonl`), and 2 were skipped as already started. Nothing
   is sent in a dry run, so this cost nothing, but it is why live runs carry the late guard: a batch whose observed
   slot has reached window start + 2 is skipped instead of sent.
3. **Late batches skipped.** 12 live batches were skipped (run 1: 4 late + 2 already started, run 2: 1 late,
   run 3: 3 late + 2 already started), mostly when windows of different validators were only a few slots apart:
   the tool handles one window at a time, and the QUIC handshake for the next one ate into it. Skipping is
   deliberate — it pays no fees for probes that would land with the next leader.
4. **Unstaked QUIC refusals.** `closed by peer: dropped (code 1)` from AgaveBam `DRpbCBMx…` (4 batches in run 2,
   1 in run 3), JitoLabs `HEL1USMZ…` and `E1r4Psq8…` (one batch each in runs 1 and 3) and Agave `5Us18hLZ…` (one
   batch in run 3). These probes are recorded with the error; they are counted in "probes sent" and "refused at
   send", never as landed or missing.
5. **Live run 2 was stopped after batch 92 of 96** — the host machine ran low on memory and the process was
   killed; the last three windows (all of `5Us18hLZ…`) were never reached. The sub-payer keys had been written to
   `keys/` before funding, so `itb sweep` returned every lamport (all four sub-payers verified at 0 afterwards).
   Run 3 was started as a detached process for that reason.
6. **Run 3 asked for more than the wallet held.** A first attempt planned 9 windows per validator (108 batches);
   the pre-funding check refused it (6 767 632 lamports needed, 6 135 000 held) before anything was sent, and the
   run was re-planned with 6 windows per validator.
7. **An analysis bug caught before the first submission.** A first version counted probes that missed their window
   and landed in the *next* leader's block as evidence about the targeted validator. For Harmonic this was a third
   of the data. Evidence is restricted to blocks whose leader is the targeted identity (`leader` in
   `landed.jsonl`).
8. **Claims of the first submission that went further than its data**, corrected in this version after an
   outside review:
   * "conflicting transactions are ordered by priority fee in every client" — JitoLabs never separated price from
     arrival; now five of six, with JitoLabs stated as not shown;
   * "strict: 100%" for AgaveBam conflict came from 2 blocks — cells with fewer than 4 blocks are now not judged
     (the cell now has 5 blocks and is still 100%);
   * "no rule detected: ordering indistinguishable from random" — replaced by the effect each cell could have
     detected; a null result here is a bound, not proof of randomness;
   * burst probes were described as "≈0.3 ms apart" (README: "≈0.5 ms span") — it is the whole batch that spans
     ≈0.3 ms, ≈0.1 ms between probes;
   * the README said late probes were "excluded from the in-window counts" — probes that landed in a *later window
     of the same validator* (20 in run 2) were counted; the "only probes inside the planned window" check now
     shows the verdicts without them;
   * the report said refused probes were "never counted as sent" — they were, in the sent columns;
   * the spend cap was per run and did not reserve sweep fees — both fixed (see Spend);
   * the README attributed ≈0.21 s slots to Alpenglow — slot time had been falling gradually since August; the
     cutover is now identified from vote transactions instead.
9. **Public RPC limits.** `getBlock` on the public endpoint returns 429 under bursts; collection backs off
   exponentially and waits 600 ms between blocks. For run 3 one `collect` pass aborted on a network error of
   `getSignatureStatuses`; collection is idempotent (it skips probes already located) and the rerun completed.
10. **Docker was not run on the measuring machine** (no Docker there). The image is built, the dry-run planner is
   run against mainnet, and the published statistics are re-derived and compared, in GitHub Actions
   (`.github/workflows/ci.yml`).

## Limits of these conclusions

* Two sessions five days apart (one evening, one afternoon; epochs 1065 and 1069), about 1 h 45 min of sending.
  Load and validator configuration change; the rule could too. The per-run table shows the signs held across the
  three runs, but three runs are not a time series.
* "Arrival" means the order in which our machine finished writing the streams. Arrival at the scheduler can
  differ (network, QUIC stream handling, any builder or relay in front of the node).
* Only one stagger gap (30 ms) was measured, so where between ≈0.1 ms and 30 ms each client switches from price to
  arrival order is not known.
* The probes are tiny and cheap (≤ 100 000 micro-lamports/CU). The rule among high-value transactions or under
  Jito bundles/tips is out of scope.
* "No rule detected" is bounded by the detectable effect printed for each cell (|tau| 0.29–0.48 for the
  no-rule cells of the main table), not proof of
  random ordering.

## Verifying a claim yourself

Every located probe has its slot and index in `data/landed.jsonl`. Example (the last probe of live run 2):

```bash
curl -s https://api.mainnet-beta.solana.com -H 'Content-Type: application/json' -d \
  '{"jsonrpc":"2.0","id":1,"method":"getBlock","params":[460214962,{"maxSupportedTransactionVersion":1,"transactionDetails":"signatures","rewards":false}]}' \
  | jq -r '.result.signatures[304]'
# 4FHcA14LrFtyRHGvKkFBhbyYj1632uq7XnhuVqqqhhB1J7NBZCRQKg1MxPMctcWVJiqZDY9tAVjWYXFkKvjZ1zLr
```
