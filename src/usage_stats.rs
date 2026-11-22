//! Aggregations behind the "Agent usage" dashboard tab.
//!
//! Pure functions over [`UsageStore`] events: no I/O, no UI, so every number
//! on the dashboard is unit-tested. Local time comes in as a closure giving
//! the UTC offset at an instant, which keeps time zones out of the tests.
//!
//! Definitions:
//! * a **task** is an agent session (one Claude Code / Codex / Gemini id);
//! * a **turn** is one model call;
//! * **input** counts uncached input plus cache reads and cache writes;
//! * **agent time** merges each session's calls into active spans (a gap of
//!   more than [`IDLE_GAP_MS`] ends a span; a lone call counts [`MIN_SPAN_MS`]).

use std::collections::{HashMap, HashSet};

use crate::ai_monitor::{NO_MODEL, UsageEvent, UsageStore};

pub const IDLE_GAP_MS: u64 = 5 * 60_000;
pub const MIN_SPAN_MS: u64 = 60_000;
/// Model rows shown before folding the rest into "N more".
pub const TOP_MODELS: usize = 6;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Range {
    Week,
    #[default]
    Month,
    Quarter,
    All,
}

impl Range {
    pub const ALL: [Range; 4] = [Range::Week, Range::Month, Range::Quarter, Range::All];

    /// Days covered; `None` = since the first known event.
    pub fn days(self) -> Option<u32> {
        match self {
            Range::Week => Some(7),
            Range::Month => Some(30),
            Range::Quarter => Some(90),
            Range::All => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Range::Week => "7 days",
            Range::Month => "30 days",
            Range::Quarter => "90 days",
            Range::All => "All",
        }
    }

    /// Days of logs the monitor must have loaded (`u32::MAX` = all). Twice
    /// the range, so the previous period for the trend is complete too.
    pub fn history_needed(self) -> u32 {
        self.days().map_or(u32::MAX, |d| d * 2)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Provider {
    Anthropic,
    Google,
    OpenAi,
    XAi,
    Other,
}

impl Provider {
    pub fn of_model(model: &str) -> Provider {
        let m = model.to_ascii_lowercase();
        let has = |w: &str| m.contains(w);
        if has("claude") || has("opus") || has("sonnet") || has("haiku") || has("fable") {
            Provider::Anthropic
        } else if has("gemini") || has("gemma") {
            Provider::Google
        } else if has("gpt") || has("codex") || is_openai_o_series(&m) {
            Provider::OpenAi
        } else if has("grok") {
            Provider::XAi
        } else {
            Provider::Other
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Provider::Anthropic => "Anthropic",
            Provider::Google => "Google",
            Provider::OpenAi => "OpenAI",
            Provider::XAi => "xAI",
            Provider::Other => "Other",
        }
    }
}

/// `o1`, `o3-mini`, `o4-high`…
fn is_openai_o_series(m: &str) -> bool {
    let b = m.as_bytes();
    b.len() >= 2 && b[0] == b'o' && b[1].is_ascii_digit()
}

/// A headline number with its previous-period value and a daily sparkline.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Kpi {
    pub value: u64,
    /// Same-length period right before the range; `None` for "All".
    pub previous: Option<u64>,
    pub daily: Vec<u64>,
}

impl Kpi {
    /// Change vs the previous period in percent; `None` when there's nothing
    /// to compare against.
    pub fn trend_percent(&self) -> Option<f64> {
        let prev = self.previous.filter(|p| *p > 0)?;
        Some((self.value as f64 - prev as f64) / prev as f64 * 100.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DayBar {
    /// Days since 1970-01-01 in local time.
    pub local_day: i64,
    pub input: u64,
    pub output: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelRow {
    pub name: String,
    pub provider: Provider,
    pub input: u64,
    pub output: u64,
    pub tasks: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentRow {
    pub name: String,
    pub input: u64,
    pub output: u64,
    pub active_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageStats {
    /// First and last local day of the range (inclusive).
    pub first_day: i64,
    pub last_day: i64,
    pub input: Kpi,
    pub output: Kpi,
    pub tasks: Kpi,
    /// Milliseconds.
    pub agent_time: Kpi,
    /// Share of input that came from cache reads (0..=1).
    pub cache_share: f64,
    pub turns: u64,
    pub output_per_turn: u64,
    /// Most sessions active at the same moment.
    pub max_concurrent: u32,
    pub days: Vec<DayBar>,
    pub active_days: usize,
    /// Mean of input + output per day of the range.
    pub tokens_per_day: u64,
    /// Turns by local weekday (0 = Monday) and hour.
    pub heat: [[u64; 24]; 7],
    /// Busiest (weekday, hour), if anything happened.
    pub peak: Option<(usize, usize)>,
    /// Sorted by total tokens, all of them (the view folds the tail).
    pub models: Vec<ModelRow>,
    /// Input + output per provider, sorted descending.
    pub providers: Vec<(Provider, u64)>,
    /// Every agent with usage in the range, ignoring the agent filter.
    pub agents: Vec<AgentRow>,
}

impl UsageStats {
    /// `offset_secs(ts_ms)` is the local UTC offset at that instant.
    pub fn compute(store: &UsageStore, range: Range, agent: Option<&str>, now_ms: u64, offset_secs: impl Fn(u64) -> i64) -> Self {
        let local_day = |ts: u64| ((ts / 1000) as i64 + offset_secs(ts)).div_euclid(86_400);
        let events = store.events();
        let today = local_day(now_ms);
        let first_day = match range.days() {
            Some(d) => today - (d as i64 - 1),
            None => events.iter().map(|e| local_day(e.ts_ms)).min().unwrap_or(today).min(today),
        };
        let len = (today - first_day + 1) as usize;
        // Previous period: the same number of days right before.
        let prev_first = range.days().map(|_| first_day - len as i64);

        // An unknown agent maps to an id no event has, so nothing matches.
        let agent_id = agent.map(|name| store.name_id(name).unwrap_or(NO_MODEL));
        let wanted = |e: &UsageEvent| agent_id.is_none_or(|a| e.agent == a);

        let mut s = UsageStats { first_day, last_day: today, ..Default::default() };
        s.days = (0..len).map(|i| DayBar { local_day: first_day + i as i64, ..Default::default() }).collect();
        let mut daily_turns = vec![0u64; len];
        let mut daily_sessions: Vec<HashSet<u32>> = vec![HashSet::new(); len];
        let mut sessions = HashSet::new();
        let (mut prev_input, mut prev_output) = (0u64, 0u64);
        let mut prev_sessions = HashSet::new();
        let mut cache_read = 0u64;
        let mut models: HashMap<u16, (u64, u64, HashSet<u32>)> = HashMap::new();
        let mut agents: HashMap<u16, (u64, u64)> = HashMap::new();
        // Call times per session, for agent time (prev period included).
        let mut calls: HashMap<u32, (u16, Vec<u64>)> = HashMap::new();

        for e in events {
            let day = local_day(e.ts_ms);
            let input = e.usage.input + e.usage.cache_read + e.usage.cache_creation;
            let output = e.usage.output;
            let in_range = day >= first_day && day <= today;
            let in_prev = prev_first.is_some_and(|p| day >= p && day < first_day);
            if !(in_range || in_prev) {
                continue;
            }
            // Every agent's calls: the agent list ignores the filter.
            calls.entry(e.session).or_insert_with(|| (e.agent, Vec::new())).1.push(e.ts_ms);
            if in_range {
                let a = agents.entry(e.agent).or_default();
                a.0 += input;
                a.1 += output;
            }
            if !wanted(e) {
                continue;
            }
            if in_prev {
                prev_input += input;
                prev_output += output;
                prev_sessions.insert(e.session);
                continue;
            }
            let i = (day - first_day) as usize;
            s.days[i].input += input;
            s.days[i].output += output;
            daily_turns[i] += e.turns as u64;
            daily_sessions[i].insert(e.session);
            sessions.insert(e.session);
            cache_read += e.usage.cache_read;
            s.turns += e.turns as u64;

            let local_secs = (e.ts_ms / 1000) as i64 + offset_secs(e.ts_ms);
            let weekday = (local_secs.div_euclid(86_400) + 3).rem_euclid(7) as usize; // 1970-01-01 was a Thursday
            let hour = (local_secs.rem_euclid(86_400) / 3600) as usize;
            s.heat[weekday][hour] += (e.turns as u64).max(1);

            let m = models.entry(e.model).or_default();
            m.0 += input;
            m.1 += output;
            m.2.insert(e.session);
        }

        // Agent time from active spans, attributed to the day each starts.
        let mut daily_ms = vec![0u64; len];
        let mut prev_ms = 0u64;
        let mut agent_ms: HashMap<u16, u64> = HashMap::new();
        let mut edges: Vec<(u64, i32)> = Vec::new();
        for (agent, times) in calls.values_mut() {
            times.sort_unstable();
            for (start, end) in spans(times) {
                let ms = end - start;
                let day = local_day(start);
                if day >= first_day && day <= today {
                    *agent_ms.entry(*agent).or_default() += ms;
                }
                if agent_id.is_some_and(|a| a != *agent) {
                    continue;
                }
                if day >= first_day && day <= today {
                    daily_ms[(day - first_day) as usize] += ms;
                    edges.push((start, 1));
                    edges.push((end, -1));
                } else if prev_first.is_some_and(|p| day >= p && day < first_day) {
                    prev_ms += ms;
                }
            }
        }
        // Ends sort before starts at the same instant: touching isn't overlapping.
        edges.sort_unstable();
        let mut open = 0i32;
        for (_, d) in edges {
            open += d;
            s.max_concurrent = s.max_concurrent.max(open.max(0) as u32);
        }

        let daily_input: Vec<u64> = s.days.iter().map(|d| d.input).collect();
        let daily_output: Vec<u64> = s.days.iter().map(|d| d.output).collect();
        let prev = |v: u64| prev_first.map(|_| v);
        s.input = Kpi { value: daily_input.iter().sum(), previous: prev(prev_input), daily: daily_input };
        s.output = Kpi { value: daily_output.iter().sum(), previous: prev(prev_output), daily: daily_output };
        s.tasks = Kpi {
            value: sessions.len() as u64,
            previous: prev(prev_sessions.len() as u64),
            daily: daily_sessions.iter().map(|d| d.len() as u64).collect(),
        };
        s.agent_time = Kpi { value: daily_ms.iter().sum(), previous: prev(prev_ms), daily: daily_ms };
        s.cache_share = if s.input.value == 0 { 0.0 } else { cache_read as f64 / s.input.value as f64 };
        s.output_per_turn = s.output.value.checked_div(s.turns).unwrap_or(0);
        s.active_days = daily_turns.iter().zip(&s.days).filter(|(t, d)| **t > 0 || d.input + d.output > 0).count();
        s.tokens_per_day = (s.input.value + s.output.value) / len.max(1) as u64;
        s.peak = (0..7)
            .flat_map(|d| (0..24).map(move |h| (d, h)))
            .filter(|&(d, h)| s.heat[d][h] > 0)
            .max_by_key(|&(d, h)| (s.heat[d][h], std::cmp::Reverse((d, h))));

        let mut by_provider: HashMap<Provider, u64> = HashMap::new();
        s.models = models
            .into_iter()
            .map(|(id, (input, output, tasks))| {
                let name = if id == NO_MODEL { "unknown".to_owned() } else { store.name(id).to_owned() };
                let provider = Provider::of_model(&name);
                *by_provider.entry(provider).or_default() += input + output;
                ModelRow { name, provider, input, output, tasks: tasks.len() as u64 }
            })
            .collect();
        s.models.sort_by(|a, b| (b.input + b.output).cmp(&(a.input + a.output)).then_with(|| a.name.cmp(&b.name)));
        s.providers = by_provider.into_iter().filter(|(_, v)| *v > 0).collect();
        s.providers.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

        s.agents = agents
            .into_iter()
            .map(|(id, (input, output))| AgentRow {
                name: store.name(id).to_owned(),
                input,
                output,
                active_ms: agent_ms.get(&id).copied().unwrap_or(0),
            })
            .collect();
        s.agents.sort_by(|a, b| (b.input + b.output).cmp(&(a.input + a.output)).then_with(|| a.name.cmp(&b.name)));
        s
    }
}

/// Merges sorted call times into `(start, end)` active spans.
fn spans(times: &[u64]) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &t in times {
        match out.last_mut() {
            Some((_, end)) if t <= *end + IDLE_GAP_MS => *end = (*end).max(t),
            _ => out.push((t, t)),
        }
    }
    for (start, end) in &mut out {
        *end = (*end).max(*start + MIN_SPAN_MS);
    }
    out
}

/// `(year, month 1-12, day)` of a day number (Howard Hinnant's algorithm).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `Oct 23`.
pub fn format_day(local_day: i64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (_, m, d) = civil_from_days(local_day);
    format!("{} {d}", MONTHS[(m - 1) as usize])
}

/// `1,110`.
pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `347 h`, `9.2 h`, `23 min`, `<1 min`.
pub fn format_hours(ms: u64) -> String {
    let mins = ms / 60_000;
    match mins {
        0 => "<1 min".into(),
        1..60 => format!("{mins} min"),
        60..600 => format!("{:.1} h", ms as f64 / 3_600_000.0).replace(".0 h", " h"),
        _ => format!("{} h", (ms as f64 / 3_600_000.0).round() as u64),
    }
}

pub const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_monitor::{TokenUsage, UsageRecord};

    const DAY_MS: u64 = 86_400_000;
    const H: u64 = 3_600_000;
    const MIN: u64 = 60_000;
    /// 2026-11-16 00:00 UTC, a Monday.
    const MONDAY: u64 = 1_794_787_200_000;

    fn call(store: &mut UsageStore, agent: &str, session: &str, model: &str, ts: u64, input: u64, output: u64) {
        let record = UsageRecord {
            session_id: session.into(),
            model: Some(model.into()),
            timestamp_ms: Some(ts),
            usage: TokenUsage { input, output, cache_read: input * 9, cache_creation: 0 },
            ..Default::default()
        };
        store.ingest(agent, record, ts);
    }

    fn utc(_: u64) -> i64 {
        0
    }

    #[test]
    fn monday_constant_is_a_monday() {
        assert_eq!(civil_from_days((MONDAY / DAY_MS) as i64), (2026, 11, 16));
        assert_eq!(((MONDAY / DAY_MS) as i64 + 3) % 7, 0);
    }

    #[test]
    fn kpis_days_and_previous_period() {
        let mut store = UsageStore::default();
        let now = MONDAY + 6 * DAY_MS + 12 * H; // Sunday noon
        // In range (Mon..Sun): two sessions on Monday, one on Wednesday.
        call(&mut store, "Claude Code", "a", "claude-opus-5-5", MONDAY + 10 * H, 100, 10);
        call(&mut store, "Claude Code", "a", "claude-opus-5-5", MONDAY + 10 * H + MIN, 100, 30);
        call(&mut store, "Codex", "b", "gpt-6", MONDAY + 11 * H, 50, 5);
        call(&mut store, "Claude Code", "c", "claude-fable-5-2", MONDAY + 2 * DAY_MS + 9 * H, 10, 1);
        // Previous week: one session.
        call(&mut store, "Claude Code", "old", "claude-opus-5-5", MONDAY - 3 * DAY_MS, 100, 10);

        let s = UsageStats::compute(&store, Range::Week, None, now, utc);
        assert_eq!(s.days.len(), 7);
        assert_eq!(s.days[0].local_day, (MONDAY / DAY_MS) as i64);
        // input counts cache reads (9x) too.
        assert_eq!(s.input.value, 260 * 10);
        assert_eq!(s.output.value, 46);
        assert_eq!(s.input.daily, vec![2500, 0, 100, 0, 0, 0, 0]);
        assert_eq!(s.tasks.value, 3);
        assert_eq!(s.tasks.previous, Some(1));
        assert_eq!(s.turns, 4);
        assert_eq!(s.output_per_turn, 11);
        assert!((s.cache_share - 0.9).abs() < 1e-9);
        assert_eq!(s.active_days, 2);
        assert_eq!(s.output.previous, Some(10));
        assert_eq!(s.output.trend_percent().unwrap().round(), 360.0);
        assert_eq!(s.tokens_per_day, (2600 + 46) / 7);
    }

    #[test]
    fn all_range_starts_at_first_event_and_has_no_trend() {
        let mut store = UsageStore::default();
        call(&mut store, "Claude Code", "a", "opus", MONDAY - 40 * DAY_MS, 1, 1);
        let s = UsageStats::compute(&store, Range::All, None, MONDAY, utc);
        assert_eq!(s.days.len(), 41);
        assert_eq!(s.input.previous, None);
        assert_eq!(s.input.trend_percent(), None);
        // Empty store: a single day, today.
        let s = UsageStats::compute(&UsageStore::default(), Range::All, None, MONDAY, utc);
        assert_eq!(s.days.len(), 1);
        assert_eq!(s.peak, None);
    }

    #[test]
    fn heatmap_uses_local_time() {
        let mut store = UsageStore::default();
        // Monday 23:30 UTC = Tuesday 01:30 at UTC+2.
        call(&mut store, "Claude Code", "a", "opus", MONDAY + 23 * H + 30 * MIN, 1, 1);
        let s = UsageStats::compute(&store, Range::Week, None, MONDAY + 3 * DAY_MS, |_| 7200);
        assert_eq!(s.heat[1][1], 1);
        assert_eq!(s.peak, Some((1, 1)));
        let tuesday = (MONDAY / DAY_MS) as i64 + 1;
        assert_eq!(s.days.iter().find(|d| d.input > 0).map(|d| d.local_day), Some(tuesday));
        let s = UsageStats::compute(&store, Range::Week, None, MONDAY + 3 * DAY_MS, utc);
        assert_eq!(s.peak, Some((0, 23)));
    }

    #[test]
    fn agent_time_spans_and_concurrency() {
        let mut store = UsageStore::default();
        // Session a: 10:00, 10:04 (same span), 11:00 (new span) → 4 min + 1 min.
        for t in [0, 4 * MIN, H] {
            call(&mut store, "Claude Code", "a", "opus", MONDAY + 10 * H + t, 1, 1);
        }
        // Session b overlaps the first span of a.
        call(&mut store, "Codex", "b", "gpt-6", MONDAY + 10 * H + 2 * MIN, 1, 1);
        call(&mut store, "Codex", "b", "gpt-6", MONDAY + 10 * H + 5 * MIN, 1, 1);
        let s = UsageStats::compute(&store, Range::Week, None, MONDAY + DAY_MS, utc);
        assert_eq!(s.agent_time.value, 4 * MIN + MIN + 3 * MIN);
        assert_eq!(s.max_concurrent, 2);
        let codex = s.agents.iter().find(|a| a.name == "Codex").unwrap();
        assert_eq!(codex.active_ms, 3 * MIN);
    }

    #[test]
    fn agent_filter_narrows_everything_but_the_agent_list() {
        let mut store = UsageStore::default();
        call(&mut store, "Claude Code", "a", "claude-opus-5-5", MONDAY + H, 100, 10);
        call(&mut store, "Codex", "b", "gpt-6", MONDAY + 2 * H, 50, 5);
        let s = UsageStats::compute(&store, Range::Week, Some("Codex"), MONDAY + DAY_MS, utc);
        assert_eq!(s.output.value, 5);
        assert_eq!(s.tasks.value, 1);
        assert_eq!(s.models.len(), 1);
        assert_eq!(s.models[0].name, "gpt-6");
        assert_eq!(s.agents.len(), 2, "the agent list stays complete");
        assert_eq!(s.agents[0].name, "Claude Code");
        assert_eq!(s.max_concurrent, 1);
        // Unknown agent → nothing.
        let s = UsageStats::compute(&store, Range::Week, Some("nope"), MONDAY + DAY_MS, utc);
        assert_eq!(s.output.value, 0);
    }

    #[test]
    fn models_and_providers() {
        let mut store = UsageStore::default();
        call(&mut store, "Claude Code", "a", "claude-opus-5-5", MONDAY + H, 100, 10);
        call(&mut store, "Claude Code", "b", "claude-opus-5-5", MONDAY + H, 100, 10);
        call(&mut store, "Gemini", "c", "gemini-4-argon-high", MONDAY + H, 10, 1);
        let s = UsageStats::compute(&store, Range::Week, None, MONDAY + DAY_MS, utc);
        assert_eq!(s.models[0].name, "claude-opus-5-5");
        assert_eq!(s.models[0].tasks, 2);
        assert_eq!(s.providers[0].0, Provider::Anthropic);
        assert_eq!(s.providers[1].0, Provider::Google);

        assert_eq!(Provider::of_model("Opus 5.5"), Provider::Anthropic);
        assert_eq!(Provider::of_model("gpt-6.2-astra"), Provider::OpenAi);
        assert_eq!(Provider::of_model("o3-mini"), Provider::OpenAi);
        assert_eq!(Provider::of_model("gpt-5-codex"), Provider::OpenAi);
        assert_eq!(Provider::of_model("grok-4.7-build"), Provider::XAi);
        assert_eq!(Provider::of_model("llama-4"), Provider::Other);
        assert_eq!(Provider::of_model("ollama"), Provider::Other);
    }

    #[test]
    fn formats() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(1_110), "1,110");
        assert_eq!(format_count(1_234_567), "1,234,567");
        assert_eq!(format_hours(30_000), "<1 min");
        assert_eq!(format_hours(23 * MIN), "23 min");
        assert_eq!(format_hours(9 * H + 12 * MIN), "9.2 h");
        assert_eq!(format_hours(347 * H + 10 * MIN), "347 h");
        assert_eq!(format_day((MONDAY / DAY_MS) as i64), "Nov 16");
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }
}
