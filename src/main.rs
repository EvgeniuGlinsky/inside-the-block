//! itb — Inside the Block.
//!
//! Measures, on Solana mainnet, by what rule specific leaders order transactions in their blocks.
//! `itb pipeline` runs everything; it is a dry run unless `--live` is given.

mod analyze;
mod collect;
mod plan;
mod quic;
mod rpc;
mod run;
mod slots;
mod txs;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

#[derive(Parser)]
#[command(name = "itb", about = "Inside the Block: how Solana leaders order transactions", version)]
struct Cli {
    /// JSON-RPC endpoint (mainnet).
    #[arg(long, env = "RPC_URL", default_value = "https://api.mainnet-beta.solana.com", global = true)]
    rpc_url: String,
    /// Websocket endpoint for slot notifications.
    #[arg(long, env = "WS_URL", default_value = "wss://api.mainnet-beta.solana.com", global = true)]
    ws_url: String,
    /// Where plan, raw records, blocks and results go.
    #[arg(long, env = "DATA_DIR", default_value = "data", global = true)]
    data_dir: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pick validators and their upcoming leader windows; writes plan.json.
    Plan(PlanOpts),
    /// Send probes into the planned windows (dry-run unless --live).
    Run(RunOpts),
    /// Fetch the blocks of probed windows and locate every probe.
    Collect,
    /// Compute per-validator / per-client ordering statistics; writes results.json and RESULTS.md.
    Analyze(AnalyzeOpts),
    /// plan -> run -> collect -> analyze.
    Pipeline {
        #[command(flatten)]
        plan: PlanOpts,
        #[command(flatten)]
        run: RunOpts,
        #[command(flatten)]
        analyze: AnalyzeOpts,
    },
    /// Open (and close) a QUIC connection to each planned leader's TPU without sending anything.
    TpuCheck,
    /// Create a new keypair file for the experiment wallet and print its address (fund it, then run --live).
    Keygen {
        #[arg(long, default_value = "keys/payer.json")]
        out: String,
    },
    /// Return lamports left on sub-payers of an interrupted live run to the main wallet.
    Sweep {
        #[arg(long, env = "KEYPAIR")]
        keypair: String,
        /// subpayers-<run>.json written by the interrupted run.
        #[arg(long)]
        subpayers: String,
    },
}

#[derive(Args, Clone)]
struct PlanOpts {
    /// Client ids as advertised in gossip (getClusterNodes .clientId).
    #[arg(long, value_delimiter = ',', default_value = "AgaveBam,HarmonicAgave,JitoLabs,Firedancer,HarmonicFiredancer,Agave")]
    clients: Vec<String>,
    #[arg(long, default_value_t = 2)]
    validators_per_client: usize,
    /// Windows per validator; modes rotate over them, so use a multiple of the number of modes.
    #[arg(long, default_value_t = 6)]
    windows_per_validator: usize,
    /// Ignore windows starting sooner than this many slots from now (setup time).
    #[arg(long, default_value_t = 300)]
    warmup_slots: u64,
    /// Only consider windows within this many slots (~0.2 s per slot after Alpenglow).
    #[arg(long, default_value_t = 18_000)]
    horizon_slots: u64,
    /// Only these validator identities (comma-separated), e.g. to repeat an earlier plan's targets.
    #[arg(long, value_delimiter = ',')]
    identities: Vec<String>,
}

#[derive(Args, Clone)]
struct RunOpts {
    /// Actually send transactions. Without it nothing leaves this machine.
    #[arg(long, default_value_t = false)]
    live: bool,
    /// Funded keypair file (JSON byte array). Mount it, never commit it.
    #[arg(long, env = "KEYPAIR")]
    keypair: Option<String>,
    /// Where sub-payer keys of a live run are kept until swept back.
    #[arg(long, env = "KEYS_DIR", default_value = "keys")]
    keys_dir: PathBuf,
    /// Hard cap on fees, in SOL, for all live runs recorded in --data-dir together: earlier runs
    /// count with their actual fees, this one with its worst case.
    #[arg(long, default_value_t = 0.05)]
    max_spend_sol: f64,
    /// Accept a --max-spend-sol above the task's 0.05 SOL budget.
    #[arg(long, default_value_t = false)]
    allow_over_budget: bool,
    #[arg(long, default_value_t = 8)]
    batch_size: usize,
    #[arg(long, default_value_t = 30_000)]
    cu_limit: u32,
    /// Compute-unit prices in micro-lamports; each batch uses a random permutation of these.
    #[arg(long, value_delimiter = ',', default_value = "0,1000,2000,5000,10000,20000,50000,100000")]
    price_levels: Vec<u64>,
    #[arg(long, value_delimiter = ',', default_value = "burst,stagger,conflict")]
    modes: Vec<String>,
    #[arg(long, default_value_t = 30)]
    stagger_ms: u64,
    /// Fire this many observed slots before the window starts (our RPC lags the leader).
    #[arg(long, default_value_t = 1)]
    lead_slots: u64,
    #[arg(long, default_value_t = 42)]
    seed: u64,
}

#[derive(Args, Clone)]
struct AnalyzeOpts {
    #[arg(long, default_value_t = 10_000)]
    permutations: usize,
}

fn run_args(o: &RunOpts) -> Result<run::RunArgs> {
    if o.max_spend_sol <= 0.0 {
        bail!("--max-spend-sol must be positive");
    }
    if o.max_spend_sol > 0.05 && !o.allow_over_budget {
        bail!(
            "--max-spend-sol {} is above the 0.05 SOL budget of the task (add --allow-over-budget to mean it)",
            o.max_spend_sol
        );
    }
    Ok(run::RunArgs {
        live: o.live,
        keypair: o.keypair.clone(),
        keys_dir: o.keys_dir.clone(),
        max_spend_lamports: (o.max_spend_sol * LAMPORTS_PER_SOL) as u64,
        batch_size: o.batch_size,
        cu_limit: o.cu_limit,
        price_levels: o.price_levels.clone(),
        modes: o.modes.iter().map(|m| m.parse()).collect::<Result<_>>()?,
        stagger_ms: o.stagger_ms,
        lead_slots: o.lead_slots,
        seed: o.seed,
    })
}

async fn do_plan(rpc: &rpc::Rpc, data: &PathBuf, o: &PlanOpts) -> Result<plan::Plan> {
    let p = plan::make_plan(
        rpc,
        &plan::PlanArgs {
            clients: o.clients.clone(),
            validators_per_client: o.validators_per_client,
            windows_per_validator: o.windows_per_validator,
            warmup_slots: o.warmup_slots,
            horizon_slots: o.horizon_slots,
            identities: o.identities.clone(),
        },
    )
    .await?;
    std::fs::write(data.join("plan.json"), serde_json::to_string_pretty(&p)?)?;
    println!("plan: {} validators, epoch {}, planned at slot {}", p.targets.len(), p.epoch, p.planned_at_slot);
    for t in &p.targets {
        println!(
            "  {:<18} {:<14} {} stake {:.3}% windows {:?}",
            t.client_id,
            t.version,
            t.identity,
            100.0 * t.stake_share,
            t.windows
        );
    }
    Ok(p)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    std::fs::create_dir_all(&cli.data_dir).with_context(|| format!("create {}", cli.data_dir.display()))?;
    let rpc = rpc::Rpc::new(&cli.rpc_url);
    match &cli.cmd {
        Cmd::Plan(o) => {
            do_plan(&rpc, &cli.data_dir, o).await?;
        }
        Cmd::Run(o) => {
            let p: plan::Plan = serde_json::from_str(&std::fs::read_to_string(cli.data_dir.join("plan.json"))?)?;
            let id = run::run(&rpc, &cli.ws_url, &p, &run_args(o)?, &cli.data_dir).await?;
            println!("run {id} finished");
        }
        Cmd::Collect => collect::collect(&rpc, &cli.data_dir).await?,
        Cmd::Analyze(o) => {
            let rows = analyze::analyze(&cli.data_dir, o.permutations)?;
            print!("{}", analyze::render(&rows));
        }
        Cmd::Pipeline { plan: po, run: ro, analyze: ao } => {
            let p = do_plan(&rpc, &cli.data_dir, po).await?;
            let id = run::run(&rpc, &cli.ws_url, &p, &run_args(ro)?, &cli.data_dir).await?;
            println!("run {id} finished");
            if ro.live {
                println!("waiting 30 s for blocks to be confirmed before collecting");
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
            collect::collect(&rpc, &cli.data_dir).await?;
            let rows = analyze::analyze(&cli.data_dir, ao.permutations)?;
            print!("{}", analyze::render(&rows));
        }
        Cmd::TpuCheck => {
            let p: plan::Plan = serde_json::from_str(&std::fs::read_to_string(cli.data_dir.join("plan.json"))?)?;
            let sender = quic::TpuSender::new()?;
            for t in &p.targets {
                let Some(addr) = &t.tpu_quic else {
                    println!("{:<18} {} no tpuQuic advertised", t.client_id, t.identity);
                    continue;
                };
                let started = std::time::Instant::now();
                match sender.connect(addr).await {
                    Ok(c) => {
                        println!("{:<18} {} {addr} handshake ok in {} ms", t.client_id, t.identity, started.elapsed().as_millis());
                        c.close(0u32.into(), b"check");
                    }
                    Err(e) => println!("{:<18} {} {addr} FAILED: {e}", t.client_id, t.identity),
                }
            }
        }
        Cmd::Keygen { out } => {
            if std::path::Path::new(out).exists() {
                bail!("{out} already exists; refusing to overwrite a wallet");
            }
            if let Some(dir) = std::path::Path::new(out).parent() {
                std::fs::create_dir_all(dir)?;
            }
            let kp = solana_keypair::Keypair::new();
            txs::write_keypair(out, &kp)?;
            println!("{}", solana_signer::Signer::pubkey(&kp));
        }
        Cmd::Sweep { keypair, subpayers } => {
            let main = txs::read_keypair(keypair)?;
            let raw: Vec<Vec<u8>> = serde_json::from_str(&std::fs::read_to_string(subpayers)?)?;
            let payers = raw
                .iter()
                .map(|b| solana_keypair::Keypair::try_from(b.as_slice()).map_err(|e| anyhow::anyhow!("{e}")))
                .collect::<Result<Vec<_>>>()?;
            run::sweep(&rpc, &main, &payers).await?;
        }
    }
    Ok(())
}
