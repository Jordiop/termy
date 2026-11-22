//! 100% local AI usage monitor — no network, ever.
//!
//! Watches the agents' own session logs with `notify` and aggregates token
//! usage per agent session and per week:
//!
//! * Claude Code: `~/.claude/projects/**/*.jsonl` (`message.usage`, one line
//!   per content block, deduplicated by `message.id` + `requestId`).
//! * Codex: `~/.codex/sessions/**/rollout-*.jsonl` (cumulative `token_count`).
//! * Gemini CLI: `~/.gemini/tmp/**/chats/*.json` (per-message `tokens`).
//! * Generic JSONL with JSON-pointer field mapping, configured in TOML.
//! * A local command that prints `{"used", "limit", "resets_at"}`.
//!
//! The UI thread only takes a short read lock on [`UsageStore`]; all file I/O
//! happens on the monitor thread, which calls `wake` after each batch.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender, unbounded};
use notify::Watcher;
use parking_lot::RwLock;
use serde_json::Value;

use crate::Wake;
use crate::config::{AgentProfile, AppConfig, JsonlSource, MetricsSource, TokenBudget, expand_tilde};

const MINUTE_MS: u64 = 60_000;
const DAY_MS: u64 = 86_400_000;
const WEEK_MS: u64 = 7 * DAY_MS;

// ---------------------------------------------------------------------------
// Token accounting
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Uncached input tokens.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_creation
    }

    /// Tokens counted against budgets.
    pub fn billable(&self, include_cache_reads: bool) -> u64 {
        self.input + self.output + self.cache_creation + if include_cache_reads { self.cache_read } else { 0 }
    }

    pub fn is_zero(&self) -> bool {
        self.total() == 0
    }

    fn saturating_sub(self, o: TokenUsage) -> TokenUsage {
        TokenUsage {
            input: self.input.saturating_sub(o.input),
            output: self.output.saturating_sub(o.output),
            cache_read: self.cache_read.saturating_sub(o.cache_read),
            cache_creation: self.cache_creation.saturating_sub(o.cache_creation),
        }
    }

    fn max(self, o: TokenUsage) -> TokenUsage {
        TokenUsage {
            input: self.input.max(o.input),
            output: self.output.max(o.output),
            cache_read: self.cache_read.max(o.cache_read),
            cache_creation: self.cache_creation.max(o.cache_creation),
        }
    }

    /// Any field went down → the counter was reset (new run / truncated log).
    fn regressed_from(&self, prev: &TokenUsage) -> bool {
        self.input < prev.input
            || self.output < prev.output
            || self.cache_read < prev.cache_read
            || self.cache_creation < prev.cache_creation
    }
}

impl std::ops::AddAssign for TokenUsage {
    fn add_assign(&mut self, o: TokenUsage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_creation += o.cache_creation;
    }
}

/// One observation extracted from a log.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageRecord {
    pub session_id: String,
    pub cwd: Option<PathBuf>,
    pub model: Option<String>,
    pub timestamp_ms: Option<u64>,
    pub usage: TokenUsage,
    /// Records with the same key describe the same API call.
    pub dedupe_key: Option<String>,
    /// `usage` is the session's running total, not a delta.
    pub cumulative: bool,
    /// Running total of model calls in the session, when the log states it
    /// (Gemini). Otherwise each new call counts as one turn.
    pub turns: Option<u32>,
}

/// Aggregated usage of one agent session (one Claude/Codex/Gemini session id).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionUsage {
    pub agent: String,
    pub session_id: String,
    pub cwd: Option<PathBuf>,
    pub model: Option<String>,
    pub usage: TokenUsage,
    /// Model calls ("turns").
    pub turns: u64,
    pub first_ms: u64,
    pub last_ms: u64,
    /// Index referenced by [`UsageEvent::session`].
    idx: u32,
    last_cumulative: TokenUsage,
    last_cumulative_turns: u32,
}

/// Sentinel for "no model known" in [`UsageEvent::model`].
pub const NO_MODEL: u16 = u16::MAX;

/// One increment of usage, kept for time-based views (budgets, dashboard).
/// Names are interned in the store: see [`UsageStore::name`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UsageEvent {
    pub agent: u16,
    pub model: u16,
    pub session: u32,
    pub ts_ms: u64,
    pub usage: TokenUsage,
    /// New model calls in this increment.
    pub turns: u32,
}

/// Reported by a `kind = "command"` source.
#[derive(Clone, Debug, PartialEq)]
pub struct QuotaReport {
    pub used: u64,
    pub limit: Option<u64>,
    pub resets_at_ms: Option<u64>,
    pub fetched_ms: u64,
}

/// A used/limit pair ready for a progress bar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gauge {
    pub used: u64,
    pub limit: u64,
    pub resets_in_ms: Option<u64>,
}

impl Gauge {
    pub fn fraction_used(&self) -> f32 {
        if self.limit == 0 { 1.0 } else { (self.used as f64 / self.limit as f64).min(1.0) as f32 }
    }

    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.used)
    }

    pub fn percent_used(&self) -> u8 {
        (self.fraction_used() * 100.0).round() as u8
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BudgetReport {
    pub session: Option<Gauge>,
    pub weekly: Option<Gauge>,
    /// Usage in the current weekly window even when no limit is configured.
    pub weekly_used: u64,
}

/// All usage known to the monitor. Pure data; unit-testable without I/O.
#[derive(Debug, Default)]
pub struct UsageStore {
    sessions: HashMap<(String, String), SessionUsage>,
    events: Vec<UsageEvent>,
    seen: HashMap<String, TokenUsage>,
    quotas: HashMap<String, QuotaReport>,
    /// Interned agent and model names (indices in [`UsageEvent`]).
    names: Vec<String>,
    name_ids: HashMap<String, u16>,
    /// Bumped on every change, so views can cache what they derive.
    generation: u64,
    /// How many days of logs have been scanned (`u32::MAX` = everything).
    history_days: u32,
    /// A deeper history scan is running.
    loading: bool,
}

impl UsageStore {
    pub fn ingest(&mut self, agent: &str, record: UsageRecord, now_ms: u64) {
        let ts = record.timestamp_ms.unwrap_or(now_ms);
        let key = (agent.to_owned(), record.session_id.clone());
        let next_idx = self.sessions.len() as u32;
        let session = self.sessions.entry(key).or_insert_with(|| SessionUsage {
            agent: agent.to_owned(),
            session_id: record.session_id.clone(),
            first_ms: ts,
            last_ms: ts,
            idx: next_idx,
            ..Default::default()
        });

        let new_call;
        let delta = if record.cumulative {
            let prev = session.last_cumulative;
            session.last_cumulative = record.usage;
            let delta = if record.usage.regressed_from(&prev) { record.usage } else { record.usage.saturating_sub(prev) };
            new_call = !delta.is_zero();
            delta
        } else if let Some(dedupe) = record.dedupe_key.as_ref().map(|k| format!("{agent}\0{k}")) {
            // Streaming logs repeat a call's usage on every content block,
            // sometimes growing: count only what's new.
            let prev = self.seen.get(&dedupe).copied();
            // Zero-usage entries (e.g. `<synthetic>` messages) aren't calls.
            new_call = prev.is_none() && !record.usage.is_zero();
            let prev = prev.unwrap_or_default();
            self.seen.insert(dedupe, prev.max(record.usage));
            record.usage.saturating_sub(prev)
        } else {
            new_call = !record.usage.is_zero();
            record.usage
        };
        let turns = match record.turns {
            Some(total) => {
                let prev = session.last_cumulative_turns;
                session.last_cumulative_turns = total;
                if total < prev { total } else { total - prev }
            }
            None => new_call as u32,
        };

        session.usage += delta;
        session.turns += turns as u64;
        session.first_ms = session.first_ms.min(ts);
        session.last_ms = session.last_ms.max(ts);
        if record.cwd.is_some() {
            session.cwd = record.cwd;
        }
        if let Some(model) = record.model.filter(|m| !m.starts_with('<')) {
            session.model = Some(model);
        }
        if !delta.is_zero() || turns > 0 {
            let (session, model) = (session.idx, session.model.clone());
            let agent = self.intern(agent);
            let model = model.map_or(NO_MODEL, |m| self.intern(&m));
            self.events.push(UsageEvent { agent, model, session, ts_ms: ts, usage: delta, turns });
        }
        self.generation += 1;
    }

    fn intern(&mut self, name: &str) -> u16 {
        if let Some(&id) = self.name_ids.get(name) {
            return id;
        }
        // NO_MODEL is reserved; past that many names, lump the rest together.
        let id = (self.names.len() as u16).min(NO_MODEL - 1);
        if (id as usize) == self.names.len() {
            self.names.push(name.to_owned());
        }
        self.name_ids.insert(name.to_owned(), id);
        id
    }

    pub fn name_id(&self, name: &str) -> Option<u16> {
        self.name_ids.get(name).copied()
    }

    /// An interned agent or model name.
    pub fn name(&self, id: u16) -> &str {
        self.names.get(id as usize).map_or("", String::as_str)
    }

    pub fn events(&self) -> &[UsageEvent] {
        &self.events
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    #[cfg(test)]
    pub fn history_days(&self) -> u32 {
        self.history_days
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    pub fn set_quota(&mut self, agent: &str, report: QuotaReport) {
        self.quotas.insert(agent.to_owned(), report);
        self.generation += 1;
    }

    /// Drops per-event history older than `keep_ms` (session totals stay).
    pub fn prune(&mut self, now_ms: u64, keep_ms: u64) {
        let cutoff = now_ms.saturating_sub(keep_ms);
        let before = self.events.len();
        self.events.retain(|e| e.ts_ms >= cutoff);
        if self.events.len() != before {
            self.generation += 1;
        }
    }

    #[cfg(test)]
    pub fn sessions(&self) -> impl Iterator<Item = &SessionUsage> {
        self.sessions.values()
    }

    #[cfg(test)]
    pub fn session(&self, agent: &str, session_id: &str) -> Option<&SessionUsage> {
        self.sessions.get(&(agent.to_owned(), session_id.to_owned()))
    }

    /// The agent session most likely running in a terminal session: same
    /// agent and working directory, active since `since_ms`, most recent.
    pub fn latest_session(&self, agent: &str, cwd: &Path, since_ms: u64) -> Option<&SessionUsage> {
        self.sessions
            .values()
            .filter(|s| s.agent == agent && s.last_ms >= since_ms)
            .filter(|s| s.cwd.as_deref().is_some_and(|c| same_dir(c, cwd)))
            .max_by_key(|s| s.last_ms)
    }

    /// Sum of usage in `[from_ms, to_ms)`.
    pub fn usage_between(&self, agent: &str, from_ms: u64, to_ms: u64) -> TokenUsage {
        let mut total = TokenUsage::default();
        let Some(agent) = self.name_id(agent) else { return total };
        for e in self.events.iter().filter(|e| e.agent == agent && e.ts_ms >= from_ms && e.ts_ms < to_ms) {
            total += e.usage;
        }
        total
    }

    pub fn budget(&self, profile: &AgentProfile, session: Option<&SessionUsage>, now_ms: u64) -> BudgetReport {
        let budget = &profile.budget;
        let cache = budget.include_cache_reads;
        let window = WeeklyWindow::from_budget(budget);
        let (from, resets_at) = match window {
            WeeklyWindow::Fixed(reset) => {
                let start = reset.window_start(now_ms);
                (start, Some(start + WEEK_MS))
            }
            WeeklyWindow::Rolling => {
                let from = now_ms.saturating_sub(WEEK_MS);
                let agent = self.name_id(&profile.name);
                let oldest = self
                    .events
                    .iter()
                    .filter(|e| Some(e.agent) == agent && e.ts_ms >= from)
                    .map(|e| e.ts_ms)
                    .min();
                (from, oldest.map(|t| t + WEEK_MS))
            }
        };
        let weekly_used = self.usage_between(&profile.name, from, now_ms + 1).billable(cache);
        let mut weekly = budget.weekly_tokens.map(|limit| Gauge {
            used: weekly_used,
            limit,
            resets_in_ms: resets_at.map(|t| t.saturating_sub(now_ms)),
        });
        // An explicit quota command is authoritative.
        if let Some(q) = self.quotas.get(&profile.name)
            && let Some(limit) = q.limit.or(budget.weekly_tokens)
        {
            weekly = Some(Gauge {
                used: q.used,
                limit,
                resets_in_ms: q.resets_at_ms.map(|t| t.saturating_sub(now_ms)),
            });
        }
        let session = budget.session_tokens.zip(session).map(|(limit, s)| Gauge {
            used: s.usage.billable(cache),
            limit,
            resets_in_ms: None,
        });
        BudgetReport { session, weekly, weekly_used }
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().trim_end_matches(['/', '\\']).to_owned();
    norm(a) == norm(b)
}

// ---------------------------------------------------------------------------
// Weekly windows & time
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WeeklyWindow {
    Rolling,
    Fixed(WeeklyReset),
}

impl WeeklyWindow {
    fn from_budget(budget: &TokenBudget) -> Self {
        match budget.weekly_reset.as_deref().map(WeeklyReset::parse) {
            Some(Ok(reset)) => WeeklyWindow::Fixed(reset),
            Some(Err(e)) => {
                log::warn!("ignoring weekly_reset: {e}");
                WeeklyWindow::Rolling
            }
            None => WeeklyWindow::Rolling,
        }
    }
}

/// `"mon 09:00"` (UTC).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WeeklyReset {
    /// 0 = Monday … 6 = Sunday.
    pub weekday: u8,
    pub minute_of_day: u16,
}

impl WeeklyReset {
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut parts = s.split_whitespace();
        let day = parts.next().ok_or("empty weekly_reset")?.to_lowercase();
        let weekday = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
            .iter()
            .position(|d| day.starts_with(d))
            .ok_or_else(|| format!("unknown weekday in {s:?}"))? as u8;
        let minute_of_day = match parts.next() {
            None => 0,
            Some(t) => {
                let (h, m) = t.split_once(':').unwrap_or((t, "0"));
                let (h, m): (u16, u16) = (h.parse().map_err(|_| format!("bad hour in {s:?}"))?, m.parse().map_err(|_| format!("bad minute in {s:?}"))?);
                if h > 23 || m > 59 {
                    return Err(format!("time out of range in {s:?}"));
                }
                h * 60 + m
            }
        };
        Ok(Self { weekday, minute_of_day })
    }

    /// Start (ms) of the weekly window containing `now_ms`.
    pub fn window_start(&self, now_ms: u64) -> u64 {
        let day = now_ms / DAY_MS;
        let weekday = ((day + 3) % 7) as u8; // 1970-01-01 was a Thursday.
        let back = (weekday + 7 - self.weekday) % 7;
        let candidate = (day - back as u64) * DAY_MS + self.minute_of_day as u64 * MINUTE_MS;
        if candidate > now_ms { candidate - WEEK_MS } else { candidate }
    }
}

/// `950`, `12.3k`, `139.5k`, `1.2M`, `11.6B`.
pub fn format_tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3).replace(".0k", "k"),
        1_000_000..1_000_000_000 => format!("{:.1}M", n as f64 / 1e6).replace(".0M", "M"),
        _ => format!("{:.1}B", n as f64 / 1e9).replace(".0B", "B"),
    }
}

/// `5d 11h`, `3h 20m`, `45m`, `<1m`.
pub fn format_reset(ms: u64) -> String {
    let mins = ms / MINUTE_MS;
    match mins {
        0 => "<1m".into(),
        1..60 => format!("{mins}m"),
        60..1440 => format!("{}h {}m", mins / 60, mins % 60),
        _ => format!("{}d {}h", mins / 1440, (mins % 1440) / 60),
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Parses RFC 3339 / ISO 8601 (`2026-11-20T10:00:03.123Z`, `…+01:00`).
pub fn parse_timestamp_ms(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut rest = &s[19..];
    let mut millis = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        let padded = format!("{:0<3}", &frac[..digits.min(3)]);
        millis = padded.parse().ok()?;
        rest = &frac[digits..];
    }
    let offset_min = match rest {
        "" | "Z" | "z" => 0,
        tz if tz.len() == 6 && (tz.starts_with('+') || tz.starts_with('-')) => {
            let sign = if tz.starts_with('-') { -1 } else { 1 };
            sign * (tz[1..3].parse::<i64>().ok()? * 60 + tz[4..6].parse::<i64>().ok()?)
        }
        _ => return None,
    };
    let days = days_from_civil(y, mo, d);
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec - offset_min * 60;
    u64::try_from(secs * 1000 + millis).ok()
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ---------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------

fn u64_at(v: &Value, pointer: &str) -> u64 {
    match v.pointer(pointer) {
        Some(Value::Number(n)) => n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)).unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn str_at<'a>(v: &'a Value, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer).and_then(Value::as_str)
}

fn file_stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// One line of a Claude Code session log.
pub fn parse_claude_line(line: &str, path: &Path) -> Option<UsageRecord> {
    if !line.contains("\"usage\"") {
        return None; // fast path: most lines are user/tool messages
    }
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let msg = v.get("message")?;
    let usage = msg.get("usage")?;
    let dedupe_key = str_at(msg, "/id").map(|id| format!("{id}:{}", str_at(&v, "/requestId").unwrap_or("")));
    Some(UsageRecord {
        session_id: str_at(&v, "/sessionId").map(str::to_owned).unwrap_or_else(|| file_stem(path)),
        cwd: str_at(&v, "/cwd").map(PathBuf::from),
        model: str_at(msg, "/model").map(str::to_owned),
        timestamp_ms: str_at(&v, "/timestamp").and_then(parse_timestamp_ms),
        usage: TokenUsage {
            input: u64_at(usage, "/input_tokens"),
            output: u64_at(usage, "/output_tokens"),
            cache_read: u64_at(usage, "/cache_read_input_tokens"),
            cache_creation: u64_at(usage, "/cache_creation_input_tokens"),
        },
        dedupe_key,
        cumulative: false,
        turns: None,
    })
}

/// Per-file context for Codex rollouts (metadata appears before usage).
#[derive(Clone, Debug, Default)]
pub struct CodexContext {
    session_id: Option<String>,
    cwd: Option<PathBuf>,
    model: Option<String>,
}

pub fn parse_codex_line(line: &str, path: &Path, ctx: &mut CodexContext) -> Option<UsageRecord> {
    let v: Value = serde_json::from_str(line).ok()?;
    let payload = v.get("payload")?;
    match v.get("type")?.as_str()? {
        "session_meta" => {
            ctx.session_id = str_at(payload, "/id").map(str::to_owned);
            ctx.cwd = str_at(payload, "/cwd").map(PathBuf::from).or(ctx.cwd.take());
            None
        }
        "turn_context" => {
            ctx.model = str_at(payload, "/model").map(str::to_owned).or(ctx.model.take());
            ctx.cwd = str_at(payload, "/cwd").map(PathBuf::from).or(ctx.cwd.take());
            None
        }
        "event_msg" if str_at(payload, "/type") == Some("token_count") => {
            let total = payload.pointer("/info/total_token_usage")?;
            let input = u64_at(total, "/input_tokens");
            let cached = u64_at(total, "/cached_input_tokens");
            Some(UsageRecord {
                session_id: ctx.session_id.clone().unwrap_or_else(|| file_stem(path)),
                cwd: ctx.cwd.clone(),
                model: ctx.model.clone(),
                timestamp_ms: str_at(&v, "/timestamp").and_then(parse_timestamp_ms),
                usage: TokenUsage {
                    input: input.saturating_sub(cached),
                    output: u64_at(total, "/output_tokens"),
                    cache_read: cached,
                    cache_creation: 0,
                },
                dedupe_key: None,
                cumulative: true,
                turns: None,
            })
        }
        _ => None,
    }
}

/// A whole Gemini CLI chat file → one cumulative record.
pub fn parse_gemini_chat(text: &str, path: &Path) -> Option<UsageRecord> {
    let v: Value = serde_json::from_str(text).ok()?;
    let mut usage = TokenUsage::default();
    let mut model = None;
    let mut last_ts = None;
    let mut turns = 0u32;
    for msg in v.get("messages")?.as_array()? {
        let Some(tokens) = msg.get("tokens") else { continue };
        turns += 1;
        let input = u64_at(tokens, "/input");
        let cached = u64_at(tokens, "/cached");
        usage += TokenUsage {
            input: input.saturating_sub(cached),
            output: u64_at(tokens, "/output") + u64_at(tokens, "/thoughts"),
            cache_read: cached,
            cache_creation: 0,
        };
        model = str_at(msg, "/model").map(str::to_owned).or(model);
        last_ts = str_at(msg, "/timestamp").and_then(parse_timestamp_ms).or(last_ts);
    }
    Some(UsageRecord {
        session_id: str_at(&v, "/sessionId").map(str::to_owned).unwrap_or_else(|| file_stem(path)),
        cwd: None, // Gemini only stores a hash of the project path.
        model,
        timestamp_ms: last_ts.or_else(|| str_at(&v, "/lastUpdated").and_then(parse_timestamp_ms)),
        usage,
        dedupe_key: None,
        cumulative: true,
        turns: Some(turns),
    })
}

/// A line of a user-described JSONL log.
pub fn parse_generic_line(line: &str, path: &Path, spec: &JsonlSource) -> Option<UsageRecord> {
    let v: Value = serde_json::from_str(line).ok()?;
    let opt_u64 = |p: &Option<String>| p.as_deref().map(|p| u64_at(&v, p)).unwrap_or(0);
    let opt_str = |p: &Option<String>| p.as_deref().and_then(|p| v.pointer(p)).map(|x| match x {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    // Lines without the input field aren't usage lines.
    v.pointer(&spec.input_tokens)?;
    Some(UsageRecord {
        session_id: opt_str(&spec.session_id).unwrap_or_else(|| file_stem(path)),
        cwd: opt_str(&spec.cwd).map(PathBuf::from),
        model: opt_str(&spec.model),
        timestamp_ms: opt_str(&spec.timestamp).and_then(|t| {
            parse_timestamp_ms(&t).or_else(|| t.parse::<u64>().ok().map(|n| if n < 10_000_000_000 { n * 1000 } else { n }))
        }),
        usage: TokenUsage {
            input: u64_at(&v, &spec.input_tokens),
            output: u64_at(&v, &spec.output_tokens),
            cache_read: opt_u64(&spec.cache_read_tokens),
            cache_creation: opt_u64(&spec.cache_creation_tokens),
        },
        dedupe_key: opt_str(&spec.dedupe_key),
        cumulative: spec.cumulative,
        turns: None,
    })
}

/// `{"used": N, "limit": N?, "resets_at": "rfc3339" | epoch_secs?}`.
pub fn parse_quota_output(stdout: &str, now_ms: u64) -> Option<QuotaReport> {
    let v: Value = serde_json::from_str(stdout.trim()).ok()?;
    let used = v.get("used")?.as_u64()?;
    let limit = v.get("limit").and_then(Value::as_u64);
    let resets_at_ms = match v.get("resets_at") {
        Some(Value::String(s)) => parse_timestamp_ms(s),
        Some(Value::Number(n)) => n.as_u64().map(|s| s * 1000),
        _ => None,
    };
    Some(QuotaReport { used, limit, resets_at_ms, fetched_ms: now_ms })
}

// ---------------------------------------------------------------------------
// Sources & incremental file reading
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum LogFormat {
    Claude,
    Codex,
    Gemini,
    Jsonl(Box<JsonlSource>),
}

#[derive(Clone, Debug)]
struct LogSource {
    agent: String,
    format: LogFormat,
    root: PathBuf,
}

impl LogSource {
    fn accepts(&self, path: &Path) -> bool {
        if !path.starts_with(&self.root) {
            return false;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        match &self.format {
            LogFormat::Claude | LogFormat::Codex => ext == "jsonl",
            LogFormat::Gemini => ext == "json" && path.parent().is_some_and(|p| p.ends_with("chats")),
            LogFormat::Jsonl(spec) => ext == spec.extension.trim_start_matches('.'),
        }
    }
}

#[derive(Clone, Debug)]
struct CommandSource {
    agent: String,
    program: String,
    args: Vec<String>,
    interval: Duration,
}

fn sources_from(agents: &[AgentProfile]) -> (Vec<LogSource>, Vec<CommandSource>) {
    let mut logs = Vec::new();
    let mut commands = Vec::new();
    for agent in agents {
        let name = agent.name.clone();
        let (format, path) = match &agent.metrics {
            MetricsSource::None => continue,
            MetricsSource::ClaudeCode { path } => (LogFormat::Claude, path),
            MetricsSource::Codex { path } => (LogFormat::Codex, path),
            MetricsSource::GeminiCli { path } => (LogFormat::Gemini, path),
            MetricsSource::Jsonl(spec) => (LogFormat::Jsonl(spec.clone()), &spec.path),
            MetricsSource::Command { program, args, interval_secs } => {
                commands.push(CommandSource {
                    agent: name,
                    program: program.clone(),
                    args: args.clone(),
                    interval: Duration::from_secs((*interval_secs).max(5)),
                });
                continue;
            }
        };
        logs.push(LogSource { agent: name, format, root: expand_tilde(path) });
    }
    (logs, commands)
}

#[derive(Default)]
struct FileCursor {
    offset: u64,
    modified: Option<SystemTime>,
    partial: Vec<u8>,
    codex: CodexContext,
}

/// Files that changed this recently are also polled. On macOS, FSEvents only
/// reports content changes when the writer *closes* the file, so an agent
/// that keeps its log open (e.g. Codex) would otherwise go unnoticed.
const HOT_FILE_TTL: Duration = Duration::from_secs(15 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Owns read positions and feeds new lines into the store.
struct Ingestor {
    sources: Vec<LogSource>,
    cursors: HashMap<PathBuf, FileCursor>,
    hot: HashMap<PathBuf, Instant>,
}

impl Ingestor {
    fn new(sources: Vec<LogSource>) -> Self {
        Self { sources, cursors: HashMap::new(), hot: HashMap::new() }
    }

    /// Re-reads hot files whose size or mtime changed. Returns whether
    /// anything new was ingested.
    fn poll_hot(&mut self, store: &RwLock<UsageStore>) -> bool {
        self.hot.retain(|_, last| last.elapsed() < HOT_FILE_TTL);
        let stale: Vec<PathBuf> = self
            .hot
            .keys()
            .filter(|path| {
                let meta = std::fs::metadata(path).ok();
                let cursor = self.cursors.get(*path);
                match (meta, cursor) {
                    (Some(m), Some(c)) => m.len() != c.offset || m.modified().ok() != c.modified,
                    (Some(_), None) => true,
                    (None, _) => false,
                }
            })
            .cloned()
            .collect();
        let mut changed = false;
        for path in stale {
            changed |= self.ingest_file(&path, store);
        }
        changed
    }

    /// Reads whatever was appended to `path` since last time.
    fn ingest_file(&mut self, path: &Path, store: &RwLock<UsageStore>) -> bool {
        let Some(source) = self.sources.iter().find(|s| s.accepts(path)).cloned() else {
            return false;
        };
        let now = now_ms();
        let Ok(mut file) = File::open(path) else { return false };
        let meta = file.metadata().ok();
        let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = meta.and_then(|m| m.modified().ok());
        if modified.is_some_and(|m| m.elapsed().unwrap_or_default() < HOT_FILE_TTL) {
            self.hot.insert(path.to_path_buf(), Instant::now());
        }
        let cursor = self.cursors.entry(path.to_path_buf()).or_default();

        if let LogFormat::Gemini = source.format {
            // Rewritten in place on every message; small enough to re-read.
            if cursor.modified == modified && cursor.offset == len {
                return false;
            }
            (cursor.offset, cursor.modified) = (len, modified);
            let mut text = String::new();
            if file.read_to_string(&mut text).is_err() {
                return false;
            }
            return match parse_gemini_chat(&text, path) {
                Some(record) => {
                    store.write().ingest(&source.agent, record, now);
                    true
                }
                None => false,
            };
        }

        if len < cursor.offset {
            *cursor = FileCursor::default(); // truncated or replaced
        }
        cursor.modified = modified;
        if len == cursor.offset || file.seek(SeekFrom::Start(cursor.offset)).is_err() {
            return false;
        }
        let mut buf = std::mem::take(&mut cursor.partial);
        let Ok(read) = file.take(len - cursor.offset).read_to_end(&mut buf) else { return false };
        cursor.offset += read as u64;

        let complete = match buf.iter().rposition(|&b| b == b'\n') {
            Some(i) => {
                cursor.partial = buf.split_off(i + 1);
                buf
            }
            None => {
                cursor.partial = buf;
                return false;
            }
        };

        let mut records = Vec::new();
        for line in String::from_utf8_lossy(&complete).lines().filter(|l| !l.trim().is_empty()) {
            let record = match &source.format {
                LogFormat::Claude => parse_claude_line(line, path),
                LogFormat::Codex => parse_codex_line(line, path, &mut cursor.codex),
                LogFormat::Jsonl(spec) => parse_generic_line(line, path, spec),
                LogFormat::Gemini => None,
            };
            records.extend(record);
        }
        if records.is_empty() {
            return false;
        }
        let mut store = store.write();
        for record in records {
            store.ingest(&source.agent, record, now);
        }
        true
    }

    /// Pass over files modified in the last `days` days (`u32::MAX` = all).
    /// Files already read are only read from where they were left.
    fn scan(&mut self, days: u32, store: &RwLock<UsageStore>) {
        let cutoff = SystemTime::now().checked_sub(Duration::from_secs(days as u64 * 86_400)).unwrap_or(UNIX_EPOCH);
        let mut files = Vec::new();
        for source in &self.sources {
            collect_files(&source.root, 6, &mut |p, modified| {
                if modified >= cutoff && source.accepts(p) {
                    files.push((modified, p.to_path_buf()));
                }
            });
        }
        files.sort();
        for (_, path) in files {
            self.ingest_file(&path, store);
        }
    }
}

fn collect_files(dir: &Path, depth: u32, f: &mut impl FnMut(&Path, SystemTime)) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let path = entry.path();
        if meta.is_dir() {
            if depth > 0 {
                collect_files(&path, depth - 1, f);
            }
        } else if let Ok(modified) = meta.modified() {
            f(&path, modified);
        }
    }
}

/// Runs a quota command with a timeout; never blocks longer than `timeout`.
fn run_quota_command(cmd: &CommandSource, timeout: Duration) -> Option<QuotaReport> {
    let mut child = std::process::Command::new(&cmd.program)
        .args(&cmd.args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                log::warn!("quota command `{}` timed out", cmd.program);
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    parse_quota_output(&out, now_ms())
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

enum Msg {
    Fs(notify::Event),
    Reconfigure(Vec<AgentProfile>, u32),
    /// Scan this many days back (`u32::MAX` = everything).
    LoadHistory(u32),
    Shutdown,
}

/// Background monitor. Dropping it stops the thread.
pub struct AiMonitor {
    store: Arc<RwLock<UsageStore>>,
    tx: Sender<Msg>,
    thread: Option<JoinHandle<()>>,
}

impl AiMonitor {
    pub fn start(config: &AppConfig, wake: Wake) -> Self {
        let store = Arc::new(RwLock::new(UsageStore::default()));
        let (tx, rx) = unbounded();
        let agents = if config.monitor.enabled { config.agents.clone() } else { Vec::new() };
        let (thread_store, thread_tx) = (store.clone(), tx.clone());
        let scan_days = config.monitor.scan_days;
        let thread = std::thread::Builder::new()
            .name("ai-monitor".into())
            .spawn(move || worker(agents, scan_days, thread_store, thread_tx, rx, wake))
            .ok();
        Self { store, tx, thread }
    }

    pub fn store(&self) -> &Arc<RwLock<UsageStore>> {
        &self.store
    }

    /// Applies new agent profiles after a config hot-reload.
    pub fn reconfigure(&self, config: &AppConfig) {
        let agents = if config.monitor.enabled { config.agents.clone() } else { Vec::new() };
        let _ = self.tx.send(Msg::Reconfigure(agents, config.monitor.scan_days));
    }

    /// Makes sure at least `days` days of logs are loaded (`u32::MAX` = all),
    /// scanning older files in the background. Returns immediately; the
    /// store reports [`UsageStore::is_loading`] until the scan is done.
    pub fn load_history(&self, days: u32) {
        let mut store = self.store.write();
        if days <= store.history_days || store.loading {
            return;
        }
        store.loading = true;
        store.generation += 1;
        drop(store);
        let _ = self.tx.send(Msg::LoadHistory(days));
    }
}

impl Drop for AiMonitor {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn worker(
    agents: Vec<AgentProfile>,
    scan_days: u32,
    store: Arc<RwLock<UsageStore>>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    wake: Wake,
) {
    let mut state = WorkerState::setup(&agents, scan_days, &store, &tx);
    wake();
    // Deepest history asked for by a view; survives reconfiguration.
    let mut requested_days = 0;
    let mut last_prune = Instant::now();
    let mut last_poll = Instant::now();
    let mut pending: Option<Msg> = None;
    loop {
        let until_command = state.next_command_due().saturating_duration_since(Instant::now());
        let msg = match pending.take() {
            Some(msg) => Ok(msg),
            None => rx.recv_timeout(until_command.min(POLL_INTERVAL)),
        };
        let mut changed = false;
        match msg {
            Ok(Msg::Shutdown) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            Ok(Msg::Reconfigure(agents, days)) => {
                *store.write() = UsageStore::default();
                state = WorkerState::setup(&agents, days.max(requested_days), &store, &tx);
                changed = true;
            }
            Ok(Msg::LoadHistory(days)) => {
                requested_days = requested_days.max(days);
                if days > store.read().history_days {
                    state.ingestor.scan(days, &store);
                }
                let mut s = store.write();
                s.history_days = s.history_days.max(days);
                s.loading = false;
                s.generation += 1;
                changed = true;
            }
            Ok(Msg::Fs(event)) => {
                // Coalesce bursts (agents write many lines per second), without
                // swallowing control messages that arrive meanwhile.
                let mut paths: HashSet<PathBuf> = event.paths.into_iter().collect();
                loop {
                    match rx.recv_timeout(Duration::from_millis(30)) {
                        Ok(Msg::Fs(more)) => paths.extend(more.paths),
                        Ok(other) => {
                            pending = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                for path in paths {
                    changed |= state.ingestor.ingest_file(&path, &store);
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
        }
        if last_poll.elapsed() >= POLL_INTERVAL {
            changed |= state.ingestor.poll_hot(&store);
            last_poll = Instant::now();
        }
        changed |= state.run_due_commands(&store);
        if last_prune.elapsed() > Duration::from_secs(3600) {
            let mut s = store.write();
            let keep_days = s.history_days.max(8);
            s.prune(now_ms(), (keep_days as u64).saturating_mul(DAY_MS));
            drop(s);
            last_prune = Instant::now();
        }
        if changed {
            wake();
        }
    }
}

struct WorkerState {
    ingestor: Ingestor,
    commands: Vec<(CommandSource, Instant)>,
    _watcher: Option<notify::RecommendedWatcher>,
}

impl WorkerState {
    fn setup(agents: &[AgentProfile], scan_days: u32, store: &RwLock<UsageStore>, tx: &Sender<Msg>) -> Self {
        let (logs, commands) = sources_from(agents);
        let fs_tx = tx.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res
                && matches!(event.kind, notify::EventKind::Create(_) | notify::EventKind::Modify(_))
            {
                let _ = fs_tx.send(Msg::Fs(event));
            }
        });
        let watcher = match watcher {
            Ok(mut w) => {
                for source in &logs {
                    // Agents that aren't installed simply have no log dir.
                    if source.root.is_dir()
                        && let Err(e) = w.watch(&source.root, notify::RecursiveMode::Recursive)
                    {
                        log::warn!("cannot watch {}: {e}", source.root.display());
                    }
                }
                Some(w)
            }
            Err(e) => {
                log::warn!("AI monitor file watching unavailable: {e}");
                None
            }
        };
        let mut ingestor = Ingestor::new(logs);
        ingestor.scan(scan_days, store);
        let mut s = store.write();
        s.history_days = scan_days;
        s.generation += 1;
        drop(s);
        let now = Instant::now();
        Self { ingestor, commands: commands.into_iter().map(|c| (c, now)).collect(), _watcher: watcher }
    }

    fn next_command_due(&self) -> Instant {
        self.commands.iter().map(|(_, due)| *due).min().unwrap_or_else(|| Instant::now() + Duration::from_secs(60))
    }

    fn run_due_commands(&mut self, store: &RwLock<UsageStore>) -> bool {
        let mut changed = false;
        for (cmd, due) in &mut self.commands {
            if Instant::now() < *due {
                continue;
            }
            *due = Instant::now() + cmd.interval;
            if let Some(report) = run_quota_command(cmd, Duration::from_secs(10)) {
                store.write().set_quota(&cmd.agent, report);
                changed = true;
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_agents;
    use std::io::Write;

    const CLAUDE_FIXTURE: &str = include_str!("../tests/fixtures/claude-session.jsonl");
    const CODEX_FIXTURE: &str = include_str!("../tests/fixtures/codex-rollout.jsonl");
    const GEMINI_FIXTURE: &str = include_str!("../tests/fixtures/gemini-chat.json");
    const CLAUDE_SESSION: &str = "3f2a9c1e-7b4d-4e8a-9f10-2c5d6e7f8a9b";

    fn ingest_lines(store: &mut UsageStore, agent: &str, text: &str, mut parse: impl FnMut(&str) -> Option<UsageRecord>) {
        for line in text.lines() {
            if let Some(r) = parse(line) {
                store.ingest(agent, r, 0);
            }
        }
    }

    #[test]
    fn claude_jsonl_tokens_are_extracted_and_deduplicated() {
        let path = Path::new("/x/session.jsonl");
        let mut store = UsageStore::default();
        ingest_lines(&mut store, "Claude Code", CLAUDE_FIXTURE, |l| parse_claude_line(l, path));

        let s = store.session("Claude Code", CLAUDE_SESSION).expect("session");
        // msg_01A appears twice (5 → 350 output tokens) and counts once.
        assert_eq!(s.usage, TokenUsage { input: 14, output: 470, cache_read: 32_000, cache_creation: 2_500 });
        assert_eq!(s.model.as_deref(), Some("claude-opus-4-5")); // <synthetic> ignored
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/Users/dev/midterm")));
        assert_eq!(s.first_ms, parse_timestamp_ms("2026-11-20T10:00:03.000Z").unwrap());
        assert_eq!(s.last_ms, parse_timestamp_ms("2026-11-20T10:01:01.000Z").unwrap());
        assert_eq!(store.sessions().count(), 1);
    }

    #[test]
    fn reingesting_the_same_claude_lines_does_not_double_count() {
        let path = Path::new("/x/session.jsonl");
        let mut store = UsageStore::default();
        for _ in 0..2 {
            ingest_lines(&mut store, "Claude Code", CLAUDE_FIXTURE, |l| parse_claude_line(l, path));
        }
        assert_eq!(store.session("Claude Code", CLAUDE_SESSION).unwrap().usage.output, 470);
    }

    #[test]
    fn codex_cumulative_totals() {
        let path = Path::new("/x/rollout-2026-11-20.jsonl");
        let mut ctx = CodexContext::default();
        let mut store = UsageStore::default();
        ingest_lines(&mut store, "Codex", CODEX_FIXTURE, |l| parse_codex_line(l, path, &mut ctx));
        let s = store.session("Codex", "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").unwrap();
        assert_eq!(s.usage, TokenUsage { input: 9_000, output: 900, cache_read: 11_000, cache_creation: 0 });
        assert_eq!(s.model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(s.cwd.as_deref(), Some(Path::new("/Users/dev/midulive")));
        // Two increments → two weekly events, summing to the total.
        let week = store.usage_between("Codex", 0, u64::MAX);
        assert_eq!(week, s.usage);
    }

    #[test]
    fn gemini_chat_file() {
        let r = parse_gemini_chat(GEMINI_FIXTURE, Path::new("/x/chats/session-1.json")).unwrap();
        assert_eq!(r.session_id, "b7c8d9e0-1f2a-4b3c-8d4e-5f6a7b8c9d0e");
        assert_eq!(r.usage, TokenUsage { input: 7_000, output: 550, cache_read: 5_000, cache_creation: 0 });
        assert_eq!(r.model.as_deref(), Some("gemini-2.5-pro"));
        assert!(r.cumulative);
        // Re-reading the rewritten file must not double count.
        let mut store = UsageStore::default();
        store.ingest("Gemini CLI", r.clone(), 0);
        store.ingest("Gemini CLI", r, 0);
        assert_eq!(store.session("Gemini CLI", "b7c8d9e0-1f2a-4b3c-8d4e-5f6a7b8c9d0e").unwrap().usage.output, 550);
    }

    #[test]
    fn turns_count_model_calls_once() {
        let path = Path::new("/x/session.jsonl");
        let mut store = UsageStore::default();
        for _ in 0..2 {
            ingest_lines(&mut store, "Claude Code", CLAUDE_FIXTURE, |l| parse_claude_line(l, path));
        }
        // msg_01A (streamed twice) + msg_01B; the zero-usage <synthetic> isn't a call.
        assert_eq!(store.session("Claude Code", CLAUDE_SESSION).unwrap().turns, 2);
        let events = store.events();
        assert_eq!(events.iter().map(|e| e.turns).sum::<u32>(), 2);
        assert!(events.iter().all(|e| store.name(e.agent) == "Claude Code"));
        assert!(events.iter().all(|e| store.name(e.model) == "claude-opus-4-5"));

        let mut ctx = CodexContext::default();
        let codex = Path::new("/x/rollout.jsonl");
        ingest_lines(&mut store, "Codex", CODEX_FIXTURE, |l| parse_codex_line(l, codex, &mut ctx));
        assert_eq!(store.session("Codex", "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").unwrap().turns, 2);

        // Gemini states its running total; re-reading the file adds nothing.
        let r = parse_gemini_chat(GEMINI_FIXTURE, Path::new("/x/chats/s.json")).unwrap();
        assert_eq!(r.turns, Some(2));
        store.ingest("Gemini CLI", r.clone(), 0);
        store.ingest("Gemini CLI", r, 0);
        assert_eq!(store.session("Gemini CLI", "b7c8d9e0-1f2a-4b3c-8d4e-5f6a7b8c9d0e").unwrap().turns, 2);
        assert_eq!(store.name(NO_MODEL), "");
    }

    #[test]
    fn generic_jsonl_with_pointers() {
        let spec = JsonlSource {
            path: "~/logs".into(),
            extension: "jsonl".into(),
            input_tokens: "/usage/prompt".into(),
            output_tokens: "/usage/completion".into(),
            cache_read_tokens: None,
            cache_creation_tokens: None,
            session_id: Some("/sid".into()),
            timestamp: Some("/ts".into()),
            cwd: Some("/dir".into()),
            model: Some("/model".into()),
            dedupe_key: None,
            cumulative: false,
        };
        let spec = Box::new(spec);
        let line = r#"{"sid":"abc","ts":1763632800,"dir":"/w","model":"llama","usage":{"prompt":"12","completion":30}}"#;
        let r = parse_generic_line(line, Path::new("/logs/a.jsonl"), &spec).unwrap();
        assert_eq!(r.session_id, "abc");
        assert_eq!(r.usage.input, 12);
        assert_eq!(r.usage.output, 30);
        assert_eq!(r.timestamp_ms, Some(1_763_632_800_000));
        assert!(parse_generic_line(r#"{"other":1}"#, Path::new("/a.jsonl"), &spec).is_none());
    }

    #[test]
    fn compact_formats() {
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(12_345), "12.3k");
        assert_eq!(format_tokens(139_500), "139.5k");
        assert_eq!(format_tokens(2_000), "2k");
        assert_eq!(format_tokens(1_250_000), "1.2M");
        assert_eq!(format_tokens(11_600_000_000), "11.6B");
        assert_eq!(format_tokens(3_000_000_000), "3B");
        assert_eq!(format_reset(30_000), "<1m");
        assert_eq!(format_reset(45 * MINUTE_MS), "45m");
        assert_eq!(format_reset(200 * MINUTE_MS), "3h 20m");
        assert_eq!(format_reset(5 * DAY_MS + 11 * 60 * MINUTE_MS), "5d 11h");
    }

    #[test]
    fn timestamps() {
        assert_eq!(parse_timestamp_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_timestamp_ms("2026-11-20T10:00:03.5Z"), Some(1_795_168_803_500));
        assert_eq!(parse_timestamp_ms("2026-11-20T11:00:03.500+01:00"), Some(1_795_168_803_500));
        assert_eq!(parse_timestamp_ms("2026-11-20T10:00:03.123456Z"), Some(1_795_168_803_123));
        assert_eq!(parse_timestamp_ms("2024-02-29T00:00:00Z"), Some(1_709_164_800_000));
        assert_eq!(parse_timestamp_ms("not a date"), None);
    }

    #[test]
    fn fixed_weekly_reset_window() {
        let reset = WeeklyReset::parse("Mon 09:00").unwrap();
        assert_eq!(reset, WeeklyReset { weekday: 0, minute_of_day: 540 });
        // Friday 2026-11-20 10:00Z → window started Monday 2026-11-16 09:00Z.
        let now = parse_timestamp_ms("2026-11-20T10:00:00Z").unwrap();
        assert_eq!(reset.window_start(now), parse_timestamp_ms("2026-11-16T09:00:00Z").unwrap());
        // Monday 08:59 still belongs to the previous week.
        let early = parse_timestamp_ms("2026-11-16T08:59:00Z").unwrap();
        assert_eq!(reset.window_start(early), parse_timestamp_ms("2026-11-09T09:00:00Z").unwrap());
        assert!(WeeklyReset::parse("funday").is_err());
        assert!(WeeklyReset::parse("mon 25:00").is_err());
    }

    #[test]
    fn budget_report_against_config_limits() {
        let mut profile = default_agents().remove(0);
        profile.budget = TokenBudget {
            session_tokens: Some(10_000),
            weekly_tokens: Some(4_000),
            weekly_reset: Some("mon 09:00".into()),
            include_cache_reads: false,
        };
        let mut store = UsageStore::default();
        let path = Path::new("/x/s.jsonl");
        ingest_lines(&mut store, &profile.name, CLAUDE_FIXTURE, |l| parse_claude_line(l, path));

        let now = parse_timestamp_ms("2026-11-20T12:00:00Z").unwrap();
        let session = store.latest_session(&profile.name, Path::new("/Users/dev/midterm/"), 0);
        assert!(session.is_some());
        let report = store.budget(&profile, session, now);
        // billable = input 14 + output 470 + cache_creation 2500 (cache reads excluded)
        let weekly = report.weekly.unwrap();
        assert_eq!(weekly.used, 2_984);
        assert_eq!(weekly.percent_used(), 75);
        assert_eq!(weekly.remaining(), 1_016);
        assert_eq!(weekly.resets_in_ms, Some(parse_timestamp_ms("2026-11-23T09:00:00Z").unwrap() - now));
        assert_eq!(report.session.unwrap().used, 2_984);

        // Next week: nothing used yet.
        let later = parse_timestamp_ms("2026-11-24T12:00:00Z").unwrap();
        assert_eq!(store.budget(&profile, None, later).weekly_used, 0);
        // Only sessions active since `since` and in the same cwd match.
        assert!(store.latest_session(&profile.name, Path::new("/elsewhere"), 0).is_none());
        assert!(store.latest_session(&profile.name, Path::new("/Users/dev/midterm"), later).is_none());
    }

    #[test]
    fn quota_command_output_overrides_weekly_gauge() {
        let r = parse_quota_output(r#"{"used": 150, "limit": 1000, "resets_at": "2026-11-23T09:00:00Z"}"#, 7).unwrap();
        assert_eq!(r.used, 150);
        let mut store = UsageStore::default();
        store.set_quota("Claude Code", r);
        let profile = default_agents().remove(0);
        let now = parse_timestamp_ms("2026-11-20T12:00:00Z").unwrap();
        let weekly = store.budget(&profile, None, now).weekly.unwrap();
        assert_eq!((weekly.used, weekly.limit, weekly.percent_used()), (150, 1000, 15));
        assert!(parse_quota_output("garbage", 0).is_none());
    }

    #[test]
    fn incremental_reads_handle_partial_lines_and_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("projects");
        let file = root.join("-Users-dev-midterm/s.jsonl");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let source = LogSource { agent: "Claude Code".into(), format: LogFormat::Claude, root };
        let mut ingestor = Ingestor::new(vec![source]);
        let store = RwLock::new(UsageStore::default());

        let lines: Vec<&str> = CLAUDE_FIXTURE.lines().collect();
        let (head, tail) = lines.split_at(4); // ends right after msg_01A's 2nd line
        let mut f = std::fs::File::create(&file).unwrap();
        write!(f, "{}\n{}", head.join("\n"), &tail[0][..20]).unwrap(); // + half a line
        f.flush().unwrap();
        assert!(ingestor.ingest_file(&file, &store));
        assert_eq!(store.read().session("Claude Code", CLAUDE_SESSION).unwrap().usage.output, 350);

        write!(f, "{}\n{}\n", &tail[0][20..], tail[1..].join("\n")).unwrap();
        f.flush().unwrap();
        assert!(ingestor.ingest_file(&file, &store));
        assert_eq!(store.read().session("Claude Code", CLAUDE_SESSION).unwrap().usage.output, 470);

        // Unrelated files are ignored.
        assert!(!ingestor.ingest_file(&dir.path().join("other.jsonl"), &store));
    }

    #[test]
    fn monitor_scans_and_follows_logs_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("claude-projects");
        let project = root.join("-Users-dev-midterm");
        std::fs::create_dir_all(&project).unwrap();
        let lines: Vec<&str> = CLAUDE_FIXTURE.lines().collect();
        std::fs::write(project.join("old.jsonl"), format!("{}\n", lines[..4].join("\n"))).unwrap();

        let mut config = AppConfig::with_defaults();
        config.agents.truncate(1);
        config.agents[0].metrics = MetricsSource::ClaudeCode { path: root.to_string_lossy().into_owned() };
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded();
        let monitor = AiMonitor::start(&config, Arc::new(move || {
            let _ = wake_tx.send(());
        }));

        let output = |m: &AiMonitor| m.store().read().session("Claude Code", CLAUDE_SESSION).map(|s| s.usage.output);
        let wait_for = |want: u64| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while output(&monitor) != Some(want) && Instant::now() < deadline {
                let _ = wake_rx.recv_timeout(Duration::from_millis(100));
            }
            assert_eq!(output(&monitor), Some(want));
        };
        wait_for(350); // initial scan

        // Writer that keeps the file open (like Codex): FSEvents stays quiet
        // on macOS until close, so this exercises the hot-file polling.
        let mut f = std::fs::OpenOptions::new().append(true).open(project.join("old.jsonl")).unwrap();
        writeln!(f, "{}", lines[4..6].join("\n")).unwrap();
        wait_for(470);

        // A brand-new session file written with open-append-close (like
        // Claude Code): picked up through notify.
        let other = lines[5].replace(CLAUDE_SESSION, "second-session").replace("msg_01B", "msg_02");
        std::fs::write(project.join("new.jsonl"), format!("{other}\n")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let second = |m: &AiMonitor| m.store().read().session("Claude Code", "second-session").map(|s| s.usage.output);
        while second(&monitor).is_none() && Instant::now() < deadline {
            let _ = wake_rx.recv_timeout(Duration::from_millis(100));
        }
        assert_eq!(second(&monitor), Some(120));
        drop(f);

        // Hot-reload: pointing the agent elsewhere resets and rescans.
        let other_root = root.with_file_name("elsewhere");
        std::fs::create_dir_all(other_root.join("p")).unwrap();
        std::fs::write(other_root.join("p/r.jsonl"), CODEX_FIXTURE).unwrap();
        config.agents[0].metrics = MetricsSource::Codex { path: other_root.to_string_lossy().into_owned() };
        monitor.reconfigure(&config);
        let codex_id = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
        let codex = |m: &AiMonitor| m.store().read().session("Claude Code", codex_id).map(|s| s.usage.output);
        let deadline = Instant::now() + Duration::from_secs(5);
        while codex(&monitor).is_none() && Instant::now() < deadline {
            let _ = wake_rx.recv_timeout(Duration::from_millis(100));
        }
        assert_eq!(codex(&monitor), Some(900));
        assert!(output(&monitor).is_none(), "old data is dropped on reconfigure");
    }

    #[test]
    fn history_is_loaded_on_demand_without_double_counting() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("claude-projects");
        let project = root.join("-Users-dev-midterm");
        std::fs::create_dir_all(&project).unwrap();
        let old = project.join("old.jsonl");
        std::fs::write(&old, CLAUDE_FIXTURE).unwrap();
        // Last written 40 days ago: outside the startup scan.
        let forty_days_ago = SystemTime::now() - Duration::from_secs(40 * 86_400);
        std::fs::File::options().write(true).open(&old).unwrap().set_modified(forty_days_ago).unwrap();

        let mut config = AppConfig::with_defaults();
        config.agents.truncate(1);
        config.agents[0].metrics = MetricsSource::ClaudeCode { path: root.to_string_lossy().into_owned() };
        config.monitor.scan_days = 8;
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded();
        let monitor = AiMonitor::start(&config, Arc::new(move || {
            let _ = wake_tx.send(());
        }));
        let wait = |done: &dyn Fn(&UsageStore) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done(&monitor.store().read()) && Instant::now() < deadline {
                let _ = wake_rx.recv_timeout(Duration::from_millis(100));
            }
            assert!(done(&monitor.store().read()));
        };
        wait(&|s| s.history_days() == 8);
        assert!(monitor.store().read().session("Claude Code", CLAUDE_SESSION).is_none());

        monitor.load_history(30); // not deep enough
        wait(&|s| s.history_days() == 30 && !s.is_loading());
        assert!(monitor.store().read().session("Claude Code", CLAUDE_SESSION).is_none());

        monitor.load_history(u32::MAX);
        wait(&|s| s.history_days() == u32::MAX && !s.is_loading());
        let output = || monitor.store().read().session("Claude Code", CLAUDE_SESSION).map(|s| s.usage.output);
        assert_eq!(output(), Some(470));

        // Asking again (or for less) changes nothing.
        let generation = monitor.store().read().generation();
        monitor.load_history(u32::MAX);
        monitor.load_history(90);
        assert!(!monitor.store().read().is_loading());
        assert_eq!(monitor.store().read().generation(), generation);
        assert_eq!(output(), Some(470));
    }

    #[cfg(unix)]
    #[test]
    fn quota_command_runs_locally() {
        let cmd = CommandSource {
            agent: "x".into(),
            program: "/bin/sh".into(),
            args: vec!["-c".into(), r#"echo '{"used": 42, "limit": 100}'"#.into()],
            interval: Duration::from_secs(60),
        };
        let r = run_quota_command(&cmd, Duration::from_secs(5)).unwrap();
        assert_eq!((r.used, r.limit), (42, Some(100)));

        let slow = CommandSource { args: vec!["-c".into(), "sleep 5".into()], ..cmd };
        assert!(run_quota_command(&slow, Duration::from_millis(200)).is_none());
    }
}
