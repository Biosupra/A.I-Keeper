//! The only ways the A.I reaches the outside world. Both go through `guard` first.
//!
//! * `Web::get*`: HTTP GET to an allowlisted host. There is deliberately no post/put/delete.
//! * `Rpc::call*`: JSON-RPC to an allowlisted Base endpoint, allowlisted read-only methods only.
//! * `Claude::messages`: text to Anthropic's Messages API at one fixed URL, never with tools
//!   (`guard::check_claude_body`), so what comes back can only be words for the brief.

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

/// Anthropic's Messages API, for the brief's "Claude's read" section (see claude.rs).
#[derive(Clone)]
pub struct Claude {
    client: reqwest::Client,
}

impl Claude {
    pub fn new() -> Result<Self> {
        // A medium-effort answer about one brief takes well under a minute; four is the ceiling.
        let client =
            reqwest::Client::builder().timeout(Duration::from_secs(240)).user_agent("PandaFinancial-AIKeeper/0.1 (watch-only)").build()?;
        Ok(Claude { client })
    }

    /// One request, no retries. Returns the HTTP status and the JSON body (Null when it is not JSON).
    pub async fn messages(&self, api_key: &str, beta: Option<&str>, body: &Value) -> Result<(u16, Value)> {
        guard::check_claude_body(guard::CLAUDE_MESSAGES_URL, body)?;
        let mut req = self.client.post(guard::CLAUDE_MESSAGES_URL).header("x-api-key", api_key).header("anthropic-version", "2023-06-01");
        if let Some(b) = beta {
            req = req.header("anthropic-beta", b);
        }
        let resp = req.json(body).send().await.context("POST to the Claude API")?;
        let status = resp.status().as_u16();
        Ok((status, resp.json::<Value>().await.unwrap_or(Value::Null)))
    }
}

/// A test-only stand-in for Base: answers each (method, params) in-process (see `Rpc::mock`).
#[cfg(test)]
pub type MockChain = std::sync::Arc<dyn Fn(&str, &Value) -> Result<Value> + Send + Sync>;

#[derive(Clone)]
pub struct Rpc {
    client: reqwest::Client,
    urls: Vec<String>,
    #[cfg(test)]
    mock: Option<MockChain>,
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
        Ok(Rpc {
            client,
            urls,
            #[cfg(test)]
            mock: None,
        })
    }

    /// Tests only: an Rpc whose calls are answered by `chain` instead of the network (the method allowlist
    /// still applies). Nothing in the release binary can build one.
    #[cfg(test)]
    pub fn mock(chain: MockChain) -> Self {
        Rpc { client: reqwest::Client::new(), urls: vec!["https://mainnet.base.org".into()], mock: Some(chain) }
    }

    /// One read-only call, trying each endpoint in turn.
    #[allow(dead_code)]
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let mut out = self.batch(&[(method, params)]).await?;
        out.pop().ok_or_else(|| anyhow!("empty RPC response"))?
    }

    /// Several read-only calls in one HTTP request (JSON-RPC batch); results in request order.
    /// An endpoint that answers some calls with errors (public nodes rate-limit inside a batch) is
    /// not the last word: the next endpoint is tried, and the answer with the fewest errors wins.
    pub async fn batch(&self, calls: &[(&str, Value)]) -> Result<Vec<Result<Value>>> {
        for (m, _) in calls {
            guard::check_rpc_method(m)?;
        }
        #[cfg(test)]
        if let Some(chain) = &self.mock {
            return Ok(calls.iter().map(|(m, p)| chain(m, p)).collect());
        }
        let body: Vec<Value> =
            calls.iter().enumerate().map(|(i, (m, p))| json!({"jsonrpc": "2.0", "id": i, "method": m, "params": p})).collect();
        let mut last_err = anyhow!("no endpoint answered");
        let mut best: Option<(usize, Vec<Result<Value>>)> = None;
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
                        let errors = out.iter().filter(|r| r.is_err()).count();
                        if errors == 0 {
                            return Ok(out);
                        }
                        if best.as_ref().map(|(e, _)| errors < *e).unwrap_or(true) {
                            best = Some((errors, out));
                        }
                    }
                    Ok(other) => last_err = anyhow!("{url}: unexpected reply {}", other.to_string().chars().take(200).collect::<String>()),
                    Err(e) => last_err = anyhow!("{url}: {e}"),
                },
                Ok(resp) => last_err = anyhow!("{url}: HTTP {}", resp.status()),
                Err(e) => last_err = anyhow!("{url}: {e}"),
            }
        }
        match best {
            Some((_, out)) => Ok(out),
            None => Err(last_err),
        }
    }
}
