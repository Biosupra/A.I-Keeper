//! Settings, all from environment variables (see `.env.example`). Every one has a safe default
//! except the email settings, which are optional: without them the daily brief is written to disk.

use std::path::PathBuf;

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn num<T: std::str::FromStr>(name: &str, default: T) -> T {
    var(name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

#[derive(Clone, Debug)]
pub struct Email {
    pub smtp_host: String,
    pub smtp_user: String,
    pub smtp_password: String,
    pub to: String,
}

/// Claude's read of the day (claude.rs). Off unless `AIK_ANTHROPIC_API_KEY` is set.
#[derive(Clone)]
pub struct ClaudeCfg {
    pub api_key: String,
    pub model: String,
    /// Dollars Claude may spend per calendar month (UTC), counted from its own replies. The Console
    /// workspace spend limit is the second fence.
    pub monthly_usd: f64,
}

impl std::fmt::Debug for ClaudeCfg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCfg").field("api_key", &"(set)").field("model", &self.model).field("monthly_usd", &self.monthly_usd).finish()
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub data_dir: PathBuf,
    /// Read-only Base JSON-RPC endpoints, tried in order.
    pub rpc_urls: Vec<String>,
    pub pool: String,
    pub gauge: String,
    /// `AIK_GAUGE_MODE`: auto (default: the gauge is probed once), aerodrome or aero. A value that is none of
    /// these is kept as the error it is and reported; the gauge is then not read.
    pub gauge_mode: Result<crate::aero::GaugeModeSetting, String>,
    /// `AIK_AERO_WETH_POOL`: the AERO/WETH pool that prices AERO. Unset = Aerodrome's AERO/WETH CL200 pool
    /// (`chain::AERO_WETH_POOL`), which in Aero mode prices the new AERO one for one with the old.
    pub aero_weth_pool: Option<String>,
    /// The keeper's history bucket (read-only).
    pub bucket: Option<String>,
    /// First day of keeper history to ingest (YYYY-MM-DD).
    pub bucket_start: String,
    /// For running on a laptop: a short-lived OAuth token (`gcloud auth print-access-token`).
    /// On the Google Cloud VM leave it unset; the VM's own service account is used.
    pub gcs_token: Option<String>,
    pub site: String,
    pub email: Option<Email>,
    /// UTC hour the daily brief is sent.
    pub brief_hour_utc: u32,
    pub chain_poll_secs: u64,
    pub market_poll_secs: u64,
    pub bucket_poll_secs: u64,
    pub site_poll_secs: u64,
    pub predict_every_secs: u64,
    /// Disk the database may use before the ai-disk alert (GB; the VM disk is 30 GB).
    pub disk_budget_gb: f64,
    /// First block of pool/gauge history the backfill reads (the vault's deploy block).
    pub chain_start_block: i64,
    /// The vault whose keeper the A.I follows (shadow mode, the rules check, the rule-drift alert): the live
    /// one, lower-case address, or "old" for the original vault. Default: the WETH/USDC vault of the
    /// multi-vault platform, live since 2026-10-04 (the original vault is withdraw-only since then).
    pub primary_vault: String,
    pub claude: Option<ClaudeCfg>,
    /// Token for the website's own report (GET /api/ops/report: Claude's daily site digest and weekly agent test),
    /// shown in the brief. Unset = the brief has no website section.
    pub site_report_token: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        let email = match (var("AIK_SMTP_USER"), var("AIK_SMTP_PASSWORD"), var("AIK_EMAIL_TO")) {
            (Some(user), Some(password), Some(to)) => Some(Email {
                smtp_host: var("AIK_SMTP_HOST").unwrap_or_else(|| "smtp.gmail.com".into()),
                smtp_user: user,
                smtp_password: password,
                to,
            }),
            _ => None,
        };
        Config {
            data_dir: var("AIK_DATA_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("data")),
            rpc_urls: var("AIK_RPC_URLS")
                .unwrap_or_else(|| {
                    "https://mainnet.base.org,https://base-rpc.publicnode.com,https://base.drpc.org,https://base.gateway.tenderly.co".into()
                })
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            // The live Panda deployment (verified addresses; see the dapp's src/config/contracts.ts).
            pool: var("AIK_POOL").unwrap_or_else(|| "0x3FE04A59Ebd38cF06080a6F60a98D124eb59392A".into()),
            gauge: var("AIK_GAUGE").unwrap_or_else(|| "0xA0B61fdB9f1FB9b917Fe38b49427Fd4D87472D28".into()),
            gauge_mode: var("AIK_GAUGE_MODE").unwrap_or_default().parse(),
            aero_weth_pool: var("AIK_AERO_WETH_POOL"),
            bucket: match var("AIK_BUCKET").as_deref() {
                Some("none") => None,
                Some(b) => Some(b.to_string()),
                None => Some("pfkeeperdata".into()),
            },
            bucket_start: var("AIK_BUCKET_START").unwrap_or_else(|| "2026-09-14".into()),
            gcs_token: var("AIK_GCS_TOKEN"),
            site: var("AIK_SITE").unwrap_or_else(|| "https://pandafinancial.org".into()),
            email,
            brief_hour_utc: num("AIK_BRIEF_HOUR_UTC", 12),
            chain_poll_secs: num("AIK_CHAIN_POLL_SECS", 30),
            market_poll_secs: num("AIK_MARKET_POLL_SECS", 60),
            // Every 30 minutes: every file the keeper writes is read (once) and archived.
            bucket_poll_secs: num("AIK_BUCKET_POLL_SECS", 1800),
            site_poll_secs: num("AIK_SITE_POLL_SECS", 3600),
            predict_every_secs: num("AIK_PREDICT_EVERY_SECS", 3600),
            disk_budget_gb: num("AIK_DISK_BUDGET_GB", 20.0),
            chain_start_block: num("AIK_CHAIN_START_BLOCK", 51_189_251),
            primary_vault: var("AIK_PRIMARY_VAULT").unwrap_or_else(|| crate::collect::bucket::PRIMARY_DEFAULT.into()).to_lowercase(),
            claude: var("AIK_ANTHROPIC_API_KEY").map(|api_key| ClaudeCfg {
                api_key,
                model: var("AIK_CLAUDE_MODEL").unwrap_or_else(|| "claude-opus-5-5".into()),
                monthly_usd: num("AIK_CLAUDE_MONTHLY_USD", 10.0),
            }),
            site_report_token: var("AIK_SITE_REPORT_TOKEN"),
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("aikeeper.sqlite")
    }
}
