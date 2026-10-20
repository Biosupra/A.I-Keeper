//! The 24/7 loop. Every job is independent: a failure is logged and retried on its next turn, and
//! nothing any job does can reach a wallet, the live keeper or the website's write routes.
//!
//! No job can stall the loop: each network job runs under a hard time limit (`bounded`), and the
//! keeper-bucket sync, which takes minutes, runs on its own thread with its own database connection.
//! Every job writes to the database only after its network waits, so a job cut off at its limit
//! leaves nothing half-written.

use crate::collect::{bucket, chain, flow, gauge, liquidity, market, news, site};
use crate::config::Config;
use crate::model::har;
use crate::net::{Rpc, Web};
use crate::registry;
use crate::store::Store;
use anyhow::Result;
use std::future::Future;
use std::time::Duration;

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

struct Every {
    secs: i64,
    last: i64,
}

impl Every {
    fn new(secs: u64) -> Self {
        Every { secs: secs as i64, last: 0 }
    }
    fn due(&mut self, t: i64) -> bool {
        if t - self.last >= self.secs {
            self.last = t;
            true
        } else {
            false
        }
    }
}

fn day(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default()
}

/// Runs one job under a hard time limit. Errors and timeouts are logged; the loop always continues.
async fn bounded<T>(name: &str, secs: u64, fut: impl Future<Output = Result<T>>) -> Option<T> {
    tracing::debug!("job {name}");
    match tokio::time::timeout(Duration::from_secs(secs), fut).await {
        Ok(Ok(v)) => Some(v),
        Ok(Err(e)) => {
            tracing::warn!("{name}: {e:#}");
            None
        }
        Err(_) => {
            tracing::warn!("{name}: gave up after {secs} s (it will run again on its next turn)");
            None
        }
    }
}

/// One keeper-bucket sync (every file archived and decoded) plus the shadow decisions it enables.
/// Runs on its own thread with its own database connection: reading a day of files takes minutes
/// and must never stall the 30-second pool and event reads (SQLite in WAL mode takes both writers).
fn bucket_job(cfg: &Config, web: &Web, t: i64, daily: bool) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let store = Store::open(&cfg.db_path())?;
        if let Some(name) = &cfg.bucket {
            let b = bucket::Bucket { web, name: name.clone(), token: cfg.gcs_token.clone() };
            // Today; yesterday too for the first 3 hours of a day (its last files); once a day the
            // last 3 days, so a gap from a restart fills itself.
            let from = if daily {
                t - 3 * 86_400
            } else if t % 86_400 < 3 * 3600 {
                t - 86_400
            } else {
                t
            };
            let days = bucket::days_between(&day(from), t);
            // Every keeper in the bucket: the original vault's and one per vault of the multi-vault platform
            // (found by listing their folders, so a new vault needs no configuration).
            let sources = match bucket::sources(&b).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("bucket: could not list the vault folders, reading the original vault only: {e:#}");
                    vec![bucket::Source::old()]
                }
            };
            // A vault seen for the first time: its whole history since AIK_BUCKET_START, once.
            for src in sources.iter().filter(|s| s.vault != bucket::OLD) {
                let key = format!("bucket_backfilled:{}", src.vault);
                if store.meta_get(&key)?.is_none() {
                    let all = bucket::days_between(&cfg.bucket_start, t);
                    match bucket::sync_days(&b, &store, std::slice::from_ref(src), &all, bucket::Mode::Lean).await {
                        Ok((o, r)) => {
                            tracing::info!("bucket: vault {} backfilled ({o} files, {r} rows)", src.vault);
                            store.meta_set(&key, &now().to_string())?;
                        }
                        Err(e) => tracing::warn!("bucket: backfill of vault {}: {e:#}", src.vault),
                    }
                }
            }
            match bucket::sync_days(&b, &store, &sources, &days, bucket::Mode::Lean).await {
                Ok((o, r)) => {
                    tracing::info!("bucket: {o} files, {r} rows ({} keepers)", sources.len());
                    // A sync counts as healthy when listing worked (sync_days logs per-table errors).
                    if bucket::readable(&b).await {
                        store.meta_set("bucket_ok_ts", &now().to_string())?;
                    }
                }
                Err(e) => tracing::warn!("bucket: {e:#}"),
            }
        }
        if daily {
            // Yesterday joins the replayed record (and any hour the daemon missed is filled).
            match crate::replay::run(&store, now(), 3, &cfg.primary_vault) {
                Ok(r) => tracing::info!("replay: {r}"),
                Err(e) => tracing::warn!("replay: {e:#}"),
            }
        }
        match registry::make_shadow(&store, now(), &cfg.primary_vault) {
            Ok(n) if n > 0 => tracing::info!("shadow: {n} new decisions"),
            Ok(_) => {}
            Err(e) => tracing::warn!("shadow: {e:#}"),
        }
        Ok(())
    })
}

pub async fn daemon(cfg: Config) -> Result<()> {
    let store = Store::open(&cfg.db_path())?;
    let web = Web::new()?;
    let rpc = Rpc::new(cfg.rpc_urls.clone())?;
    tracing::info!("A.I Keeper running (watch-only). Data: {}", cfg.db_path().display());

    let mut chain_t = Every::new(cfg.chain_poll_secs);
    let mut flow_t = Every::new(cfg.chain_poll_secs);
    let mut market_t = Every::new(cfg.market_poll_secs);
    let mut book_t = Every::new(300);
    let mut perps_t = Every::new(300);
    let mut ticks_t = Every::new(600);
    let mut news_t = Every::new(900);
    let mut dvol_t = Every::new(3600);
    let mut options_t = Every::new(3600);
    let mut emissions_t = Every::new(3600);
    let mut yields_t = Every::new(6 * 3600);
    let mut fng_t = Every::new(6 * 3600);
    let mut bucket_t = Every::new(cfg.bucket_poll_secs);
    let mut site_t = Every::new(cfg.site_poll_secs);
    let mut predict_t = Every::new(cfg.predict_every_secs);
    let mut resolve_t = Every::new(600);
    let mut refit_t = Every::new(86_400);
    let mut alerts_t = Every::new(600);
    let mut inbox_t = Every::new(600);
    let mut bucket_thread: Option<std::thread::JoinHandle<()>> = None;
    // The gauge is probed once (AIK_GAUGE_MODE); until a probe succeeds no gauge function is read. A probe that
    // could not reach Base is retried with the next pool read, one the gauge refused at most hourly.
    let mut gauge_ctx: Option<gauge::GaugeCtx> = None;
    let mut gauge_refused = false;
    let mut probe_t = Every::new(3600);

    loop {
        let t = now();
        if chain_t.due(t) {
            if gauge_ctx.is_none() && (!gauge_refused || probe_t.due(t)) {
                probe_t.last = t;
                if let Some((ctx, refused)) =
                    bounded("gauge probe", 60, async { Ok(gauge::resolve_and_record(&rpc, &cfg, &store, t).await) }).await
                {
                    (gauge_ctx, gauge_refused) = (ctx, refused);
                }
            }
            bounded("chain", 60, chain::poll(&rpc, &cfg.pool, &cfg.gauge, gauge_ctx.as_ref(), &store)).await;
        }
        if flow_t.due(t) {
            // Every pool and gauge event since the last read (catches up up to 3 ranges at a time).
            if let Some(s) = bounded("flow", 180, flow::poll(&rpc, &store, &cfg.pool, &cfg.gauge, 3)).await {
                if s.behind > flow::SPAN {
                    tracing::info!("flow: {} logs, {} swaps; {} blocks behind, catching up", s.logs, s.swaps, s.behind);
                }
            }
        }
        if market_t.due(t) {
            bounded("coinbase", 90, market::coinbase_latest(&web, &store, t)).await;
        }
        if book_t.due(t) {
            bounded("coinbase book", 60, market::book(&web, &store, t)).await;
        }
        if perps_t.due(t) {
            bounded("deribit perps", 60, market::perps(&web, &store, t)).await;
        }
        if ticks_t.due(t) {
            bounded("liquidity map", 90, liquidity::snapshot_ticks(&rpc, &store, &cfg.pool, t)).await;
        }
        if news_t.due(t) {
            if let Some((_, n)) = bounded("news", 120, news::poll(&web, &store, t)).await {
                if n > 0 {
                    tracing::info!("news: {n} new headlines");
                }
            }
        }
        if dvol_t.due(t) {
            bounded("deribit", 90, market::deribit_dvol(&web, &store, t - 6 * 3600, t)).await;
            bounded("deribit BTC", 90, market::deribit_dvol_for(&web, &store, "BTC", t - 6 * 3600, t)).await;
        }
        if options_t.due(t) {
            bounded("deribit options", 90, market::options(&web, &store, t)).await;
        }
        if emissions_t.due(t) {
            bounded("emissions", 90, liquidity::snapshot_emissions(&rpc, &store, &cfg.pool, &cfg.gauge, gauge_ctx.as_ref(), t)).await;
        }
        if yields_t.due(t) {
            bounded("defillama", 120, market::pool_yields(&web, &store, t)).await;
        }
        if fng_t.due(t) {
            bounded("fear & greed", 60, market::fear_greed(&web, &store, 2)).await;
        }
        let daily = refit_t.due(t);
        if bucket_t.due(t) || daily {
            if bucket_thread.as_ref().map(|h| h.is_finished()).unwrap_or(true) {
                let (cfg2, web2) = (cfg.clone(), web.clone());
                bucket_thread = Some(std::thread::spawn(move || {
                    if let Err(e) = bucket_job(&cfg2, &web2, t, daily) {
                        tracing::warn!("bucket job: {e:#}");
                    }
                }));
            } else {
                tracing::info!("bucket: the previous sync is still running; skipping this turn");
            }
        }
        if site_t.due(t) {
            bounded("site", 180, site::check_all(&web, &store, &cfg.site, t)).await;
        }
        if daily {
            tracing::debug!("job har");
            match har::refit(&store, t) {
                Ok(Some(h)) => tracing::info!("har refit on {} days: {:?}", h.n, h),
                Ok(None) => tracing::info!("har: not enough history yet"),
                Err(e) => tracing::warn!("har: {e:#}"),
            }
        }
        if predict_t.due(t) {
            tracing::debug!("job predict");
            match registry::make_predictions(&store, t) {
                Ok(m) => tracing::info!("predictions: {} vol models, {} exit rows", m.vol_models, m.exit_rows),
                Err(e) => tracing::warn!("predict: {e:#}"),
            }
        }
        if resolve_t.due(t) {
            tracing::debug!("job resolve");
            match registry::resolve(&store, t) {
                Ok(n) if n > 0 => tracing::info!("graded {n}"),
                Ok(_) => {}
                Err(e) => tracing::warn!("resolve: {e:#}"),
            }
        }
        if alerts_t.due(t) {
            if let Some(n) = bounded("alerts", 180, crate::alerts::run(&cfg, &store, &web, &rpc, t)).await {
                if n > 0 {
                    tracing::warn!("alerts: {n} active");
                }
            }
        }
        if inbox_t.due(t) {
            if let Some(email) = &cfg.email {
                let fetched = bounded("inbox", 120, crate::inbox::fetch(email, 3)).await;
                if let Some(replies) = fetched {
                    match crate::inbox::store_replies(&store, &replies) {
                        Ok(n) if n > 0 => tracing::info!("inbox: {n} new notes from your replies"),
                        Ok(_) => {}
                        Err(e) => tracing::warn!("inbox: {e:#}"),
                    }
                }
            }
        }
        // The brief: once a day at the configured UTC hour.
        let today = t / 86_400;
        let sent: i64 = store.meta_get("brief_day")?.and_then(|v| v.parse().ok()).unwrap_or(0);
        if sent < today && (t % 86_400) / 3600 >= cfg.brief_hour_utc as i64 {
            if let Some(w) = bounded("brief", 360, crate::brief::deliver(&cfg, &store, t, false)).await {
                store.meta_set("brief_day", &today.to_string())?;
                tracing::info!("brief {w}");
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("stopping");
                return Ok(());
            }
        }
    }
}
