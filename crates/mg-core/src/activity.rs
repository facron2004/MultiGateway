use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use chrono::Local;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

pub const MEMORY_ROWS: usize = 2000;

/// Upper bound for `requests.jsonl`. Older lines are dropped at startup so the log
/// cannot grow forever and slow down every subsequent launch.
pub const MAX_LOG_LINES: usize = 20_000;

/// Truncates to at most `max_chars` characters without splitting a UTF-8 sequence.
///
/// Session ids arrive from the `x-session-id` request header, so slicing at a byte
/// offset would panic mid-character and abort the process under `panic = "abort"`.
fn char_prefix(value: &str, max_chars: usize) -> &str {
    match value.char_indices().nth(max_chars) {
        Some((index, _)) => &value[..index],
        None => value,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub protocol: String,
    pub port: u16,
    pub timestamp: f64,
    pub status: String, // "processing", "success", "failed", "cancelled"
    pub http_status: Option<u16>,
    pub duration_ms: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub account: String,
    pub error: String,
    pub session_id: String,
    pub session_source: String,
    #[serde(skip)]
    pub start_instant: Option<Instant>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaySummary {
    pub requests: u64,
    pub failed: u64,
    pub tokens: u64,
    pub metered: u64,
}

pub struct Activity {
    path: PathBuf,
    active: RwLock<HashMap<String, RequestRecord>>,
    rows: RwLock<VecDeque<RequestRecord>>,
    daily: RwLock<HashMap<(String, String), DaySummary>>, // (date_str, provider) -> DaySummary
    daily_accounts: RwLock<HashMap<(String, String), HashMap<String, DaySummary>>>,
    revision: AtomicU64,
    board_cache: RwLock<Option<(u64, serde_json::Value)>>,
}

impl Activity {
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        let path = data_dir.as_ref().join("requests.jsonl");
        let activity = Self {
            path: path.clone(),
            active: RwLock::new(HashMap::new()),
            rows: RwLock::new(VecDeque::with_capacity(MEMORY_ROWS)),
            daily: RwLock::new(HashMap::new()),
            daily_accounts: RwLock::new(HashMap::new()),
            revision: AtomicU64::new(0),
            board_cache: RwLock::new(None),
        };
        activity.load_history();
        activity
    }

    fn load_history(&self) {
        let Ok(file) = File::open(&self.path) else {
            return;
        };

        // map_while stops at the first read error; flatten() would keep retrying a
        // persistently failing reader forever.
        let mut lines: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();

        // requests.jsonl grows without bound and is reparsed in full on every start,
        // so trim it once here rather than paying that cost forever.
        if lines.len() > MAX_LOG_LINES {
            let dropped = lines.len() - MAX_LOG_LINES;
            lines.drain(..dropped);
            self.rewrite_log(&lines);
        }

        {
            let mut rows = self.rows.write();
            let mut daily = self.daily.write();
            let mut daily_accounts = self.daily_accounts.write();

            for line in &lines {
                if let Ok(record) = serde_json::from_str::<RequestRecord>(line) {
                    Self::count_record(&mut daily, &mut daily_accounts, &record);
                    if rows.len() >= MEMORY_ROWS {
                        rows.pop_front();
                    }
                    rows.push_back(record);
                }
            }
        }
    }

    fn rewrite_log(&self, lines: &[String]) {
        let tmp = self.path.with_extension("jsonl.trim.tmp");
        let written = (|| -> std::io::Result<()> {
            let mut file = File::create(&tmp)?;
            for line in lines {
                writeln!(file, "{}", line)?;
            }
            file.flush()
        })();

        match written {
            Ok(()) => {
                if std::fs::rename(&tmp, &self.path).is_err() {
                    let _ = std::fs::remove_file(&tmp);
                }
            }
            Err(_) => {
                // Keep the existing history rather than trading it for a partial file.
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }

    fn count_record(
        daily: &mut HashMap<(String, String), DaySummary>,
        daily_accounts: &mut HashMap<(String, String), HashMap<String, DaySummary>>,
        record: &RequestRecord,
    ) {
        let date = chrono::DateTime::from_timestamp(record.timestamp.floor() as i64, 0)
            .map(|timestamp| timestamp.with_timezone(&Local).format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| Local::now().format("%Y-%m-%d").to_string());
        let key = (date.clone(), record.provider.clone());
        let summary = daily.entry(key).or_default();
        summary.requests += 1;
        if record.status == "failed" {
            summary.failed += 1;
        }
        let tokens = record.input_tokens.unwrap_or(0) + record.output_tokens.unwrap_or(0);
        summary.tokens += tokens;
        if record.input_tokens.is_some() || record.output_tokens.is_some() {
            summary.metered += 1;
        }

        let today = Local::now().format("%Y-%m-%d").to_string();
        if date == today && !record.account.is_empty() && record.status != "processing" {
            let account_summary = daily_accounts
                .entry((date, record.provider.clone()))
                .or_default()
                .entry(record.account.clone())
                .or_default();
            account_summary.requests += 1;
            if record.input_tokens.is_some() || record.output_tokens.is_some() {
                account_summary.tokens +=
                    record.input_tokens.unwrap_or(0) + record.output_tokens.unwrap_or(0);
                account_summary.metered += 1;
            }
        }
    }

    pub fn begin(
        &self,
        provider: &str,
        model: &str,
        protocol: &str,
        port: u16,
        session: Option<&str>,
    ) -> RequestRecord {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let session_id = match session {
            Some(s) if !s.is_empty() => format!("ses_{}", char_prefix(s, 16)),
            _ => format!("req_{}", char_prefix(&id, 16)),
        };

        let now_sec = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        let record = RequestRecord {
            id: id.clone(),
            provider: provider.to_string(),
            model: model.to_string(),
            protocol: protocol.to_string(),
            port,
            timestamp: now_sec,
            status: "processing".to_string(),
            http_status: None,
            duration_ms: None,
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            reasoning_tokens: None,
            ttft_ms: None,
            account: String::new(),
            error: String::new(),
            session_id,
            session_source: if session.is_some() { "client".to_string() } else { "request".to_string() },
            start_instant: Some(Instant::now()),
        };

        self.active.write().insert(id, record.clone());
        self.revision.fetch_add(1, Ordering::Relaxed);
        record
    }

    pub fn record_first_token(&self, id: &str) {
        if let Some(record) = self.active.write().get_mut(id) {
            if record.ttft_ms.is_none() {
                if let Some(start) = record.start_instant {
                    record.ttft_ms = Some(start.elapsed().as_millis() as u64);
                }
            }
        }
    }

    pub fn finish(
        &self,
        mut record: RequestRecord,
        status: u16,
        error_msg: Option<String>,
        disconnected: bool,
    ) {
        let duration = record
            .start_instant
            .map(|s| s.elapsed().as_millis() as u64)
            .unwrap_or(0);
        record.duration_ms = Some(duration);
        record.http_status = Some(status);

        if let Some(err) = error_msg {
            record.error = err;
        }

        record.status = if disconnected {
            "cancelled".to_string()
        } else if status < 400 && record.error.is_empty() {
            "success".to_string()
        } else {
            "failed".to_string()
        };

        // Remove from active
        self.active.write().remove(&record.id);

        // Update daily count
        {
            let mut daily = self.daily.write();
            let mut daily_accounts = self.daily_accounts.write();
            Self::count_record(&mut daily, &mut daily_accounts, &record);
        }

        // Write to log file
        self.append_to_file(&record);

        // Push to rows
        {
            let mut rows = self.rows.write();
            if rows.len() >= MEMORY_ROWS {
                rows.pop_front();
            }
            rows.push_back(record);
        }

        self.revision.fetch_add(1, Ordering::Relaxed);
    }

    fn append_to_file(&self, record: &RequestRecord) {
        if let Ok(json) = serde_json::to_string(record) {
            if let Ok(mut file) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            {
                let _ = writeln!(file, "{}", json);
            }
        }
    }

    pub fn account_usage(&self, provider: &str) -> HashMap<String, DaySummary> {
        let today = Local::now().format("%Y-%m-%d").to_string();
        self.daily_accounts
            .read()
            .get(&(today, provider.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    pub fn view(&self, provider: &str) -> serde_json::Value {
        let active = self.active.read();
        let rows = self.rows.read();
        let daily = self.daily.read();

        let date = Local::now().format("%Y-%m-%d").to_string();
        let summary = daily
            .get(&(date, provider.to_string()))
            .cloned()
            .unwrap_or_default();

        let in_flight = active.values().filter(|r| r.provider == provider).count();

        let mut records: Vec<RequestRecord> = active
            .values()
            .filter(|r| r.provider == provider)
            .cloned()
            .collect();

        for r in rows.iter().rev() {
            if r.provider == provider {
                records.push(r.clone());
                if records.len() >= 200 {
                    break;
                }
            }
        }

        serde_json::json!({
            "records": records,
            "summary": {
                "requests": summary.requests,
                "failed": summary.failed,
                "tokens": summary.tokens,
                "metered": summary.metered,
                "in_flight": in_flight
            }
        })
    }

    pub fn board(&self) -> serde_json::Value {
        // Building the board clones every retained record and re-serializes each one,
        // so the dashboard's 10s poll is served from cache until traffic changes it.
        let revision = self.revision.load(Ordering::Relaxed);
        if let Some((cached_revision, value)) = self.board_cache.read().as_ref() {
            if *cached_revision == revision {
                return value.clone();
            }
        }

        let value = self.compute_board(revision);
        *self.board_cache.write() = Some((revision, value.clone()));
        value
    }

    fn compute_board(&self, revision: u64) -> serde_json::Value {
        let active = self.active.read();
        let rows = self.rows.read();

        let mut all_records: Vec<RequestRecord> = active.values().cloned().collect();
        all_records.extend(rows.iter().cloned());
        all_records.sort_by(|a, b| b.timestamp.total_cmp(&a.timestamp));

        let mut provider_counts = HashMap::<String, u64>::new();
        let mut model_counts = HashMap::<(String, String), u64>::new();
        let mut account_counts = HashMap::<(String, String), u64>::new();
        let mut board_rows = Vec::with_capacity(all_records.len());
        for record in &all_records {
            *provider_counts.entry(record.provider.clone()).or_default() += 1;
            if !record.model.is_empty() {
                *model_counts
                    .entry((record.provider.clone(), record.model.clone()))
                    .or_default() += 1;
            }
            if !record.account.is_empty() {
                *account_counts
                    .entry((record.provider.clone(), record.account.clone()))
                    .or_default() += 1;
            }

            let mut row = serde_json::to_value(record).unwrap_or_default();
            if let Some(object) = row.as_object_mut() {
                object.insert("account_name".to_string(), serde_json::json!(record.account));
                if let Some(input) = record.input_tokens {
                    object.insert(
                        "new_input_tokens".to_string(),
                        serde_json::json!(input.saturating_sub(record.cached_tokens.unwrap_or(0))),
                    );
                }
                if let (Some(output), Some(duration)) = (record.output_tokens, record.duration_ms) {
                    let generation_ms = duration.saturating_sub(record.ttft_ms.unwrap_or(0));
                    if generation_ms > 200 {
                        object.insert(
                            "tokens_per_sec".to_string(),
                            serde_json::json!(output as f64 / (generation_ms as f64 / 1000.0)),
                        );
                    }
                }
            }
            board_rows.push(row);
        }

        let mut providers: Vec<_> = provider_counts
            .into_iter()
            .map(|(id, count)| serde_json::json!({"id": id, "count": count}))
            .collect();
        providers.sort_by(|a, b| {
            b["count"]
                .as_u64()
                .cmp(&a["count"].as_u64())
                .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
        });

        let mut models: Vec<_> = model_counts
            .into_iter()
            .map(|((provider, model), count)| {
                serde_json::json!({"provider": provider, "model": model, "count": count})
            })
            .collect();
        models.sort_by(|a, b| {
            b["count"]
                .as_u64()
                .cmp(&a["count"].as_u64())
                .then_with(|| a["model"].as_str().cmp(&b["model"].as_str()))
        });

        let mut accounts: Vec<_> = account_counts
            .into_iter()
            .map(|((provider, id), count)| {
                serde_json::json!({"provider": provider, "id": id, "name": id, "count": count})
            })
            .collect();
        accounts.sort_by(|a, b| {
            b["count"]
                .as_u64()
                .cmp(&a["count"].as_u64())
                .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
        });

        serde_json::json!({
            "active": active.values().cloned().collect::<Vec<_>>(),
            "rows": board_rows,
            "facets": {"providers": providers, "models": models, "accounts": accounts},
            "generated_at": chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
            "revision": revision,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Activity, MAX_LOG_LINES, MEMORY_ROWS};

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mg-rust-activity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn historical_requests_are_counted_on_their_recorded_day() {
        let dir = temp_dir();
        let activity = Activity::new(&dir);
        let mut record = activity.begin("grok", "grok-test", "chat", 8791, None);
        record.timestamp = 0.0;
        record.account = "account-1".to_string();
        record.input_tokens = Some(12);
        activity.finish(record, 200, None, false);

        let today = activity.view("grok")["summary"]["requests"]
            .as_u64()
            .unwrap();
        assert_eq!(today, 0);
        assert!(activity.account_usage("grok").is_empty());

        drop(activity);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn board_returns_rows_and_facets_expected_by_the_usage_page() {
        let dir = temp_dir();
        let activity = Activity::new(&dir);
        let mut record = activity.begin("trae", "gpt-test", "chat", 8791, None);
        record.account = "account-1".to_string();
        record.input_tokens = Some(20);
        record.output_tokens = Some(5);
        record.cached_tokens = Some(4);
        activity.finish(record, 200, None, false);

        let board = activity.board();
        assert_eq!(board["rows"].as_array().unwrap().len(), 1);
        assert_eq!(board["rows"][0]["input_tokens"], 20);
        assert_eq!(board["rows"][0]["new_input_tokens"], 16);
        assert_eq!(board["facets"]["providers"][0]["id"], "trae");
        assert_eq!(board["facets"]["models"][0]["model"], "gpt-test");
        assert_eq!(board["facets"]["accounts"][0]["id"], "account-1");

        drop(activity);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn current_day_usage_counts_reported_zero_tokens_as_metered() {
        let dir = temp_dir();
        let activity = Activity::new(&dir);
        let record = activity.begin("copilot", "gpt-test", "chat", 8791, None);
        activity.finish(record, 200, None, false);

        let summary = activity.view("copilot")["summary"].clone();
        assert_eq!(summary["requests"], 1);
        assert_eq!(summary["metered"], 0);

        let mut zero_usage = activity.begin("copilot", "gpt-test", "chat", 8791, None);
        zero_usage.account = "account-1".to_string();
        zero_usage.input_tokens = Some(0);
        zero_usage.output_tokens = Some(0);
        activity.finish(zero_usage, 200, None, false);
        let summary = activity.view("copilot")["summary"].clone();
        assert_eq!(summary["metered"], 1);
        let account_usage = activity.account_usage("copilot");
        assert_eq!(account_usage["account-1"].requests, 1);
        assert_eq!(account_usage["account-1"].metered, 1);
        assert_eq!(account_usage["account-1"].tokens, 0);

        drop(activity);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn multibyte_session_ids_are_truncated_without_panicking() {
        let dir = temp_dir();
        let activity = Activity::new(&dir);

        // 20 CJK characters: a 16-byte slice lands mid-character, which aborted the
        // whole process under `panic = "abort"`.
        let session = "会话标识会话标识会话标识会话标识会话标识";
        let expected: String = session.chars().take(16).collect();
        let record = activity.begin("trae", "model-a", "chat", 8791, Some(session));
        assert_eq!(record.session_id, format!("ses_{}", expected));

        let ascii = activity.begin("trae", "model-a", "chat", 8791, Some("0123456789abcdefghij"));
        assert_eq!(ascii.session_id, "ses_0123456789abcdef");

        let short = activity.begin("trae", "model-a", "chat", 8791, Some("会话"));
        assert_eq!(short.session_id, "ses_会话");

        drop(activity);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn board_is_cached_until_the_revision_changes() {
        let dir = temp_dir();
        let activity = Activity::new(&dir);

        let first = activity.board();
        assert_eq!(first["rows"].as_array().unwrap().len(), 0);
        assert_eq!(first["revision"].as_u64().unwrap(), 0);
        assert_eq!(activity.board(), first);

        let record = activity.begin("trae", "model-a", "chat", 8791, None);
        activity.finish(record, 200, None, false);

        let second = activity.board();
        assert_eq!(second["rows"].as_array().unwrap().len(), 1);
        assert!(second["revision"].as_u64().unwrap() > first["revision"].as_u64().unwrap());

        drop(activity);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn oversized_request_log_is_trimmed_on_startup() {
        use std::io::Write;

        let dir = temp_dir();
        let path = dir.join("requests.jsonl");
        let template = r#"{"id":"x","provider":"trae","model":"m","protocol":"chat","port":8791,"timestamp":0.0,"status":"success","http_status":200,"duration_ms":1,"input_tokens":null,"output_tokens":null,"cached_tokens":null,"reasoning_tokens":null,"ttft_ms":null,"account":"","error":"","session_id":"s","session_source":"request"}"#;
        {
            let mut file = std::fs::File::create(&path).unwrap();
            for _ in 0..(MAX_LOG_LINES + 500) {
                writeln!(file, "{}", template).unwrap();
            }
        }

        let activity = Activity::new(&dir);

        let remaining = std::fs::read_to_string(&path).unwrap();
        assert_eq!(remaining.lines().count(), MAX_LOG_LINES);
        // The in-memory window stays bounded independently of the on-disk log.
        assert_eq!(
            activity.board()["rows"].as_array().unwrap().len(),
            MEMORY_ROWS
        );
        assert!(std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .all(|entry| !entry.file_name().to_string_lossy().ends_with(".trim.tmp")));

        drop(activity);
        let _ = std::fs::remove_dir_all(dir);
    }
}
