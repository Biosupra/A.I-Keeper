//! The keeper-rules scoreboard: every candidate rule for choosing a range width, scored on what it
//! REALLY would have made over the next 24 hours, under the keeper's exact rules, on the real pool
//! prices, for every hour the A.I has graded (`shadow.replay`, written by `registry::grade_shadow`).
//!
//! Why this exists (owner, 2026-10-07): over two calm weeks the keeper's own scoring ranked the
//! widths in nearly reverse order of how they really did, and over three years its narrow/wide
//! switch earned nothing over a fixed ±8%. Changes to the keeper are measured here first, every
//! day, and ship only when a candidate has led for about `EVIDENCE_DAYS` independent days.
//!
//! What is exact and what is approximate: the realised result of each fixed width (`fixed` in the
//! graded row) and of the keeper's own choice (`keeper_net_per_day`) are exact replays. A candidate
//! RULE is scored by the fixed width it would have picked at that hour, held for the day: a rule
//! that would have changed width during the day is not followed through it. The candidates that
//! differ from the keeper only by their volatility input (ewma, har, dvol, blend, persist) are the
//! existing shadow models, graded by their own exact replay (`shadow_model.net_per_day`).

use crate::model::shadow;
use crate::store::Store;
use anyhow::Result;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

/// The narrowest width on offer since 2026-10-07 (MIN_HALF_WIDTH_SPACINGS=4 on both keepers, ±2%).
pub const FLOOR_SPACINGS: i64 = 4;
/// The hysteresis candidate switches width only when the table's best beats the current width's own
/// score by this much (fraction of position value per day, 0.01%): the median gap between the
/// keeper's best and second-best width is 0.004%/day, inside its simulation noise.
pub const HYSTERESIS_MIN_GAIN: f64 = 0.0001;
/// Independent days a lead must hold before it means anything.
pub const EVIDENCE_DAYS: i64 = 30;
/// The volatility models whose own choices are graded (the "keeper" model is the keeper itself).
pub const MODELS: [&str; 5] = ["ewma", "blend", "har", "dvol", "persist"];

/// One graded hour.
#[derive(Clone, Debug)]
pub struct Graded {
    pub made_ts: i64,
    /// What the keeper's own choice really made (fraction of position value per day, next 24 h).
    pub keeper_net: f64,
    /// What each fixed half-width really made over the same 24 h.
    pub fixed: BTreeMap<i64, f64>,
    /// The keeper's table at the decision: its predicted net per width.
    pub table: Option<shadow::Table>,
    /// Each volatility model's own graded choice, from its exact replay.
    pub models: HashMap<String, f64>,
}

impl Graded {
    /// The keeper's predicted net per width, from its table.
    pub fn predicted(&self) -> Vec<(i64, f64)> {
        self.table.as_ref().map(|t| t.rows.iter().map(|r| (r.h as i64, r.net)).collect()).unwrap_or_default()
    }
}

/// Every graded hour since `since`, one per hour: the live vault's row when both keepers were
/// shadowed at the same hour (same pool, same rules).
pub fn graded(store: &Store, since: i64, primary: &str) -> Result<Vec<Graded>> {
    let mut st = store.db.prepare(
        "SELECT made_ts, vault, keeper_net_per_day, replay, detail FROM shadow
         WHERE keeper_net_per_day IS NOT NULL AND replay IS NOT NULL AND made_ts > ?1 ORDER BY made_ts",
    )?;
    let rows = st.query_map([since], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, f64>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?))
    })?;
    let mut by_hour: BTreeMap<i64, (bool, Graded)> = BTreeMap::new();
    for r in rows {
        let (made, vault, net, replay, detail) = r?;
        let Ok(rep) = serde_json::from_str::<Value>(&replay) else { continue };
        let fixed: BTreeMap<i64, f64> = rep["fixed"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| Some((x.get(0)?.as_i64()?, x.get(1)?.as_f64()?))).collect())
            .unwrap_or_default();
        if fixed.is_empty() {
            continue;
        }
        let table = detail
            .and_then(|d| serde_json::from_str::<Value>(&d).ok())
            .and_then(|d| serde_json::from_value::<shadow::Table>(d["table"].clone()).ok());
        let g = Graded { made_ts: made, keeper_net: net, fixed, table, models: HashMap::new() };
        let is_primary = vault == primary;
        let hour = made / 3600;
        let replace = match by_hour.get(&hour) {
            None => true,
            Some((kept_primary, _)) => is_primary && !kept_primary,
        };
        if replace {
            by_hour.insert(hour, (is_primary, g));
        }
    }
    let mut out: Vec<Graded> = by_hour.into_values().map(|x| x.1).collect();
    let mut st = store.db.prepare("SELECT made_ts, model, net_per_day FROM shadow_model WHERE net_per_day IS NOT NULL AND made_ts > ?1")?;
    let mut models: HashMap<i64, HashMap<String, f64>> = HashMap::new();
    for r in st.query_map([since], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, f64>(2)?)))? {
        let (t, m, v) = r?;
        models.entry(t).or_default().insert(m, v);
    }
    for g in &mut out {
        if let Some(m) = models.remove(&g.made_ts) {
            g.models = m;
        }
    }
    Ok(out)
}

/// The keeper's own selection rule (keeper policy.rs `choose_width`, after the table is scored) over
/// a table's predicted nets, with widths below `floor` not on offer: the preferred width wins unless
/// a narrower width beats it by `narrow_margin` or a wider one by `widen_margin` (a wider width also
/// wins whenever the preferred one is expected to lose money and the wider one is not).
pub fn choose_with_floor(nets: &[(i64, f64)], floor: i64, p: &shadow::Policy) -> Option<i64> {
    let offered: Vec<(i64, f64)> = nets.iter().filter(|(h, n)| *h >= floor && n.is_finite()).copied().collect();
    let lo = offered.iter().map(|x| x.0).min()?;
    let hi = offered.iter().map(|x| x.0).max()?;
    let preferred = p.preferred_half_width_spacings.clamp(lo, hi);
    let pref_net = offered.iter().find(|x| x.0 == preferred).map(|x| x.1).unwrap_or(0.0);
    let qualifies = |net: f64, margin: f64| -> bool {
        if pref_net <= 0.0 {
            net > 0.0 || net - pref_net >= margin * pref_net.abs()
        } else {
            net - pref_net >= margin * pref_net
        }
    };
    let mut choice = preferred;
    let mut choice_net = pref_net;
    // Best narrower candidate (ties go to the one closest to the preferred width).
    if let Some(e) = offered.iter().filter(|x| x.0 < preferred).max_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0))) {
        if qualifies(e.1, p.narrow_margin) && e.1 > choice_net {
            choice = e.0;
            choice_net = e.1;
        }
    }
    // Best wider candidate (ties go to the one closest to the preferred width).
    if let Some(e) = offered.iter().filter(|x| x.0 > preferred).max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0))) {
        if qualifies(e.1, p.widen_margin) && e.1 > choice_net {
            choice = e.0;
        }
    }
    Some(choice)
}

/// One candidate on the scoreboard.
#[derive(Clone, Debug)]
pub struct Line {
    pub key: String,
    /// Plain words for the brief.
    pub name: String,
    pub hours: i64,
    pub days: i64,
    /// Mean realised net per day (fraction of position value).
    pub mean: f64,
    /// Mean of (candidate − keeper's own choice) over the same hours.
    pub vs_keeper: f64,
    /// 90% day-block bootstrap interval of `vs_keeper`.
    pub ci: (f64, f64),
    /// Days on which the candidate's daily mean beat the keeper's.
    pub days_ahead: i64,
}

#[derive(Clone, Debug)]
pub struct Scoreboard {
    pub hours: i64,
    pub days: i64,
    pub keeper_mean: f64,
    /// Every candidate, best realised mean first (the keeper itself and hindsight included).
    pub lines: Vec<Line>,
    /// How often `choose_with_floor` with no floor reproduced the keeper's own recorded choice: a
    /// check that the rule is ported right (hours matched, hours with a table).
    pub port_check: (i64, i64),
}

/// Candidate name in plain words.
pub fn describe(key: &str) -> String {
    if let Some(h) = key.strip_prefix("fixed:").and_then(|h| h.parse::<i64>().ok()) {
        return format!("always ±{}%", pct_of(h));
    }
    match key {
        "keeper" => "the keeper's own choices".into(),
        "floor" => format!("keeper rules, narrowest ±{}% (live since Oct 7)", pct_of(FLOOR_SPACINGS)),
        "hysteresis" => "keeper rules, switch only on a clear gain".into(),
        "ewma" => "keeper rules with EWMA volatility".into(),
        "har" => "keeper rules with HAR volatility".into(),
        "dvol" => "keeper rules with options-implied volatility".into(),
        "blend" => "keeper rules with the blended forecast".into(),
        "persist" => "keeper rules with last hour's volatility".into(),
        "hindsight" => "perfect hindsight (not achievable)".into(),
        k => k.into(),
    }
}

fn pct_of(h: i64) -> String {
    let p = h as f64 * 0.5;
    if p.fract() == 0.0 {
        format!("{p:.0}")
    } else {
        format!("{p:.1}")
    }
}

/// xorshift64*: a small deterministic generator for the bootstrap (no dependency).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Mean of (value − keeper) with a 90% day-block bootstrap interval: days are resampled, not
/// hours, because hourly decisions share most of their 24 h windows.
fn stats(obs: &[(i64, f64, f64)]) -> (f64, f64, (f64, f64), i64, i64) {
    let n = obs.len() as f64;
    let mean = obs.iter().map(|x| x.1).sum::<f64>() / n;
    let mut by_day: BTreeMap<i64, Vec<f64>> = BTreeMap::new();
    let mut keeper_by_day: BTreeMap<i64, Vec<f64>> = BTreeMap::new();
    for (day, v, k) in obs {
        by_day.entry(*day).or_default().push(v - k);
        keeper_by_day.entry(*day).or_default().push(*k);
    }
    let diffs: Vec<Vec<f64>> = by_day.values().cloned().collect();
    let vs = diffs.iter().flatten().sum::<f64>() / n;
    let days_ahead =
        by_day.iter().filter(|(d, v)| v.iter().sum::<f64>() / v.len() as f64 > 0.0 && keeper_by_day.contains_key(*d)).count() as i64;
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut boots: Vec<f64> = Vec::with_capacity(2000);
    for _ in 0..2000 {
        let (mut sum, mut cnt) = (0.0, 0usize);
        for _ in 0..diffs.len() {
            let d = &diffs[rng.below(diffs.len())];
            sum += d.iter().sum::<f64>();
            cnt += d.len();
        }
        boots.push(if cnt > 0 { sum / cnt as f64 } else { 0.0 });
    }
    boots.sort_by(|a, b| a.total_cmp(b));
    let lo = boots[(boots.len() as f64 * 0.05) as usize];
    let hi = boots[((boots.len() as f64 * 0.95) as usize).min(boots.len() - 1)];
    (mean, vs, (lo, hi), by_day.len() as i64, days_ahead)
}

/// The scoreboard over the last `days` days. None when nothing is graded yet.
pub fn scoreboard(store: &Store, now: i64, days: i64, primary: &str) -> Result<Option<Scoreboard>> {
    let since = now - days * 86_400;
    let g = graded(store, since, primary)?;
    if g.is_empty() {
        return Ok(None);
    }
    let policy = shadow::Policy::default();
    // key -> (day, value, keeper value)
    let mut series: BTreeMap<String, Vec<(i64, f64, f64)>> = BTreeMap::new();
    let mut push = |key: &str, day: i64, v: f64, k: f64| series.entry(key.to_string()).or_default().push((day, v, k));
    let mut port = (0i64, 0i64);
    let mut current: Option<(i64, i64)> = None; // (hour index, width) for the hysteresis candidate
    for x in &g {
        let day = x.made_ts / 86_400;
        let k = x.keeper_net;
        push("keeper", day, k, k);
        for (h, v) in &x.fixed {
            push(&format!("fixed:{h}"), day, *v, k);
        }
        if let Some(best) = x.fixed.values().copied().fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v)))) {
            push("hindsight", day, best, k);
        }
        for m in MODELS {
            if let Some(v) = x.models.get(m) {
                push(m, day, *v, k);
            }
        }
        let predicted = x.predicted();
        if let Some(t) = &x.table {
            port.1 += 1;
            let lo = predicted.iter().map(|p| p.0).min().unwrap_or(1);
            if choose_with_floor(&predicted, lo, &policy) == Some(t.choice) {
                port.0 += 1;
            }
        }
        if let Some(c) = choose_with_floor(&predicted, FLOOR_SPACINGS, &policy) {
            if let Some(v) = x.fixed.get(&c) {
                push("floor", day, *v, k);
            }
            // Hysteresis: keep the current width unless the table's best beats it by a clear gain.
            let hour = x.made_ts / 3600;
            let pred_of = |h: i64| predicted.iter().find(|p| p.0 == h).map(|p| p.1);
            let keep = match current {
                Some((prev_hour, w)) if hour - prev_hour <= 1 => match (pred_of(c), pred_of(w)) {
                    (Some(a), Some(b)) if a - b > HYSTERESIS_MIN_GAIN => None,
                    _ => Some(w),
                },
                _ => None,
            };
            let w = keep.unwrap_or(c);
            current = Some((hour, w));
            if let Some(v) = x.fixed.get(&w) {
                push("hysteresis", day, *v, k);
            }
        }
    }
    let hours = g.len() as i64;
    let keeper = series.get("keeper").cloned().unwrap_or_default();
    let (keeper_mean, _, _, total_days, _) = stats(&keeper);
    let mut lines: Vec<Line> = series
        .iter()
        // A fixed width present in under 90% of the hours (an old table band) is not comparable.
        .filter(|(k, v)| !k.starts_with("fixed:") || v.len() as f64 >= 0.9 * hours as f64)
        .map(|(k, v)| {
            let (mean, vs, ci, d, ahead) = stats(v);
            Line { key: k.clone(), name: describe(k), hours: v.len() as i64, days: d, mean, vs_keeper: vs, ci, days_ahead: ahead }
        })
        .collect();
    // Best first; on a tie the hindsight ceiling is listed before the rule that reached it.
    lines.sort_by(|a, b| b.mean.total_cmp(&a.mean).then_with(|| (b.key == "hindsight").cmp(&(a.key == "hindsight"))));
    Ok(Some(Scoreboard { hours, days: total_days, keeper_mean, lines, port_check: port }))
}

/// The keeper's predicted net per width against what that width really made, over the graded hours.
#[derive(Clone, Debug)]
pub struct WidthCalibration {
    pub h: i64,
    pub predicted: f64,
    pub realised: f64,
}

#[derive(Clone, Debug)]
pub struct Calibration {
    pub hours: i64,
    pub widths: Vec<WidthCalibration>,
    /// Mean per-hour Spearman rank correlation between the keeper's predicted and the realised net
    /// across widths: 1 = it ranked them right, 0 = no relation, below 0 = backwards.
    pub rank_corr: f64,
    /// Hours in which the keeper's predicted best width was also the realised best.
    pub best_matched: i64,
}

fn ranks(v: &[f64]) -> Vec<f64> {
    let mut idx: Vec<usize> = (0..v.len()).collect();
    idx.sort_by(|a, b| v[*a].total_cmp(&v[*b]));
    let mut r = vec![0.0; v.len()];
    for (rank, i) in idx.into_iter().enumerate() {
        r[i] = rank as f64;
    }
    r
}

fn pearson(a: &[f64], b: &[f64]) -> Option<f64> {
    let n = a.len() as f64;
    if n < 3.0 {
        return None;
    }
    let (ma, mb) = (a.iter().sum::<f64>() / n, b.iter().sum::<f64>() / n);
    let (mut sab, mut saa, mut sbb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        sab += (x - ma) * (y - mb);
        saa += (x - ma) * (x - ma);
        sbb += (y - mb) * (y - mb);
    }
    (saa > 0.0 && sbb > 0.0).then(|| sab / (saa * sbb).sqrt())
}

/// Calibration over graded hours that carry the keeper's table. None without any.
pub fn calibration(g: &[Graded]) -> Option<Calibration> {
    let mut sum: BTreeMap<i64, (f64, f64, i64)> = BTreeMap::new();
    let mut corr: Vec<f64> = Vec::new();
    let mut best_matched = 0;
    let mut hours = 0;
    for x in g {
        let predicted = x.predicted();
        if predicted.is_empty() {
            continue;
        }
        let pairs: Vec<(i64, f64, f64)> = predicted.iter().filter_map(|(h, p)| x.fixed.get(h).map(|r| (*h, *p, *r))).collect();
        if pairs.len() < 3 {
            continue;
        }
        hours += 1;
        for (h, p, r) in &pairs {
            let e = sum.entry(*h).or_insert((0.0, 0.0, 0));
            e.0 += p;
            e.1 += r;
            e.2 += 1;
        }
        let p: Vec<f64> = pairs.iter().map(|x| x.1).collect();
        let r: Vec<f64> = pairs.iter().map(|x| x.2).collect();
        if let Some(c) = pearson(&ranks(&p), &ranks(&r)) {
            corr.push(c);
        }
        let pb = pairs.iter().max_by(|a, b| a.1.total_cmp(&b.1)).map(|x| x.0);
        let rb = pairs.iter().max_by(|a, b| a.2.total_cmp(&b.2)).map(|x| x.0);
        if pb.is_some() && pb == rb {
            best_matched += 1;
        }
    }
    if hours == 0 {
        return None;
    }
    let widths = sum
        .into_iter()
        .filter(|(_, (_, _, n))| *n as f64 >= 0.9 * hours as f64)
        .map(|(h, (p, r, n))| WidthCalibration { h, predicted: p / n as f64, realised: r / n as f64 })
        .collect();
    let rank_corr = if corr.is_empty() { 0.0 } else { corr.iter().sum::<f64>() / corr.len() as f64 };
    Some(Calibration { hours, widths, rank_corr, best_matched })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::bucket::PRIMARY_DEFAULT;
    use serde_json::json;

    #[test]
    fn choose_with_floor_follows_the_keeper_rule() {
        let p = shadow::Policy::default(); // preferred 4, narrow 0.10, widen 0.25
        let nets = [(2, 0.0010), (3, 0.0008), (4, 0.0005), (8, 0.0004), (16, 0.0003)];
        assert_eq!(choose_with_floor(&nets, 1, &p), Some(2), "narrower beats the preferred width by more than 10%");
        assert_eq!(
            choose_with_floor(&nets, 4, &p),
            Some(4),
            "with the floor the narrow widths are not on offer and no wider one beats ±2%"
        );
        let losing = [(4, -0.0005), (8, -0.0004), (16, -0.0001)];
        assert_eq!(
            choose_with_floor(&losing, 4, &p),
            Some(16),
            "a losing preferred width gives way to the wider one that loses least by 25%"
        );
        let flat = [(4, 0.0005), (8, 0.0005 * 1.2), (16, 0.0005 * 1.3)];
        assert_eq!(choose_with_floor(&flat, 4, &p), Some(16), "wider wins only above the 25% margin: 16 at +30% does, 8 at +20% does not");
        assert_eq!(choose_with_floor(&[], 4, &p), None);
    }

    fn table(ts: i64, choice: i64, nets: &[(i64, f64)]) -> Value {
        json!({ "ts": ts, "tick": -197700.0, "vol": 0.003, "position_usd": 1000.0, "gas_usd": 0.025, "choice": choice,
            "rows": nets.iter().map(|(h, n)| json!({ "h": *h as f64, "gross_apr": 1.0, "rebalances": 0.1, "in_range": 0.9, "capture": 0.95,
                "exec_fee_pips": 400.0, "divergence": 0.001, "net_ex_div": n + 0.001, "net": n })).collect::<Vec<_>>() })
    }

    #[test]
    fn scoreboard_ranks_candidates_on_realised_results() {
        let s = Store::memory().unwrap();
        let t0 = 1_791_000_000 / 86_400 * 86_400;
        // 48 hours over 2 days: the keeper (table choice 16) predicts wide is best; in reality ±2% (h 4) earns most.
        for k in 0..48 {
            let made = t0 + k * 3600;
            let predicted = [(2, -0.0014), (3, -0.0010), (4, -0.0007), (8, -0.0004), (12, -0.0002), (16, -0.0001)];
            let real = [(2, 0.0003), (3, 0.0007), (4, 0.0009), (8, 0.0006), (12, 0.0004), (16, 0.0003)];
            let detail = json!({ "table": table(made, 16, &predicted), "forecasts": {} }).to_string();
            let replay = json!({ "fixed": real, "keeper": { "net_per_day": 0.0003 }, "policy": {} }).to_string();
            s.db.execute(
                "INSERT INTO shadow (made_ts, start_tick, position_usd, keeper_choice, ai_choice, keeper_vol, ai_vol, ai_model, detail, keeper_net_per_day, ai_net_per_day, best_hindsight, best_net, replay, resolved_ts, vault)
                 VALUES (?1, -197700, 1000, 16, 12, 0.003, 0.003, 'ewma', ?2, 0.0003, 0.0004, 4, 0.0009, ?3, ?4, ?5)",
                rusqlite::params![made, detail, replay, made + 86_400, PRIMARY_DEFAULT],
            )
            .unwrap();
            // The same hour shadowed for the old vault too: must not double count.
            s.db.execute(
                "INSERT INTO shadow (made_ts, start_tick, position_usd, keeper_choice, ai_choice, keeper_vol, ai_vol, ai_model, detail, keeper_net_per_day, ai_net_per_day, best_hindsight, best_net, replay, resolved_ts, vault)
                 VALUES (?1, -197700, 1000, 16, 12, 0.003, 0.003, 'ewma', ?2, 0.0003, 0.0004, 4, 0.0009, ?3, ?4, 'old')",
                rusqlite::params![made + 1, detail, replay, made + 86_400, ],
            )
            .unwrap();
            s.db.execute(
                "INSERT INTO shadow_model (made_ts, model, vol, choice, net_per_day, replay) VALUES (?1, 'ewma', 0.003, 12, 0.0004, NULL)",
                [made],
            )
            .unwrap();
        }
        let sb = scoreboard(&s, t0 + 49 * 3600, 30, PRIMARY_DEFAULT).unwrap().expect("graded rows");
        assert_eq!(sb.hours, 48, "one row per hour");
        assert_eq!(sb.days, 2);
        assert!((sb.keeper_mean - 0.0003).abs() < 1e-12);
        let get = |k: &str| sb.lines.iter().find(|l| l.key == k).unwrap_or_else(|| panic!("line {k}"));
        assert_eq!(sb.lines[0].key, "hindsight", "{:?}", sb.lines.iter().map(|l| &l.key).collect::<Vec<_>>());
        assert_eq!(sb.lines[1].key, "fixed:4");
        let f4 = get("fixed:4");
        assert!((f4.vs_keeper - 0.0006).abs() < 1e-12 && f4.days_ahead == 2 && f4.hours == 48);
        assert!(f4.ci.0 > 0.0 && f4.ci.1 >= f4.ci.0, "{:?}", f4.ci);
        // The keeper's table says every width loses and 16 loses least: the floor rule keeps 16, as does hysteresis.
        assert!((get("floor").mean - 0.0003).abs() < 1e-12);
        assert!((get("hysteresis").mean - 0.0003).abs() < 1e-12);
        assert!((get("ewma").mean - 0.0004).abs() < 1e-12 && get("ewma").hours == 48);
        assert_eq!(sb.port_check, (48, 48), "the rule port reproduces the keeper's recorded choice");
        assert_eq!(describe("fixed:4"), "always ±2%");
        assert_eq!(describe("fixed:3"), "always ±1.5%");
        let c = calibration(&graded(&s, 0, PRIMARY_DEFAULT).unwrap()).unwrap();
        assert_eq!(c.hours, 48);
        assert!(c.rank_corr < 0.0, "predicted order is the reverse of reality: {}", c.rank_corr);
        assert_eq!(c.best_matched, 0);
        let w4 = c.widths.iter().find(|w| w.h == 4).unwrap();
        assert!((w4.predicted + 0.0007).abs() < 1e-12 && (w4.realised - 0.0009).abs() < 1e-12);
    }

    #[test]
    fn hysteresis_holds_the_width_until_the_gain_is_clear() {
        let s = Store::memory().unwrap();
        let t0 = 1_791_000_000 / 86_400 * 86_400;
        // Hour 0: table best is 16. Hours 1-2: 12 beats 16 by only 0.005%/day (noise). Hour 3: by 0.05%/day (clear).
        let tables = [
            [(4, -0.0007), (12, -0.0003), (16, -0.0001)],
            [(4, -0.0007), (12, -0.00005), (16, -0.0001)],
            [(4, -0.0007), (12, -0.00005), (16, -0.0001)],
            [(4, -0.0007), (12, 0.0004), (16, -0.0001)],
        ];
        for (k, nets) in tables.iter().enumerate() {
            let made = t0 + k as i64 * 3600;
            let choice = choose_with_floor(nets, 4, &shadow::Policy::default()).unwrap();
            let detail = json!({ "table": table(made, choice, nets) }).to_string();
            let replay = json!({ "fixed": [(4, 0.0), (12, 0.0012), (16, 0.0001)], "keeper": {}, "policy": {} }).to_string();
            s.db.execute(
                "INSERT INTO shadow (made_ts, start_tick, keeper_choice, ai_choice, keeper_net_per_day, detail, replay, vault) VALUES (?1, -197700, ?2, ?2, 0.0001, ?3, ?4, ?5)",
                rusqlite::params![made, choice, detail, replay, PRIMARY_DEFAULT],
            )
            .unwrap();
        }
        let sb = scoreboard(&s, t0 + 5 * 3600, 30, PRIMARY_DEFAULT).unwrap().unwrap();
        let hy = sb.lines.iter().find(|l| l.key == "hysteresis").unwrap();
        let fl = sb.lines.iter().find(|l| l.key == "floor").unwrap();
        // Floor rule follows the table: 16, 12, 12, 12 -> (0.0001 + 3 * 0.0012) / 4. Hysteresis: 16, 16, 16, 12 -> (3 * 0.0001 + 0.0012) / 4.
        assert!((fl.mean - (0.0001 + 3.0 * 0.0012) / 4.0).abs() < 1e-12, "{}", fl.mean);
        assert!((hy.mean - (3.0 * 0.0001 + 0.0012) / 4.0).abs() < 1e-12, "{}", hy.mean);
    }
}
