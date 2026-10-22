# Inside the Block — how Solana leaders order transactions

A measurement tool for Solana **mainnet**: it sends its own probe transactions into the leader windows of chosen
validators and reads the produced blocks to answer, per validator and per client: *by what rule did this
leader order the transactions inside its block, how strictly does the rule hold, and is there a rule at all?*

All measurements in `data/` were taken **after the Alpenglow upgrade of 21 Oct 2026** (see `REPORT.md` for dates
and slots). Nothing before it is mixed in.

## One command

```bash
docker compose up --build          # dry run: plan, follow the real leader schedule, build + sign probes, send nothing
```

Live measurement (spends at most `--max-spend-sol`, default **0.05 SOL**, in fees):

```bash
docker compose run --rm itb keygen                                   # prints the experiment wallet address
# fund that address with ~0.013 SOL for the defaults (8 probes x 72 windows); most of it is
# rent for the sub-payers and comes back at the end — the fees actually spent are far smaller (see REPORT.md)
docker compose run --rm itb pipeline --live --max-spend-sol 0.05     # plan -> run -> collect -> analyze
```

Keys only ever enter through the `./keys` volume (`KEYPAIR=/app/keys/payer.json`); `keys/` is git-ignored and
nothing secret is baked into the image. Without `--live` the binary never writes a transaction to the network.
New runs write to `./work`, so they never touch the published measurements in `./data`.

Re-derive every published statistic from the published raw data (no network, deterministic seed):

```bash
docker compose run --rm reanalyze    # rewrites data/RESULTS.md + data/results.json; CI checks they are unchanged
```

Subcommands: `plan`, `run [--live]`, `collect`, `analyze`, `pipeline`, `tpu-check` (QUIC handshake only),
`keygen`, `sweep` (recover sub-payer funds after an interrupted run). `itb --help` lists every option.

## The experiment

For each selected validator, the tool takes several of its upcoming **4-slot leader windows** and fires one
**batch of probes** into each window (8 by default, `--batch-size`; the published runs used 4 to fit the wallet,
see `REPORT.md`):

* every probe has the **same shape and the same requested compute-unit limit** (30 000 CU: compute-budget
  instructions plus a memo naming the batch, the send position and the price), so the probes differ only in
  their **compute-unit price** and in **when they were written**;
* prices are a **random permutation** of 8 levels (0 … 100 000 micro-lamports/CU) and the send order is an
  **independent random permutation**, so "price" and "arrival" are never confounded by design;
* each probe of a batch has **its own fee payer** (8 funded sub-payers), so the probes share no writable
  account and the leader is free to order them any way it likes.

Three batch modes rotate over each validator's windows:

| mode | what is sent | what it isolates |
|---|---|---|
| `burst` | all 8 probes back to back on one QUIC connection (≈0.5 ms span) | the scheduler's own priority rule — arrival differences are tiny |
| `stagger` | 30 ms between probes, order independent of price | whether arrival time dominates when arrivals are clearly separated |
| `conflict` | like burst, but every probe also writes one shared account | how *conflicting* transactions are sequenced (they cannot run in parallel) |

**Evidence = only our own probes, only in the targeted leader's own blocks.** The unit of evidence is a *block
group*: the probes of one batch that landed in the same block **produced by the validator the batch was aimed
at** (a probe that missed the window and was included by the next leader is counted as "landed elsewhere" and
says nothing about the targeted leader). For every pair inside a group three questions are asked:

* **price** — did the higher-priced probe come earlier?
* **arrival** — did the earlier-written probe come earlier?
* **head-to-head** — only the pairs where the two rules *disagree* (the higher-priced probe was written later):
  which one won? Price and send order are independent permutations, but with 4–8 probes a single batch can
  correlate them by chance; these pairs are the ones that actually tell the two rules apart.

Each is summarised as Kendall's tau (+1 always agrees, 0 unrelated, −1 always opposite) with a **permutation
p-value** that shuffles positions *within* each block, because pairs from one block are not independent. The
p-values are corrected for multiple testing (**Benjamini–Hochberg**, separately for the per-validator, per-client
and per-client-and-mode tables, which answer different questions), and a rule is claimed only at **q < 0.05 with
at least 10 pairs**. "No rule detected" is reported as such otherwise — with a small sample that is a statement
about the evidence, not proof that the leader orders randomly.

## What the design has to account for — and how it does

**Leader schedule and leader types.** Windows come from `getLeaderSchedule` of the current epoch; a window is a
run of consecutive slots of one identity (4 slots; occasionally 8 or 12 when a leader repeats). The client of each
validator is taken from the `clientId` it advertises in gossip (`getClusterNodes`) — `Agave`, `JitoLabs`,
`AgaveBam`, `HarmonicAgave`, `Firedancer`, `HarmonicFiredancer`, `FireBAM`, `Rakurai`, … — and saved with the
version string in `data/plan.json`, so the label can be checked against the same source. By default the plan
picks 2 validators from each of 6 client types (12 validators), preferring those advertising a QUIC TPU.

**The path from our backend to the executing node.** Probes are written **directly to the leader's advertised
QUIC TPU** (`tpuQuic` in gossip) on one connection opened a few slots ahead, one unidirectional stream per
probe, in the chosen order — no RPC node, no forwarding, no retries. The send timestamp is taken when each stream
is finished. This is still not the leader's scheduler: what sits behind the advertised address differs by client
(see `tpu-check`: Harmonic validators advertise ports on shared builder ingress hosts; BAM and Jito validators
route through their own pipelines). Every record keeps the address used and the path (`quic-direct`, or `rpc`
if the handshake failed and the probe went through `sendTransaction`), so results can be split by path.

**Timing.** Post-Alpenglow slots are ≈0.21 s (60 s of samples: 276–283 slots). The tool tracks the slot through a
websocket `slotSubscribe` (with a `getSlot` poll as fallback) and fires one observed slot before the window,
because our RPC node lags the leader. The observed slot at send time and its age are stored with every probe;
batches whose observed slot is already in the second half of the window are **skipped**, not sent, so fees are
not paid for probes that would measure the next leader. Probes landing after the window are still collected and
reported (they say something about the path), but excluded from the "in window" counts.

**Interaction of our own transactions with each other.** Distinct fee payers remove account conflicts in `burst`
and `stagger`; identical CU limits remove cost differences; the memo is unique per probe, so signatures are
unique. `conflict` mode deliberately reintroduces one shared writable account to see how conflicting
transactions are serialised.

**Competition inside the block.** Other users' transactions are not part of the evidence: the tests compare only
our probes *relative to each other* inside the same block. Block size and every probe's absolute index are still
recorded (`data/blocks/<slot>.json` keeps the full ordered list of signatures, fees and compute units), and the
median relative position of our probes is reported as context.

**Send order.** Randomised independently of price in every batch; `stagger` makes arrival differences large on
purpose, `burst` makes them small on purpose.

**Noise versus rule.** Several windows per validator, several pairs per window, a permutation test per test and
scope, and a fixed seed (`--seed`) so the randomisation is reproducible. A small or inconsistent effect is
reported as no rule.

## What this does NOT separate

* **Arrival at the leader vs. send order.** We control and record when probes leave this machine, not when they
  reach the leader's scheduler. QUIC streams on one connection may be processed out of order, and builders or
  relays in front of the validator add their own queueing.
* **The validator's scheduler vs. whatever builds the block for it.** For `HarmonicAgave`, `HarmonicFiredancer`,
  `AgaveBam`, `FireBAM` and `JitoLabs` the block content may be assembled by an external builder or auction; the
  measurement reports the rule visible in the block, not which component applied it.
* **Parallel execution threads.** Agave's banking stage and Firedancer's pack work across several threads; an
  observed order is the order entries were recorded, which can mix the policy with thread timing.
* **Stake-weighted QoS.** Our client is unstaked, so a leader may throttle or drop our streams under load; drops
  show up as probes that never land, not as reordering.
* **Jito bundles and tips** are out of scope (no bundles or tips are sent).

## Raw data — verifying every claim

| file | content |
|---|---|
| `data/plan-*.json` | per run: epoch, planning slot, chosen validators with `clientId`, version, TPU address, stake share, windows, stake share of every client |
| `data/run-*.log` | console log of each run (funding, every batch, spend, sweep) |
| `data/sends.jsonl` | every probe: batch, mode, send position, CU price, payer, **signature**, path, send time (ns), observed slot |
| `data/batches.jsonl` | every batch: window, trigger slot, observed slot at first send, in-window flag, send span, skip reason |
| `data/landed.jsonl` | every located probe: **slot**, index in block, block size, fee, CU consumed, error flag |
| `data/blocks/<slot>.json` | each fetched block as ordered rows (index, signature, fee, CU, error), leader, parent, blockhash |
| `data/results.json`, `data/RESULTS.md` | the statistics and verdicts produced by `itb analyze` |

Any position can be checked independently with
`getBlock(<slot>, {maxSupportedTransactionVersion: 1})` — the block's `transactions[i]` must carry our signature
at the recorded index. `REPORT.md` lists the date of every measurement run and a "what went wrong" section.

## Authorship

Designed and run by Yevhenij Hlynskyi (GitHub `EvgeniuGlinsky`), with Claude Code (Anthropic) used as an AI
pair-programmer for the code and the write-up. Every number in `REPORT.md` comes from the real mainnet
transactions recorded in `data/` and can be re-checked with `getBlock`.

## Building without Docker

```bash
cargo build --release
./target/release/itb --data-dir work pipeline   # dry run (keep ./data for the published measurements)
./target/release/itb analyze                    # re-derive data/RESULTS.md from data/
cargo test                                 # encoding, spend cap, window splitting, statistics
```
