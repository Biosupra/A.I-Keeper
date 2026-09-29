//! Panda Financial A.I Keeper — watch-only.
//!
//! It reads the live keeper's history, the Panda pool on Base, ETH market data and the Panda
//! website; forecasts volatility and range exits; shadows every width decision the keeper makes;
//! grades itself on what really happened; and emails a daily brief. It holds no key, calls no
//! write method and cannot change anything anywhere (see `guard.rs` and `docs/GUARDRAILS.md`).

mod brief;
mod chainmath;
mod collect;
mod config;
mod guard;
mod model;
mod net;
mod registry;
mod run;
mod store;

use anyhow::Result;
use clap::{Parser, Subcommand};
use collect::{bucket, chain, market, site};

#[derive(Parser)]
#[command(name = "aikeeper", version, about = "Panda Financial A.I Keeper (watch-only)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run 24/7: collect, forecast, shadow, grade and send the daily brief.
    Run,
    /// Check the guardrails and read once from every source. Writes nothing outside the data dir.
    Selftest,
    /// Fill history: a year of ETH minutes, DVOL, and the keeper's bucket since its first day.
    Backfill {
        #[arg(long, default_value_t = 365)]
        days: i64,
        /// Skip the keeper bucket (e.g. on a laptop without access).
        #[arg(long)]
        no_bucket: bool,
    },
    /// Make this hour's forecasts and shadow rows now.
    Predict,
    /// Grade everything that is due.
    Resolve,
    /// Print the scorecard.
    Scorecard {
        #[arg(long, default_value_t = 30)]
        days: i64,
    },
    /// Build today's brief; `--send` emails it (otherwise it is printed and written to disk).
    Brief {
        #[arg(long)]
        send: bool,
    },
    /// Tell the A.I something (kept with the day's decisions and shown in the brief).
    Note { text: Vec<String> },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    // First thing, every time: refuse to run with anything that could sign a transaction.
    guard::check_environment(std::env::vars())?;
    let cfg = config::Config::from_env();
    let t = run::now();
    match Cli::parse().cmd {
        Cmd::Run => run::daemon(cfg).await,
        Cmd::Selftest => selftest(&cfg, t).await,
        Cmd::Backfill { days, no_bucket } => backfill(&cfg, t, days, no_bucket).await,
        Cmd::Predict => {
            let s = store::Store::open(&cfg.db_path())?;
            let m = registry::make_predictions(&s, t)?;
            let sh = registry::make_shadow(&s, t)?;
            println!("forecasts: {} volatility models, {} range-exit rows; {} new shadow decisions", m.vol_models, m.exit_rows, sh);
            Ok(())
        }
        Cmd::Resolve => {
            let s = store::Store::open(&cfg.db_path())?;
            println!("graded {}", registry::resolve(&s, t)?);
            Ok(())
        }
        Cmd::Scorecard { days } => {
            let s = store::Store::open(&cfg.db_path())?;
            let (lines, sh) = registry::scorecard(&s, t, days)?;
            println!("{:<10} {:<18} {:>6} {:>10}", "kind", "model", "n", "mean");
            for l in lines {
                println!("{:<10} {:<18} {:>6} {:>10.5}", l.kind, l.model, l.n, l.mean_score);
            }
            if let Some(s) = sh {
                println!(
                    "\nshadow: {} graded · keeper {:+.4}%/day · A.I (champion) {:+.4}%/day · A.I better {} · same choice {} · best-in-hindsight {:+.4}%/day",
                    s.n,
                    s.keeper_mean * 100.0,
                    s.ai_mean * 100.0,
                    s.ai_better,
                    s.same_choice,
                    s.hindsight_mean * 100.0
                );
                println!("  ({} independent days{})", s.days, if s.days < 30 { ": an early signal, not evidence" } else { "" });
                for (m, n, mean, beat) in &s.models {
                    println!("  {m:<10} n={n:<4} {:+.4}%/day  beat the keeper {beat} times", mean * 100.0);
                }
            }
            Ok(())
        }
        Cmd::Brief { send } => {
            let s = store::Store::open(&cfg.db_path())?;
            if send {
                println!("{}", brief::deliver(&cfg, &s, t, false).await?);
            } else {
                let (subject, body) = brief::build(&s, t)?;
                println!("{subject}\n\n{body}");
            }
            Ok(())
        }
        Cmd::Note { text } => {
            let s = store::Store::open(&cfg.db_path())?;
            s.db.execute("INSERT INTO notes (ts, text) VALUES (?1, ?2)", rusqlite::params![t, text.join(" ")])?;
            println!("noted");
            Ok(())
        }
    }
}

async fn selftest(cfg: &config::Config, t: i64) -> Result<()> {
    println!("guardrails:");
    for m in guard::MUST_REFUSE {
        println!("  {m:<24} {}", if guard::check_rpc_method(m).is_err() { "refused (good)" } else { "ALLOWED — STOP" });
    }
    for u in ["https://base-mainnet.g.alchemy.com/v2/x", "https://backboard.railway.app"] {
        println!("  {u:<40} {}", if guard::check_host(u).is_err() { "refused (good)" } else { "ALLOWED — STOP" });
    }
    let s = store::Store::open(&cfg.db_path())?;
    let web = net::Web::new()?;
    let rpc = net::Rpc::new(cfg.rpc_urls.clone())?;
    println!("\nsources:");
    match chain::poll(&rpc, &cfg.pool, &cfg.gauge, &s).await {
        Ok(o) => println!(
            "  Base pool      block {} tick {} (${:.2}) fee {:?} pips, base fee {:?} gwei, emissions {:?} AERO/s",
            o.block,
            o.tick,
            chainmath::tick_to_price(o.tick as f64),
            o.fee_pips,
            o.base_fee_gwei,
            o.reward_rate
        ),
        Err(e) => println!("  Base pool      FAILED: {e:#}"),
    }
    match market::coinbase_latest(&web, &s, t).await {
        Ok(n) => println!("  Coinbase       {n} one-minute candles"),
        Err(e) => println!("  Coinbase       FAILED: {e:#}"),
    }
    match market::deribit_dvol(&web, &s, t - 6 * 3600, t).await {
        Ok(n) => println!("  Deribit DVOL   {n} hourly points"),
        Err(e) => println!("  Deribit DVOL   FAILED: {e:#}"),
    }
    match site::check_all(&web, &s, &cfg.site, t).await {
        Ok(cs) => {
            for c in cs {
                println!("  site {:<34} {} {}ms {}", c.url.trim_start_matches(&cfg.site), c.status, c.ms, c.note);
            }
        }
        Err(e) => println!("  site           FAILED: {e:#}"),
    }
    match &cfg.bucket {
        Some(name) => {
            let b = bucket::Bucket { web: &web, name: name.clone(), token: cfg.gcs_token.clone() };
            let day = chrono::DateTime::from_timestamp(t, 0).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
            match b.list(&format!("v1/position_events/dt={day}/")).await {
                Ok(n) => println!("  keeper bucket  readable ({} position_events files today)", n.len()),
                Err(e) => println!("  keeper bucket  not readable here: {e:#}"),
            }
        }
        None => println!("  keeper bucket  disabled"),
    }
    println!("\nemail: {}", if cfg.email.is_some() { "configured" } else { "not configured (briefs go to the data dir)" });
    Ok(())
}

async fn backfill(cfg: &config::Config, t: i64, days: i64, no_bucket: bool) -> Result<()> {
    let s = store::Store::open(&cfg.db_path())?;
    let web = net::Web::new()?;
    println!("Coinbase: filling {days} days of one-minute ETH-USD (about {} requests)...", days * 1440 / 300);
    println!("  {} candles stored", market::coinbase_backfill(&web, &s, t, days).await?);
    println!("Deribit DVOL: {} hourly points", market::deribit_dvol(&web, &s, t - days * 86_400, t).await.unwrap_or(0));
    if !no_bucket {
        if let Some(name) = &cfg.bucket {
            let b = bucket::Bucket { web: &web, name: name.clone(), token: cfg.gcs_token.clone() };
            let days = bucket::days_between(&cfg.bucket_start, t);
            println!("Keeper bucket: {} days from {} (price history in full, the rest hourly)...", days.len(), cfg.bucket_start);
            let (o, r) = bucket::sync_days(&b, &s, &days, bucket::Mode::History).await?;
            println!("  {o} files, {r} rows");
        }
    }
    match model::har::refit(&s, t)? {
        Some(h) => println!("HAR fitted on {} days: {:?}", h.n, h),
        None => println!("HAR: not enough history yet"),
    }
    println!("Shadow decisions from history: {}", registry::make_shadow(&s, t)?);
    println!("Graded: {}", registry::resolve(&s, t)?);
    Ok(())
}
