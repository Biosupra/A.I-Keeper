//! The only ways the A.I reaches the outside world. Both go through `guard` first.
//!
//! * `Web::get*`: HTTP GET to an allowlisted host. There is deliberately no post/put/delete.
//! * `Rpc::call*`: JSON-RPC to an allowlisted Base endpoint, allowlisted read-only methods only.

use crate::guard;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
pub struct Web {
    client: reqwest::Client,
}

impl Web {
    pub fn new() -> Result<Self> {
        let client =
            reqwest::Client::builder().timeout(Duration::from_secs(30)).user_agent("PandaFinancial-AIKeeper/0.1 (watch-only)").build()?;
        Ok(Web { client })
    }

    /// GET a URL and return the response body and status. Refuses hosts not on the allowlist.
    pub async fn get_raw(&self, url: &str, headers: &[(&str, &str)]) -> Result<(u16, Vec<u8>)> {
        guard::check_host(url)?;
        let mut req = self.client.get(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.send().await.with_context(|| format!("GET {}", redact(url)))?;
        let status = resp.status().as_u16();
        let body = resp.bytes().await?.to_vec();
        Ok((status, body))
    }

    /// GET and parse JSON; a non-2xx status is an error carrying the start of the body.
    pub async fn get_json(&self, url: &str, headers: &[(&str, &str)]) -> Result<Value> {
        let (status, body) = self.get_raw(url, headers).await?;
        if !(200..300).contains(&status) {
            bail!("GET {} -> HTTP {}: {}", redact(url), status, String::from_utf8_lossy(&body[..body.len().min(300)]));
        }
        Ok(serde_json::from_slice(&body)?)
    }
}

/// URLs are logged without their query string (tokens never travel in queries here, but be safe).
pub fn redact(url: &str) -> String {
    url.split('?').next().unwrap_or(url).to_string()
}

#[derive(Clone)]
pub struct Rpc {
    client: reqwest::Client,
    urls: Vec<String>,
}

impl Rpc {
    pub fn new(urls: Vec<String>) -> Result<Self> {
        if urls.is_empty() {
            bail!("no RPC URLs configured");
        }
        for u in &urls {
            guard::check_host(u)?;
        }
        let client =
            reqwest::Client::builder().timeout(Duration::from_secs(20)).user_agent("PandaFinancial-AIKeeper/0.1 (watch-only)").build()?;
        Ok(Rpc { client, urls })
    }

    /// One read-only call, trying each endpoint in turn.
    #[allow(dead_code)]
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let mut out = self.batch(&[(method, params)]).await?;
        out.pop().ok_or_else(|| anyhow!("empty RPC response"))?
    }

    /// Several read-only calls in one HTTP request (JSON-RPC batch); results in request order.
    pub async fn batch(&self, calls: &[(&str, Value)]) -> Result<Vec<Result<Value>>> {
        for (m, _) in calls {
            guard::check_rpc_method(m)?;
        }
        let body: Vec<Value> =
            calls.iter().enumerate().map(|(i, (m, p))| json!({"jsonrpc": "2.0", "id": i, "method": m, "params": p})).collect();
        let mut last_err = anyhow!("no endpoint answered");
        for url in &self.urls {
            guard::check_host(url)?;
            match self.client.post(url).json(&body).send().await {
                Ok(resp) if resp.status().is_success() => match resp.json::<Value>().await {
                    Ok(Value::Array(items)) => {
                        let mut out: Vec<Result<Value>> = (0..calls.len()).map(|_| Err(anyhow!("missing in batch reply"))).collect();
                        for it in items {
                            let id = it.get("id").and_then(Value::as_u64).unwrap_or(u64::MAX) as usize;
                            if id < out.len() {
                                out[id] = match (it.get("result"), it.get("error")) {
                                    (_, Some(e)) if !e.is_null() => Err(anyhow!("RPC error: {e}")),
                                    (Some(r), _) => Ok(r.clone()),
                                    _ => Err(anyhow!("RPC reply without result")),
                                };
                            }
                        }
                        return Ok(out);
                    }
                    Ok(other) => last_err = anyhow!("{url}: unexpected reply {}", other.to_string().chars().take(200).collect::<String>()),
                    Err(e) => last_err = anyhow!("{url}: {e}"),
                },
                Ok(resp) => last_err = anyhow!("{url}: HTTP {}", resp.status()),
                Err(e) => last_err = anyhow!("{url}: {e}"),
            }
        }
        Err(last_err)
    }
}
