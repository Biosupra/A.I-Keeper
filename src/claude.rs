//! Claude's read of the day. Once a day the finished brief goes to Claude (Anthropic's Messages API),
//! which writes a short "what changed, what to watch" section for the top of the email and answers the
//! owner's replies. Words in, words out: the request offers no tools (`guard::check_claude_body`), so
//! nothing Claude writes can act on anything.
//!
//! Spend: one try per UTC day, recorded BEFORE the call (a brief that is retried never pays twice), and
//! a monthly dollar cap counted from Claude's own token counts (`AIK_CLAUDE_MONTHLY_USD`, default $10).
//! The Console workspace spend limit is the second fence. When the key is missing nothing changes; when
//! the cap is reached, the credits are used up or anything fails, the brief goes out without the section
//! and says why in one line.

use crate::config::ClaudeCfg;
use crate::net;
use crate::store::Store;
use anyhow::Result;
use chrono::{Datelike, TimeZone, Utc};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::path::Path;

pub const HEADING: &str = "CLAUDE'S READ";
/// The refusal fallback: if Claude's safety checks decline, the API re-runs the request on the model
/// Anthropic recommends for that case, inside the same call.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const MAX_TOKENS: u32 = 16_000;
/// Today's brief is sent whole up to this many characters; the two days before only their plain part.
const TODAY_CHARS: usize = 60_000;
const EARLIER_CHARS: usize = 8_000;
const EARLIER_DAYS: i64 = 2;
/// Output tokens assumed while no reply has come back (for the cap only).
const OUTPUT_ESTIMATE: f64 = 4_000.0;

const SYSTEM: &str = "You write one section of the Panda Financial A.I Keeper's daily email to the owner of Panda Financial. \
Panda is a small DeFi platform on Base: its vaults hold WETH/USDC liquidity positions in an Aerodrome pool, staked for AERO \
rewards, and a keeper bot moves each position's price range. The A.I Keeper is a watch-only program that collects data, \
forecasts and grades itself; the email after your section is written by its code (it calls itself \"I\"). You are Claude, \
writing a separate section that goes at the top.

Write for a busy owner who is not a statistician: plain words, short lines. Plain text only, because the email is \
text/plain: no markdown, no tables, no bold, no headings other than the ones below. Stay under 250 words.

Use exactly these headings, in this order, each followed by one to four lines that start with \"• \":
WHAT CHANGED (since yesterday's brief, when one is given)
WHAT TO WATCH
YOUR NOTES (only when today's brief has a WHAT YOU TOLD ME section: answer each note briefly; when the numbers cannot \
answer a question, say which data would)

Rules:
- Every number you state must appear in the material you are given, or be a simple difference of two numbers that do. \
If something is not in the material, say you don't know. Never present a guessed cause as fact.
- The owner's goal for the keeper is to earn as many rewards (AERO emissions) as possible while keeping losses as small \
as possible. Judge a day by rewards minus costs. Do not compare against simply holding ETH unless a note asks about it.
- A drop in a vault's dollar value is money moving (a withdrawal, a range move, ETH's price) until the numbers show a \
loss. Say which it was when the brief shows it.
- Never suggest raising the minimum range width (the width floor); the owner has decided against it.
- The NEW keeper rules (regime mode and the return wait) are being tested only. Report what the comparison shows; never \
recommend switching them on, merging them, or a date for that.
- Neither you nor the A.I Keeper can move money or change the keeper. Put anything actionable as something the owner may \
want to look at.
- When nothing notable happened, say so in one line under a heading instead of filling space.";

/// Dollars per million input and output tokens (platform.claude.com pricing, October 2026). A model not
/// listed is priced as the most expensive, so the cap errs on the safe side.
fn price(model: &str) -> (f64, f64) {
    let m = model.to_ascii_lowercase();
    if m.starts_with("claude-haiku-5-5") {
        (0.10, 0.50)
    } else if m.starts_with("claude-sonnet-5") {
        (2.0, 10.0)
    } else if m.starts_with("claude-opus-5-5") {
        (4.0, 20.0)
    } else if m.starts_with("claude-opus-5") || m.starts_with("claude-opus-4") {
        (5.0, 25.0)
    } else {
        (10.0, 50.0)
    }
}

/// Models that take the server-side refusal fallback (`fallbacks: "default"`).
fn takes_fallback(model: &str) -> bool {
    ["claude-opus-5", "claude-fable-5-1", "claude-sonnet-5-5"].iter().any(|p| model.starts_with(p))
}

fn month_start(now: i64) -> i64 {
    let d = Utc.timestamp_opt(now, 0).single().unwrap_or_else(Utc::now);
    Utc.with_ymd_and_hms(d.year(), d.month(), 1, 0, 0, 0).single().map(|t| t.timestamp()).unwrap_or(now - 31 * 86_400)
}

fn month_spend(store: &Store, now: i64) -> Result<f64> {
    Ok(store.db.query_row("SELECT COALESCE(SUM(usd), 0) FROM claude_calls WHERE ts >= ?1", [month_start(now)], |r| r.get(0))?)
}

fn clip(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// The plain part of an earlier brief (everything before the technical detail).
fn plain_part(brief: &str) -> &str {
    let cut = brief.find("\n\n=====").unwrap_or(brief.len());
    clip(&brief[..cut], EARLIER_CHARS)
}

/// The two briefs before today's, from the briefs folder (each as it was emailed, Claude's read included).
fn earlier(data_dir: &Path, now: i64) -> Vec<(String, String)> {
    (1..=EARLIER_DAYS)
        .rev()
        .filter_map(|k| {
            let d = crate::brief::day(now - k * 86_400);
            let text = std::fs::read_to_string(data_dir.join("briefs").join(format!("{d}.txt"))).ok()?;
            Some((d, plain_part(&text).to_string()))
        })
        .collect()
}

pub fn request(model: &str, today: &str, earlier: &[(String, String)]) -> Value {
    let mut user = String::new();
    for (d, text) in earlier {
        user += &format!("<earlier_brief day=\"{d}\">\n{text}\n</earlier_brief>\n\n");
    }
    user += &format!("<todays_brief>\n{}\n</todays_brief>\n\nWrite today's {HEADING} section.", clip(today, TODAY_CHARS));
    let mut body = json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "output_config": {"effort": "medium"},
        "system": SYSTEM,
        "messages": [{"role": "user", "content": user}],
    });
    if takes_fallback(model) {
        body["fallbacks"] = json!("default");
    }
    body
}

fn estimate_usd(model: &str, body: &Value) -> f64 {
    let (pin, pout) = price(model);
    body.to_string().len() as f64 / 3.5 * pin / 1e6 + OUTPUT_ESTIMATE * pout / 1e6
}

#[derive(Debug, PartialEq)]
pub struct Outcome {
    pub status: Option<u16>,
    pub text: Option<String>,
    pub note: Option<String>,
    pub input: i64,
    pub output: i64,
    pub usd: f64,
}

/// What a reply means: Claude's text and its real cost, or why there is none (failed requests are not
/// billed, so they cost nothing).
pub fn interpret(model: &str, status: u16, v: &Value) -> Outcome {
    if status != 200 {
        let msg = v["error"]["message"].as_str().unwrap_or("").to_string();
        let low = msg.to_lowercase();
        let note = if low.contains("credit balance") {
            "the API credits are used up until they refill".to_string()
        } else if low.contains("usage limit") || low.contains("spend limit") {
            "the workspace's monthly spend limit in the Claude Console was reached".to_string()
        } else {
            match status {
                401 | 403 => "the API key was refused (check the aik-anthropic-key setting)".to_string(),
                429 => "Anthropic's rate limit was hit".to_string(),
                s if s >= 500 => format!("Anthropic's service had trouble (HTTP {s})"),
                s => format!("the request was refused (HTTP {s}: {})", clip(&msg, 160)),
            }
        };
        return Outcome { status: Some(status), text: None, note: Some(note), input: 0, output: 0, usd: 0.0 };
    }
    let u = &v["usage"];
    let tok = |k: &str| u.get(k).and_then(Value::as_i64).unwrap_or(0);
    let (input, output) = (tok("input_tokens"), tok("output_tokens"));
    let (written, read) = (tok("cache_creation_input_tokens"), tok("cache_read_input_tokens"));
    // A fallback may answer on another model; charge the dearer of the two.
    let served = v.get("model").and_then(Value::as_str).unwrap_or(model);
    let ((a_in, a_out), (b_in, b_out)) = (price(model), price(served));
    let (pin, pout) = (a_in.max(b_in), a_out.max(b_out));
    let usd = (input as f64 + 1.25 * written as f64 + 0.1 * read as f64) * pin / 1e6 + output as f64 * pout / 1e6;
    let text: String = v["content"]
        .as_array()
        .map(|blocks| blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join(""))
        .unwrap_or_default();
    let text = text.trim().to_string();
    let stop = v["stop_reason"].as_str().unwrap_or("");
    let (text, note) = if stop == "refusal" {
        (None, Some("Claude declined to write it today".to_string()))
    } else if text.is_empty() {
        (None, Some("Claude's answer came back empty".to_string()))
    } else if stop == "max_tokens" {
        (Some(format!("{text}\n(cut short: Claude ran out of room)")), None)
    } else {
        (Some(text), None)
    };
    Outcome { status: Some(status), text, note, input, output, usd }
}

fn off(note: &str) -> String {
    format!("{HEADING}: none today ({note}). The numbers below are complete.\n\n")
}

/// The finished section for a day already tried (from `claude_calls`).
fn section_for(store: &Store, cfg: &ClaudeCfg, day: i64, now: i64) -> Result<Option<String>> {
    let row: Option<(Option<String>, Option<String>, f64)> = store
        .db
        .query_row("SELECT text, note, usd FROM claude_calls WHERE day = ?1", [day], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    Ok(row.map(|(text, note, usd)| match text {
        Some(t) => format!(
            "{HEADING} (written by Claude from the numbers below; if they ever disagree, the numbers are the record)\n{t}\n\
             (Claude: ${usd:.2} today, ${:.2} of this month's ${:.2} limit.)\n\n",
            month_spend(store, now).unwrap_or(0.0),
            cfg.monthly_usd
        ),
        None => off(note.as_deref().unwrap_or("asked, but no answer came back in time")),
    }))
}

/// Claude's section for today's brief: `None` when Claude is not configured, otherwise the section or a
/// one-line reason it is missing. At most one call per UTC day.
pub async fn read(cfg: Option<&ClaudeCfg>, store: &Store, data_dir: &Path, now: i64, brief: &str) -> Result<Option<String>> {
    let Some(c) = cfg else { return Ok(None) };
    let day = now.div_euclid(86_400);
    if let Some(done) = section_for(store, c, day, now)? {
        return Ok(Some(done));
    }
    let spent = month_spend(store, now)?;
    if spent >= c.monthly_usd {
        let note = format!("paused: ${spent:.2} of this month's ${:.2} limit is used; it comes back on the 1st", c.monthly_usd);
        store
            .db
            .execute("INSERT INTO claude_calls (day, ts, model, usd, note) VALUES (?1, ?2, ?3, 0, ?4)", params![day, now, c.model, note])?;
        return Ok(Some(off(&note)));
    }
    let body = request(&c.model, brief, &earlier(data_dir, now));
    store.db.execute(
        "INSERT INTO claude_calls (day, ts, model, usd, note) VALUES (?1, ?2, ?3, ?4, 'asked, but no answer came back in time')",
        params![day, now, c.model, estimate_usd(&c.model, &body)],
    )?;
    let beta = takes_fallback(&c.model).then_some(FALLBACK_BETA);
    let out = match net::Claude::new()?.messages(&c.api_key, beta, &body).await {
        Ok((status, v)) => interpret(&c.model, status, &v),
        Err(e) => {
            tracing::warn!("claude: {e:#}");
            // Sent but unanswered: keep the estimate in case Anthropic billed it.
            let usd = estimate_usd(&c.model, &body);
            Outcome { status: None, text: None, note: Some("no answer from Anthropic".into()), input: 0, output: 0, usd }
        }
    };
    tracing::info!("claude: status {:?}, {} in / {} out tokens, ${:.4}", out.status, out.input, out.output, out.usd);
    store.db.execute(
        "UPDATE claude_calls SET status = ?2, input_tokens = ?3, output_tokens = ?4, usd = ?5, text = ?6, note = ?7 WHERE day = ?1",
        params![day, out.status, out.input, out.output, out.usd, out.text, out.note],
    )?;
    section_for(store, c, day, now)
}

/// Puts the section right above THE SHORT VERSION (or on top when that heading is missing).
pub fn insert(brief: &str, section: &str) -> String {
    match brief.find("THE SHORT VERSION") {
        Some(i) => format!("{}{section}{}", &brief[..i], &brief[i..]),
        None => format!("{section}{brief}"),
    }
}

/// One line for `aikeeper selftest`.
pub fn status_line(cfg: Option<&ClaudeCfg>, store: Option<&Store>, now: i64) -> String {
    match cfg {
        None => "not configured (no AIK_ANTHROPIC_API_KEY; the brief has no Claude section)".into(),
        Some(c) => {
            let spent = store.and_then(|s| month_spend(s, now).ok()).unwrap_or(0.0);
            format!("configured: {}, ${spent:.2} of this month's ${:.2} limit used", c.model, c.monthly_usd)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(limit: f64) -> ClaudeCfg {
        ClaudeCfg { api_key: "test".into(), model: "claude-opus-5-5".into(), monthly_usd: limit }
    }

    #[test]
    fn request_offers_no_tools_and_passes_the_guard() {
        let body = request("claude-opus-5-5", "THE SHORT VERSION\n• quiet", &[("2026-10-18".into(), "yesterday".into())]);
        crate::guard::check_claude_body(crate::guard::CLAUDE_MESSAGES_URL, &body).unwrap();
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["output_config"]["effort"], "medium");
        let user = body["messages"][0]["content"].as_str().unwrap();
        assert!(user.contains("<earlier_brief day=\"2026-10-18\">") && user.contains("<todays_brief>"));
        assert!(SYSTEM.contains("width floor") && SYSTEM.contains("tested only"));
        // Haiku takes no server-side fallback.
        assert!(request("claude-haiku-5-5", "x", &[]).get("fallbacks").is_none());
        let mut with_tools = body.clone();
        with_tools["tools"] = json!([]);
        assert!(crate::guard::check_claude_body(crate::guard::CLAUDE_MESSAGES_URL, &with_tools).is_err());
        assert!(crate::guard::check_claude_body("https://api.anthropic.com/v1/files", &body).is_err());
    }

    #[test]
    fn replies_become_text_and_dollars_or_a_reason() {
        let ok = json!({"model": "claude-opus-5-5", "stop_reason": "end_turn",
            "content": [{"type": "thinking", "thinking": ""}, {"type": "text", "text": " WHAT CHANGED\n• x "}],
            "usage": {"input_tokens": 20_000, "output_tokens": 3_000}});
        let o = interpret("claude-opus-5-5", 200, &ok);
        assert_eq!(o.text.as_deref(), Some("WHAT CHANGED\n• x"));
        assert!((o.usd - (20_000.0 * 4.0 + 3_000.0 * 20.0) / 1e6).abs() < 1e-9, "{}", o.usd);
        // Served by a dearer fallback model: charged at its price.
        let fell = json!({"model": "claude-opus-4-8", "stop_reason": "end_turn", "content": [{"type": "text", "text": "a"}],
            "usage": {"input_tokens": 1_000_000, "output_tokens": 0}});
        assert!((interpret("claude-opus-5-5", 200, &fell).usd - 5.0).abs() < 1e-9);
        let refused = json!({"stop_reason": "refusal", "content": [], "usage": {"input_tokens": 10, "output_tokens": 0}});
        assert!(interpret("claude-opus-5-5", 200, &refused).note.unwrap().contains("declined"));
        let broke = json!({"type": "error", "error": {"type": "invalid_request_error",
            "message": "Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."}});
        let o = interpret("claude-opus-5-5", 400, &broke);
        assert_eq!(o.usd, 0.0);
        assert!(o.note.unwrap().contains("credits are used up"));
        assert!(interpret("claude-opus-5-5", 401, &json!({})).note.unwrap().contains("key was refused"));
        assert!(interpret("claude-opus-5-5", 529, &json!({})).note.unwrap().contains("HTTP 529"));
    }

    #[tokio::test]
    async fn one_call_a_day_and_the_monthly_cap() {
        let s = Store::memory().unwrap();
        let dir = std::env::temp_dir();
        let now = 1_792_400_000; // 2026-10-19
        assert_eq!(read(None, &s, &dir, now, "b").await.unwrap(), None, "no key: no section, nothing stored");
        // Over the cap: no request, a one-line reason, recorded so the day is settled.
        s.db.execute("INSERT INTO claude_calls (day, ts, model, usd) VALUES (1, ?1, 'm', 10.5)", [now - 86_400]).unwrap();
        let sec = read(Some(&cfg(10.0)), &s, &dir, now, "b").await.unwrap().unwrap();
        assert!(sec.starts_with("CLAUDE'S READ: none today (paused: $10.50"), "{sec}");
        // A day already answered is served from the table, not asked again.
        s.db.execute(
            "INSERT INTO claude_calls (day, ts, model, usd, text) VALUES (?1, ?2, 'm', 0.2, 'WHAT CHANGED\n• quiet')",
            params![(now + 86_400) / 86_400, now + 86_400],
        )
        .unwrap();
        let sec = read(Some(&cfg(100.0)), &s, &dir, now + 86_400, "b").await.unwrap().unwrap();
        assert!(sec.contains("• quiet") && sec.contains("$0.20 today") && sec.contains("$10.70 of this month's $100.00 limit"), "{sec}");
        assert_eq!(s.count("SELECT COUNT(*) FROM claude_calls", &[]).unwrap(), 3);
    }

    #[test]
    fn the_section_goes_above_the_short_version() {
        let b = "Hi!\n(I only watch.)\n\nTHE SHORT VERSION\n• ok\n";
        let out = insert(b, "CLAUDE'S READ\n• x\n\n");
        assert!(out.find("CLAUDE'S READ").unwrap() < out.find("THE SHORT VERSION").unwrap());
        assert!(out.starts_with("Hi!"));
        assert_eq!(plain_part("a\n\n=====\nTECHNICAL"), "a");
        assert_eq!(month_start(1_792_400_000), 1_790_812_800); // 2026-10-01 00:00 UTC
    }
}
