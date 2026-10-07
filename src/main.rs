//! Panda Financial A.I Keeper — watch-only.
//!
//! It reads the live keeper's history, the Panda pool on Base, ETH market data and the Panda
//! website; forecasts volatility and range exits; shadows every width decision the keeper makes;
//! grades itself on what really happened; and emails a daily brief. It holds no key, calls no
//! write method and cannot change anything anywhere (see `guard.rs` and `docs/GUARDRAILS.md`).

mod alerts;
mod brief;
mod chainmath;
mod collect;
mod config;
mod guard;
mod inbox;
mod model;
mod net;
mod registry;
mod replay;
mod run;
mod store;

use anyhow::Result;
use clap::{Parser, Subcommand};
use collect::{bucket, chain, flow, liquidity, market, news, site};

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
    /// Fill history: three years of ETH minutes and ETH DVOL (what the replay learns from), a year
    /// of BTC, AERO and ETH-BTC minutes and BTC DVOL, Fear & Greed, every file in the keeper's
    /// bucket since its first day, and every pool/gauge event since the vault's deploy block; then
    /// replay it all. Safe to run again: whatever is already held is skipped.
    Backfill {
        #[arg(long, default_value_t = 365)]
        days: i64,
        /// Days of ETH-USD minutes and ETH DVOL: the replay's history (more storms to learn from).
        #[arg(long, default_value_t = 1095)]
        eth_days: i64,
        /// Skip the keeper bucket (e.g. on a laptop without access).
        #[arg(long)]
        no_bucket: bool,
        /// Skip the pool/gauge event history (it reads ~400 block ranges from Base's public node).
        #[arg(long)]
        no_chain: bool,
    },
    /// Replay the past walk-forward: every model's forecast for every hour, and every width and
    /// model strategy on every day, under the keeper's current rules. Only what is not replayed yet.
    Replay {
        #[arg(long, default_value_t = 1095)]
        days: i64,
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
    /// Check the A.I's copy of the keeper's rules against the keeper's own history.
    Verify,
    /// Run every alert check once and print what is wrong (sends nothing).
    Check,
    /// Read replies to the brief from the inbox (read-only) and keep them as notes.
    Inbox,
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
        Cmd::Backfill { days, eth_days, no_bucket, no_chain } => backfill(&cfg, t, days, eth_days.max(days), no_bucket, no_chain).await,
        Cmd::Replay { days } => {
            let s = store::Store::open(&cfg.db_path())?;
            println!("{}", replay::run(&s, t, days, &cfg.primary_vault)?);
            Ok(())
        }
        Cmd::Predict => {
            let s = store::Store::open(&cfg.db_path())?;
            let m = registry::make_predictions(&s, t)?;
            let sh = registry::make_shadow(&s, t, &cfg.primary_vault)?;
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
                let (subject, body) = brief::build(&s, t, &cfg.primary_vault)?;
                println!("{subject}\n\n{body}");
            }
            Ok(())
        }
        Cmd::Verify => {
            let s = store::Store::open(&cfg.db_path())?;
            let f = registry::fidelity(&s, &cfg.primary_vault)?;
            println!("keeper checked: {}", alerts::vault_name(&cfg.primary_vault));
            println!("width choices reproduced:   {} of {}", f.choices.0, f.choices.1);
            println!("out-of-range clocks within 10 min: {} of {}", f.clocks.0, f.clocks.1);
            match (&f.rules_version, f.rules_since) {
                (Some(v), Some(since)) => println!("current rules: keeper {v}.x, since {}", brief::when(since)),
                _ => println!("current rules: unknown (no versioned keeper run in the history yet)"),
            }
            println!("real rebalances under the current rules reproduced within 10 min: {} of {}", f.rebalances.0, f.rebalances.1);
            println!("their new widths reproduced (at the real position size): {} of {}", f.widths.0, f.widths.1);
            println!(
                "older keeper versions (different rules, for reference): {} of {} times, {} of {} widths",
                f.older_rebalances.0, f.older_rebalances.1, f.older_widths.0, f.older_widths.1
            );
            for n in f.notes {
                println!("  {n}");
            }
            Ok(())
        }
        Cmd::Check => {
            let s = store::Store::open(&cfg.db_path())?;
            let web = net::Web::new()?;
            let rpc = net::Rpc::new(cfg.rpc_urls.clone())?;
            let findings = alerts::evaluate(&cfg, &s, &web, &rpc, t).await?;
            if findings.is_empty() {
                println!("all checks pass");
            }
            for f in findings {
                println!("ALERT {}: {}", f.key, f.message);
            }
            Ok(())
        }
        Cmd::Inbox => {
            let Some(email) = &cfg.email else {
                println!("email is not configured");
                return Ok(());
            };
            let s = store::Store::open(&cfg.db_path())?;
            let replies = inbox::fetch(email, 7).await?;
            println!("{} replies from {} in the last 7 days", replies.len(), email.to);
            for r in &replies {
                println!("  {} | {} | {}", r.ts, r.subject, r.text.chars().take(80).collect::<String>());
            }
            println!("new notes stored: {}", inbox::store_replies(&s, &replies)?);
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
    match market::deribit_dvol_for(&web, &s, "BTC", t - 6 * 3600, t).await {
        Ok(n) => println!("  Deribit BTC    {n} hourly DVOL points"),
        Err(e) => println!("  Deribit BTC    FAILED: {e:#}"),
    }
    match market::perps(&web, &s, t).await {
        Ok(n) => println!("  Deribit perps  {n} values (funding, open interest, mark, index)"),
        Err(e) => println!("  Deribit perps  FAILED: {e:#}"),
    }
    match market::options(&web, &s, t).await {
        Ok(n) => println!("  ETH options    {n} instruments"),
        Err(e) => println!("  ETH options    FAILED: {e:#}"),
    }
    match market::book(&web, &s, t).await {
        Ok(()) => println!("  Coinbase book  spread and depth stored"),
        Err(e) => println!("  Coinbase book  FAILED: {e:#}"),
    }
    match market::pool_yields(&web, &s, t).await {
        Ok(n) => println!("  DefiLlama      {n} Aerodrome pools on Base"),
        Err(e) => println!("  DefiLlama      FAILED: {e:#}"),
    }
    match market::fear_greed(&web, &s, 2).await {
        Ok(n) => println!("  Fear & Greed   {n} days"),
        Err(e) => println!("  Fear & Greed   FAILED: {e:#}"),
    }
    match news::poll(&web, &s, t).await {
        Ok((feeds, n)) => println!("  News           {feeds} of {} feeds read, {n} new headlines", news::FEEDS.len()),
        Err(e) => println!("  News           FAILED: {e:#}"),
    }
    match flow::head(&rpc).await {
        Ok(h) => match flow::backfill(&rpc, &s, &cfg.pool, &cfg.gauge, h - 60, h - 3).await {
            Ok(x) => println!("  Pool events    {} logs ({} swaps) in the last ~2 minutes of blocks", x.logs, x.swaps),
            Err(e) => println!("  Pool events    FAILED: {e:#}"),
        },
        Err(e) => println!("  Pool events    FAILED: {e:#}"),
    }
    match liquidity::snapshot_ticks(&rpc, &s, &cfg.pool, t).await {
        Ok(n) => println!("  Liquidity map  {n} initialized ticks within ±20%"),
        Err(e) => println!("  Liquidity map  FAILED: {e:#}"),
    }
    match liquidity::snapshot_emissions(&rpc, &s, &cfg.pool, &cfg.gauge, t).await {
        Ok(n) => println!("  Emissions/vote {n} values"),
        Err(e) => println!("  Emissions/vote FAILED: {e:#}"),
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

async fn backfill(cfg: &config::Config, t: i64, days: i64, eth_days: i64, no_bucket: bool, no_chain: bool) -> Result<()> {
    let s = store::Store::open(&cfg.db_path())?;
    let web = net::Web::new()?;
    let rpc = net::Rpc::new(cfg.rpc_urls.clone())?;
    println!("Coinbase: filling {eth_days} days of one-minute ETH-USD (about {} requests)...", eth_days * 1440 / 300);
    println!("  {} candles stored", market::coinbase_backfill(&web, &s, t, eth_days).await?);
    for p in market::EXTRA_PRODUCTS {
        match market::backfill_product(&web, &s, p, t, days).await {
            Ok(n) => println!("Coinbase {p}: {n} candles stored"),
            Err(e) => println!("Coinbase {p}: {e:#}"),
        }
    }
    println!("Deribit DVOL: {} hourly points", market::deribit_dvol(&web, &s, t - eth_days * 86_400, t).await.unwrap_or(0));
    println!("Deribit BTC DVOL: {} hourly points", market::deribit_dvol_for(&web, &s, "BTC", t - days * 86_400, t).await.unwrap_or(0));
    println!("Fear & Greed: {} days", market::fear_greed(&web, &s, eth_days.clamp(1, 3650) as u32).await.unwrap_or(0));
    if !no_bucket {
        if let Some(name) = &cfg.bucket {
            let b = bucket::Bucket { web: &web, name: name.clone(), token: cfg.gcs_token.clone() };
            let days = bucket::days_between(&cfg.bucket_start, t);
            println!("Keeper bucket: {} days from {} (every file archived and decoded)...", days.len(), cfg.bucket_start);
            let sources = bucket::sources(&b).await?;
            println!("  keepers: {}", sources.iter().map(|x| alerts::vault_name(&x.vault)).collect::<Vec<_>>().join(", "));
            let (o, r) = bucket::sync_days(&b, &s, &sources, &days, bucket::Mode::History).await?;
            for x in sources.iter().filter(|x| x.vault != bucket::OLD) {
                s.meta_set(&format!("bucket_backfilled:{}", x.vault), &t.to_string())?;
            }
            println!("  {o} files, {r} rows");
        }
    }
    if !no_chain {
        let head = flow::head(&rpc).await? - 3;
        println!("Pool and gauge events: blocks {}..{} (every range not yet archived)...", cfg.chain_start_block, head);
        match flow::backfill(&rpc, &s, &cfg.pool, &cfg.gauge, cfg.chain_start_block, head).await {
            Ok(x) => println!("  {} logs, {} swaps", x.logs, x.swaps),
            Err(e) => println!("  stopped early: {e:#}"),
        }
    }
    // Take the snapshots now so the first brief has them.
    let _ = market::pool_yields(&web, &s, t).await.map(|n| println!("DefiLlama: {n} Aerodrome pools"));
    let _ = market::options(&web, &s, t).await.map(|n| println!("ETH options: {n} instruments"));
    let _ = liquidity::snapshot_ticks(&rpc, &s, &cfg.pool, t).await.map(|n| println!("Liquidity map: {n} ticks"));
    let _ = liquidity::snapshot_emissions(&rpc, &s, &cfg.pool, &cfg.gauge, t).await.map(|n| println!("Emissions/vote: {n} values"));
    let _ = news::poll(&web, &s, t).await.map(|(f, n)| println!("News: {n} headlines from {f} feeds"));
    match model::har::refit(&s, t)? {
        Some(h) => println!("HAR fitted on {} days: {:?}", h.n, h),
        None => println!("HAR: not enough history yet"),
    }
    // The past replayed, so the champion and the brief learn from the bad days too.
    match replay::run(&s, t, eth_days, &cfg.primary_vault) {
        Ok(r) => println!("Replay: {r}"),
        Err(e) => println!("Replay: {e:#}"),
    }
    println!("Shadow decisions from history: {}", registry::make_shadow(&s, t, &cfg.primary_vault)?);
    println!("Graded: {}", registry::resolve(&s, t)?);
    Ok(())
}
