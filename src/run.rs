//! The 24/7 loop. Every job is independent: a failure is logged and retried on its next turn, and
//! nothing any job does can reach a wallet, the live keeper or the website's write routes.

use crate::collect::{bucket, chain, market, site};
use crate::config::Config;
use crate::model::har;
use crate::net::{Rpc, Web};
use crate::registry;
use crate::store::Store;
use anyhow::Result;
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

pub async fn daemon(cfg: Config) -> Result<()> {
    let store = Store::open(&cfg.db_path())?;
    let web = Web::new()?;
    let rpc = Rpc::new(cfg.rpc_urls.clone())?;
    tracing::info!("A.I Keeper running (watch-only). Data: {}", cfg.db_path().display());

    let mut chain_t = Every::new(cfg.chain_poll_secs);
    let mut market_t = Every::new(cfg.market_poll_secs);
    let mut dvol_t = Every::new(3600);
    let mut bucket_t = Every::new(cfg.bucket_poll_secs);
    let mut site_t = Every::new(cfg.site_poll_secs);
    let mut predict_t = Every::new(cfg.predict_every_secs);
    let mut resolve_t = Every::new(600);
    let mut refit_t = Every::new(86_400);
    let mut alerts_t = Every::new(600);
    let mut inbox_t = Every::new(600);

    loop {
        let t = now();
        if chain_t.due(t) {
            if let Err(e) = chain::poll(&rpc, &cfg.pool, &cfg.gauge, &store).await {
                tracing::warn!("chain: {e:#}");
            }
        }
        if market_t.due(t) {
            if let Err(e) = market::coinbase_latest(&web, &store, t).await {
                tracing::warn!("coinbase: {e:#}");
            }
        }
        if dvol_t.due(t) {
            if let Err(e) = market::deribit_dvol(&web, &store, t - 6 * 3600, t).await {
                tracing::warn!("deribit: {e:#}");
            }
        }
        if bucket_t.due(t) {
            if let Some(name) = &cfg.bucket {
                let b = bucket::Bucket { web: &web, name: name.clone(), token: cfg.gcs_token.clone() };
                let days = bucket::days_between(
                    &chrono::DateTime::from_timestamp(t - 86_400, 0).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default(),
                    t,
                );
                match bucket::sync_days(&b, &store, &days, bucket::Mode::Lean).await {
                    Ok((o, r)) => {
                        tracing::info!("bucket: {o} files, {r} rows");
                        // A sync counts as healthy when listing worked (sync_days logs per-table errors).
                        if bucket::readable(&b).await {
                            store.meta_set("bucket_ok_ts", &t.to_string())?;
                        }
                    }
                    Err(e) => tracing::warn!("bucket: {e:#}"),
                }
            }
            match registry::make_shadow(&store, t) {
                Ok(n) if n > 0 => tracing::info!("shadow: {n} new decisions"),
                Ok(_) => {}
                Err(e) => tracing::warn!("shadow: {e:#}"),
            }
        }
        if site_t.due(t) {
            if let Err(e) = site::check_all(&web, &store, &cfg.site, t).await {
                tracing::warn!("site: {e:#}");
            }
        }
        if refit_t.due(t) {
            match har::refit(&store, t) {
                Ok(Some(h)) => tracing::info!("har refit on {} days: {:?}", h.n, h),
                Ok(None) => tracing::info!("har: not enough history yet"),
                Err(e) => tracing::warn!("har: {e:#}"),
            }
        }
        if predict_t.due(t) {
            match registry::make_predictions(&store, t) {
                Ok(m) => tracing::info!("predictions: {} vol models, {} exit rows", m.vol_models, m.exit_rows),
                Err(e) => tracing::warn!("predict: {e:#}"),
            }
        }
        if resolve_t.due(t) {
            match registry::resolve(&store, t) {
                Ok(n) if n > 0 => tracing::info!("graded {n}"),
                Ok(_) => {}
                Err(e) => tracing::warn!("resolve: {e:#}"),
            }
        }
        if alerts_t.due(t) {
            match crate::alerts::run(&cfg, &store, &web, &rpc, t).await {
                Ok(n) if n > 0 => tracing::warn!("alerts: {n} active"),
                Ok(_) => {}
                Err(e) => tracing::warn!("alerts: {e:#}"),
            }
        }
        if inbox_t.due(t) {
            if let Some(email) = &cfg.email {
                match crate::inbox::fetch(email, 3).await.and_then(|r| crate::inbox::store_replies(&store, &r)) {
                    Ok(n) if n > 0 => tracing::info!("inbox: {n} new notes from your replies"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!("inbox: {e:#}"),
                }
            }
        }
        // The brief: once a day at the configured UTC hour.
        let today = t / 86_400;
        let sent: i64 = store.meta_get("brief_day")?.and_then(|v| v.parse().ok()).unwrap_or(0);
        if sent < today && (t % 86_400) / 3600 >= cfg.brief_hour_utc as i64 {
            match crate::brief::deliver(&cfg, &store, t, false).await {
                Ok(w) => {
                    store.meta_set("brief_day", &today.to_string())?;
                    tracing::info!("brief {w}");
                }
                Err(e) => tracing::warn!("brief: {e:#}"),
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
