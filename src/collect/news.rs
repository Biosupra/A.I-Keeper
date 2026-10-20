//! Crypto headlines from public RSS feeds (GET only, no key), kept as published: title, link,
//! time and a short summary. Tagging them against the market is a later step; capturing them now
//! means that history exists when it is needed.
//!
//! A deliberately small RSS 2.0 reader (items, CDATA, the common entities) instead of an XML
//! dependency: the feeds are regular, and the build has to stay light for a 1 GB machine.

use crate::net::Web;
use crate::store::Store;
use anyhow::Result;
use rusqlite::params;

/// (source name, feed URL). Each host is on the guard's allowlist.
pub const FEEDS: &[(&str, &str)] = &[
    ("coindesk", "https://www.coindesk.com/arc/outboundfeeds/rss/"),
    ("cointelegraph", "https://cointelegraph.com/rss"),
    ("decrypt", "https://decrypt.co/feed"),
    ("theblock", "https://www.theblock.co/rss.xml"),
    ("thedefiant", "https://thedefiant.io/api/feed"),
];

#[derive(Debug, PartialEq)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub link: Option<String>,
    pub ts: Option<i64>,
    pub summary: Option<String>,
}

fn unwrap_cdata(s: &str) -> &str {
    let t = s.trim();
    t.strip_prefix("<![CDATA[").and_then(|r| r.strip_suffix("]]>")).unwrap_or(t)
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let Some(end) = tail.find(';').filter(|e| *e <= 10) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let ent = &tail[1..end];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ => ent
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0;
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Text of the first `<tag ...>...</tag>` inside `block` (CDATA unwrapped, entities decoded).
fn tag(block: &str, name: &str) -> Option<String> {
    let open = format!("<{name}");
    let mut from = 0;
    while let Some(i) = block[from..].find(&open) {
        let start = from + i;
        let after = &block[start + open.len()..];
        // Exact tag name: the next character closes the tag or starts an attribute.
        if !after.starts_with('>') && !after.starts_with(' ') && !after.starts_with('/') {
            from = start + open.len();
            continue;
        }
        let gt = after.find('>')?;
        if after[..gt].ends_with('/') {
            return None; // <tag/>
        }
        let body = &after[gt + 1..];
        let close = body.find(&format!("</{name}>"))?;
        let text = decode_entities(unwrap_cdata(&body[..close])).trim().to_string();
        return if text.is_empty() { None } else { Some(text) };
    }
    None
}

/// The items of an RSS 2.0 document.
pub fn parse(xml: &str) -> Vec<Item> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<item") {
        let tail = &rest[i..];
        let Some(end) = tail.find("</item>") else { break };
        let block = &tail[..end];
        rest = &tail[end + 7..];
        let Some(title) = tag(block, "title").map(|t| strip_tags(&t)) else { continue };
        let link = tag(block, "link");
        let guid = tag(block, "guid");
        let ts = tag(block, "pubDate").and_then(|d| chrono::DateTime::parse_from_rfc2822(d.trim()).ok()).map(|d| d.timestamp());
        let summary =
            tag(block, "description").map(|d| strip_tags(&d)).map(|d| d.chars().take(600).collect::<String>()).filter(|d| !d.is_empty());
        let id = guid.clone().or_else(|| link.clone()).unwrap_or_else(|| title.clone());
        out.push(Item { id, title, link, ts, summary });
    }
    out
}

/// Reads every feed and stores new items. Returns (feeds read, new items).
pub async fn poll(web: &Web, store: &Store, now: i64) -> Result<(usize, usize)> {
    let (mut feeds, mut new) = (0, 0);
    for (source, url) in FEEDS {
        let (status, body) = match web.get_raw(url, &[("Accept", "application/rss+xml, application/xml, text/xml")]).await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("news {source}: {e:#}");
                continue;
            }
        };
        if !(200..300).contains(&status) {
            tracing::warn!("news {source}: HTTP {status}");
            continue;
        }
        feeds += 1;
        for it in parse(&String::from_utf8_lossy(&body)) {
            new += store.db.execute(
                "INSERT OR IGNORE INTO news (id, source, ts, title, link, summary, seen_ts) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![format!("{source}:{}", it.id), source, it.ts.unwrap_or(now), it.title, it.link, it.summary, now],
            )?;
        }
    }
    Ok((feeds, new))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shapes_the_feeds_use() {
        let xml = r#"<rss><channel><title>Feed</title>
          <item><title><![CDATA[Robinhood adds AI agents & perps]]></title>
            <link>https://www.coindesk.com/markets/a</link><guid isPermaLink="false">c7345e2a</guid>
            <pubDate>Tue, 29 Sep 2026 23:00:00 +0000</pubDate>
            <description><![CDATA[<p>Robinhood &amp; friends <b>ship</b></p>]]></description></item>
          <item><title>Aztec brings back zk.money &#8217;s privacy</title>
            <link><![CDATA[https://decrypt.co/379621/x]]></link>
            <pubDate>Wed, 30 Sep 2026 02:13:52 GMT</pubDate></item>
          <item><description>no title here</description></item>
        </channel></rss>"#;
        let items = parse(xml);
        assert_eq!(items.len(), 2, "an item without a title is skipped");
        assert_eq!(items[0].title, "Robinhood adds AI agents & perps");
        assert_eq!(items[0].id, "c7345e2a");
        assert_eq!(items[0].ts, Some(1790722800));
        assert_eq!(items[0].summary.as_deref(), Some("Robinhood & friends ship"));
        assert_eq!(items[1].title, "Aztec brings back zk.money \u{2019}s privacy");
        assert_eq!(items[1].link.as_deref(), Some("https://decrypt.co/379621/x"));
        assert_eq!(items[1].id, "https://decrypt.co/379621/x", "no guid: the link identifies it");
        assert_eq!(items[1].ts, Some(1790734432));
    }

    #[test]
    fn tag_names_are_exact() {
        // <titleX> must not be read as <title>.
        assert_eq!(tag("<titleX>no</titleX><title>yes</title>", "title").as_deref(), Some("yes"));
    }
}
