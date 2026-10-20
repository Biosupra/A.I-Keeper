//! Reads the owner's replies to the daily brief and keeps them as notes, so the owner can teach the
//! A.I by simply answering the email.
//!
//! Read-only by construction:
//! * the mailbox is opened with EXAMINE (IMAP's read-only select) and messages are fetched with
//!   BODY.PEEK, so nothing is marked read, moved, flagged or deleted;
//! * only messages FROM the owner's address are read, and only when Gmail itself vouches for the
//!   sender (DMARC or DKIM pass in Gmail's Authentication-Results), so a spoofed "From" is ignored;
//! * the text is stored as data (a note shown in the brief). Nothing in it is ever executed or
//!   treated as an instruction; the A.I has no way to act on it beyond storing it.
//! * the host passes `guard::check_host` like every other connection.

use crate::config::Email;
use crate::guard;
use crate::store::Store;
use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use rusqlite::params;
use std::sync::Arc;
use std::time::Duration;

pub const IMAP_HOST: &str = "imap.gmail.com";

/// A reply as the A.I will keep it.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub message_id: String,
    pub ts: i64,
    pub subject: String,
    pub text: String,
}

/// The owner's own words from a reply: everything above the quoted brief.
pub fn strip_quoted(body: &str) -> String {
    let mut out = Vec::new();
    for line in body.lines() {
        let l = line.trim();
        // Gmail: "On Tue, Sep 30, 2026 at 8:00 AM Panda A.I Keeper <...> wrote:" (can wrap onto two lines)
        if (l.starts_with("On ") && (l.ends_with("wrote:") || l.contains(" at ")) && !out.is_empty() && l.len() > 12)
            || l.starts_with("-----Original Message")
            || l.starts_with("---------- Forwarded")
            || l.starts_with("From: Panda A.I Keeper")
            || l == "Panda A.I Keeper — daily brief"
        {
            break;
        }
        if l.starts_with('>') {
            continue;
        }
        out.push(line.trim_end());
    }
    let text = out.join("\n").trim().to_string();
    text.chars().take(4000).collect()
}

/// True when Gmail's own Authentication-Results header vouches for the sender.
pub fn authenticated(raw_headers: &str) -> bool {
    let h = raw_headers.to_ascii_lowercase();
    h.lines().any(|l| l.starts_with("authentication-results:") && l.contains("mx.google.com"))
        && (h.contains("dmarc=pass") || h.contains("dkim=pass"))
}

/// Parses one raw message. None unless it is from `owner`, authenticated, and has text.
pub fn parse(raw: &[u8], owner: &str) -> Option<Reply> {
    let msg = mail_parser::MessageParser::default().parse(raw)?;
    let from = msg.from()?.first()?.address()?.to_ascii_lowercase();
    if from != owner.to_ascii_lowercase() {
        return None;
    }
    // Headers only: up to the first blank line.
    let head_end = raw.windows(4).position(|w| w == b"\r\n\r\n").or_else(|| raw.windows(2).position(|w| w == b"\n\n")).unwrap_or(raw.len());
    if !authenticated(&String::from_utf8_lossy(&raw[..head_end])) {
        return None;
    }
    let text = strip_quoted(&msg.body_text(0)?);
    if text.is_empty() {
        return None;
    }
    Some(Reply {
        message_id: msg
            .message_id()
            .map(str::to_string)
            .unwrap_or_else(|| format!("no-id-{}", msg.date().map(|d| d.to_timestamp()).unwrap_or(0))),
        ts: msg.date().map(|d| d.to_timestamp()).unwrap_or(0),
        subject: msg.subject().unwrap_or("").to_string(),
        text,
    })
}

/// Stores replies not seen before; returns how many were new.
pub fn store_replies(store: &Store, replies: &[Reply]) -> Result<usize> {
    let mut n = 0;
    for r in replies {
        let fresh = store.db.execute("INSERT OR IGNORE INTO inbox_seen (message_id, ts) VALUES (?1, ?2)", params![r.message_id, r.ts])?;
        if fresh == 1 {
            store.db.execute(
                "INSERT INTO notes (ts, text) VALUES (?1, ?2)",
                params![r.ts, format!("(email reply to \"{}\") {}", r.subject, r.text)],
            )?;
            n += 1;
        }
    }
    Ok(n)
}

/// Connects read-only, finds the owner's messages of the last `days`, and returns their replies.
pub async fn fetch(email: &Email, days: i64) -> Result<Vec<Reply>> {
    guard::check_host(&format!("https://{IMAP_HOST}"))?;
    let tcp = tokio::time::timeout(Duration::from_secs(20), tokio::net::TcpStream::connect((IMAP_HOST, 993)))
        .await
        .context("IMAP connect timed out")??;
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config =
        tokio_rustls::rustls::ClientConfig::builder_with_provider(Arc::new(tokio_rustls::rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(tokio_rustls::rustls::pki_types::ServerName::try_from(IMAP_HOST)?.to_owned(), tcp)
        .await
        .context("IMAP TLS")?;
    let mut client = async_imap::Client::new(tls);
    // The server greets first; read it before logging in.
    let _ = tokio::time::timeout(Duration::from_secs(15), client.read_response()).await;
    let mut session = client.login(&email.smtp_user, &email.smtp_password).await.map_err(|(e, _)| anyhow!("IMAP login: {e}"))?;
    session.examine("INBOX").await.context("EXAMINE INBOX")?; // read-only
    let since = (chrono::Utc::now() - chrono::Duration::days(days)).format("%d-%b-%Y").to_string();
    let query = format!("FROM \"{}\" SINCE {since}", email.to.replace('"', ""));
    let uids = session.uid_search(&query).await.context("UID SEARCH")?;
    let mut replies = Vec::new();
    if !uids.is_empty() {
        let mut list: Vec<u32> = uids.into_iter().collect();
        list.sort_unstable();
        let set = list.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
        let mut stream = session.uid_fetch(&set, "BODY.PEEK[]").await.context("UID FETCH")?;
        while let Some(item) = stream.next().await {
            if let Ok(f) = item {
                if let Some(r) = f.body().and_then(|b| parse(b, &email.to)) {
                    replies.push(r);
                }
            }
        }
    }
    let _ = session.logout().await;
    Ok(replies)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(from: &str, auth: &str, body: &str) -> Vec<u8> {
        format!(
            "Authentication-Results: mx.google.com;\r\n       dkim={auth} header.i=@gmail.com;\r\n       spf=pass;\r\n       dmarc={auth} (p=NONE) header.from=gmail.com\r\n\
             From: Owner <{from}>\r\nTo: pandafinancialorg@gmail.com\r\nSubject: Re: Panda A.I Keeper: daily brief 2026-09-30\r\n\
             Message-ID: <abc123@mail.gmail.com>\r\nDate: Tue, 30 Sep 2026 14:05:00 +0000\r\nContent-Type: text/plain; charset=UTF-8\r\n\r\n{body}"
        )
        .into_bytes()
    }

    const BODY: &str = "Too narrow today, the chop is back.\r\nWatch the Fed at 2pm.\r\n\r\nOn Tue, Sep 30, 2026 at 8:00 AM Panda A.I Keeper <pandafinancialorg@gmail.com> wrote:\r\n> Panda A.I Keeper - daily brief\r\n> MARKET\r\n";

    #[test]
    fn owner_reply_is_kept_without_the_quote() {
        let r = parse(&raw("owner@gmail.com", "pass", BODY), "Owner@Gmail.com").expect("parsed");
        assert_eq!(r.text, "Too narrow today, the chop is back.\nWatch the Fed at 2pm.");
        assert_eq!(r.message_id, "abc123@mail.gmail.com");
        assert_eq!(r.ts, 1790777100);
    }

    #[test]
    fn strangers_and_spoofs_are_ignored() {
        assert!(parse(&raw("someone@else.com", "pass", BODY), "owner@gmail.com").is_none(), "not the owner");
        assert!(parse(&raw("owner@gmail.com", "fail", BODY), "owner@gmail.com").is_none(), "spoofed: Gmail did not vouch");
    }

    #[test]
    fn stored_once() {
        let s = Store::memory().unwrap();
        let r = parse(&raw("owner@gmail.com", "pass", BODY), "owner@gmail.com").unwrap();
        assert_eq!(store_replies(&s, std::slice::from_ref(&r)).unwrap(), 1);
        assert_eq!(store_replies(&s, &[r]).unwrap(), 0, "the same message is not stored twice");
        let note: String = s.db.query_row("SELECT text FROM notes", [], |x| x.get(0)).unwrap();
        assert!(note.contains("Too narrow today") && note.contains("daily brief 2026-09-30"));
    }

    #[test]
    fn quote_markers() {
        assert_eq!(strip_quoted("ok\n> quoted\nmore"), "ok\nmore");
        assert_eq!(strip_quoted("hi\n---------- Forwarded message ---------\nold"), "hi");
    }
}
