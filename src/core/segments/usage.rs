use super::{Segment, SegmentData};
use crate::config::{InputData, SegmentId, TranscriptEntry};
use crate::utils::credentials;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::OnceLock;

type UsageResult = Option<(f64, f64, Option<String>, Option<String>)>;
static USAGE_CACHE: OnceLock<UsageResult> = OnceLock::new();

#[derive(Debug, Deserialize)]
struct ApiUsageResponse {
    five_hour: UsagePeriod,
    seven_day: UsagePeriod,
}

#[derive(Debug, Deserialize)]
struct UsagePeriod {
    utilization: f64,
    resets_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ApiUsageCache {
    five_hour_utilization: f64,
    seven_day_utilization: f64,
    five_hour_resets_at: Option<String>,
    seven_day_resets_at: Option<String>,
    cached_at: String,
    #[serde(default)]
    tokens_at_sync: u32,
}

// Shared utilities for both usage segments
struct UsageUtils;

impl UsageUtils {
    fn log_api_event(message: &str) {
        let log_path = dirs::home_dir()
            .map(|h| h.join(".claude").join("ccline").join("api_usage.log"));
        if let Some(path) = log_path {
            if let Ok(mut file) = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let now = Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
                let _ = writeln!(file, "[{}] {}", now, message);
            }
        }
    }
}

impl UsageUtils {
    #[allow(dead_code)]
    fn get_circle_icon(utilization: f64) -> String {
        let percent = (utilization * 100.0) as u8;
        match percent {
            0..=12 => "\u{f0a9e}".to_string(),
            13..=25 => "\u{f0a9f}".to_string(),
            26..=37 => "\u{f0aa0}".to_string(),
            38..=50 => "\u{f0aa1}".to_string(),
            51..=62 => "\u{f0aa2}".to_string(),
            63..=75 => "\u{f0aa3}".to_string(),
            76..=87 => "\u{f0aa4}".to_string(),
            _ => "\u{f0aa5}".to_string(),
        }
    }

    /// Format remaining time until reset.
    /// `fallback_hours`: when `resets_at` is null, assume this many hours as the window size.
    fn format_remaining_time(reset_time_str: Option<&str>, fallback_hours: Option<u64>) -> String {
        let total_secs = if let Some(time_str) = reset_time_str {
            if let Ok(dt) = DateTime::parse_from_rfc3339(time_str) {
                let remaining = dt.with_timezone(&Utc).signed_duration_since(Utc::now());
                let secs = remaining.num_seconds();
                if secs <= 0 {
                    return "soon".to_string();
                }
                secs
            } else {
                return "?".to_string();
            }
        } else if let Some(hours) = fallback_hours {
            // API returned null resets_at — use window size as approximate remaining
            (hours * 3600) as i64
        } else {
            return "?".to_string();
        };

        let hours = total_secs / 3600;
        let minutes = (total_secs % 3600) / 60;

        if hours > 24 {
            let days = hours / 24;
            let rem_hours = hours % 24;
            format!("{}d{}h", days, rem_hours)
        } else if hours > 0 {
            format!("{}h{}m", hours, minutes)
        } else {
            format!("{}m", minutes)
        }
    }

    fn get_current_transcript_tokens(transcript_path: &str) -> u32 {
        let path = Path::new(transcript_path);
        let file = match fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return 0,
        };
        let reader = BufReader::new(file);
        let lines: Vec<String> = reader
            .lines()
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_default();

        for line in lines.iter().rev() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<TranscriptEntry>(line) {
                if entry.r#type.as_deref() == Some("assistant") {
                    if let Some(message) = &entry.message {
                        if let Some(raw_usage) = &message.usage {
                            let normalized = raw_usage.clone().normalize();
                            return normalized.display_tokens();
                        }
                    }
                }
            }
        }
        0
    }

    fn estimate_usage_increase(token_delta: u32) -> (f64, f64) {
        let delta = token_delta as f64;
        let five_hour_increase = delta / 50_000.0;
        let seven_day_increase = delta / 350_000.0;
        (five_hour_increase, seven_day_increase)
    }

    fn get_cache_path() -> Option<std::path::PathBuf> {
        let home = dirs::home_dir()?;
        Some(
            home.join(".claude")
                .join("ccline")
                .join(".api_usage_cache.json"),
        )
    }

    fn load_cache() -> Option<ApiUsageCache> {
        let cache_path = Self::get_cache_path()?;
        if !cache_path.exists() {
            return None;
        }
        let content = std::fs::read_to_string(&cache_path).ok()?;
        serde_json::from_str(&content).ok()
    }

    fn save_cache(cache: &ApiUsageCache) {
        if let Some(cache_path) = Self::get_cache_path() {
            if let Some(parent) = cache_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(json) = serde_json::to_string_pretty(cache) {
                let _ = std::fs::write(&cache_path, json);
            }
        }
    }

    fn is_cache_valid(cache: &ApiUsageCache, cache_duration: u64) -> bool {
        let now = Utc::now();

        // If any reset time has passed, cache is invalid — need fresh data
        for reset_str in [&cache.five_hour_resets_at, &cache.seven_day_resets_at].iter() {
            if let Some(ref time_str) = reset_str {
                if let Ok(dt) = DateTime::parse_from_rfc3339(time_str) {
                    if now >= dt.with_timezone(&Utc) {
                        return false;
                    }
                }
            }
        }

        // Otherwise use normal cache duration
        if let Ok(cached_at) = DateTime::parse_from_rfc3339(&cache.cached_at) {
            let elapsed = now.signed_duration_since(cached_at.with_timezone(&Utc));
            elapsed.num_seconds() < cache_duration as i64
        } else {
            false
        }
    }

    fn get_claude_code_version() -> String {
        use std::process::Command;
        let output = Command::new("npm")
            .args(["view", "@anthropic-ai/claude-code", "version"])
            .output();
        match output {
            Ok(output) if output.status.success() => {
                let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !version.is_empty() {
                    return format!("claude-code/{}", version);
                }
            }
            _ => {}
        }
        "claude-code".to_string()
    }

    fn get_proxy_from_settings() -> Option<String> {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .ok()?;
        let settings_path = format!("{}/.claude/settings.json", home);
        let content = std::fs::read_to_string(&settings_path).ok()?;
        let settings: serde_json::Value = serde_json::from_str(&content).ok()?;
        settings
            .get("env")?
            .get("HTTPS_PROXY")
            .or_else(|| settings.get("env")?.get("HTTP_PROXY"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    fn fetch_api_usage(api_base_url: &str, token: &str, timeout_secs: u64) -> Option<ApiUsageResponse> {
        let url = format!("{}/api/oauth/usage", api_base_url);
        let user_agent = Self::get_claude_code_version();

        let agent = if let Some(proxy_url) = Self::get_proxy_from_settings() {
            if let Ok(proxy) = ureq::Proxy::new(&proxy_url) {
                ureq::Agent::config_builder()
                    .proxy(Some(proxy))
                    .build()
                    .new_agent()
            } else {
                ureq::Agent::new_with_defaults()
            }
        } else {
            ureq::Agent::new_with_defaults()
        };

        Self::log_api_event(&format!("API_REQUEST url={} timeout={}s", url, timeout_secs));

        let result = agent
            .get(&url)
            .header("Authorization", &format!("Bearer {}", token))
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("User-Agent", &user_agent)
            .config()
            .timeout_global(Some(std::time::Duration::from_secs(timeout_secs)))
            .build()
            .call();

        match result {
            Ok(response) => {
                match response.into_body().read_json::<ApiUsageResponse>() {
                    Ok(data) => {
                        Self::log_api_event(&format!(
                            "API_SUCCESS 5h={:.1}% 7d={:.1}%",
                            data.five_hour.utilization, data.seven_day.utilization
                        ));
                        Some(data)
                    }
                    Err(e) => {
                        Self::log_api_event(&format!("API_PARSE_ERROR {}", e));
                        None
                    }
                }
            }
            Err(e) => {
                Self::log_api_event(&format!("API_FETCH_ERROR {}", e));
                None
            }
        }
    }

    /// Get usage data, shared across 5h and 7d segments (called once per process)
    fn get_usage_data_shared(input: &InputData) -> UsageResult {
        USAGE_CACHE.get_or_init(|| Self::get_usage_data_inner(input)).clone()
    }

    /// Fetch or use cached usage data, returns (5h_util, 7d_util, 5h_resets, 7d_resets)
    fn get_usage_data_inner(input: &InputData) -> Option<(f64, f64, Option<String>, Option<String>)> {
        let token = credentials::get_oauth_token()?;

        let config = crate::config::Config::load().ok()?;
        // Look for usage or usage_7d segment config for settings
        let segment_config = config.segments.iter()
            .find(|s| s.id == SegmentId::Usage || s.id == SegmentId::Usage7d);

        let api_base_url = segment_config
            .and_then(|sc| sc.options.get("api_base_url"))
            .and_then(|v| v.as_str())
            .unwrap_or("https://api.anthropic.com");

        let cache_duration = segment_config
            .and_then(|sc| sc.options.get("cache_duration"))
            .and_then(|v| v.as_u64())
            .unwrap_or(300);

        let timeout = segment_config
            .and_then(|sc| sc.options.get("timeout"))
            .and_then(|v| v.as_u64())
            .unwrap_or(5);

        let current_tokens = Self::get_current_transcript_tokens(&input.transcript_path);

        let cached_data = Self::load_cache();
        let use_cached = cached_data
            .as_ref()
            .map(|cache| Self::is_cache_valid(cache, cache_duration))
            .unwrap_or(false);

        if use_cached {
            let cache = cached_data.unwrap();
            let token_delta = current_tokens.saturating_sub(cache.tokens_at_sync);
            let (five_inc, seven_inc) = Self::estimate_usage_increase(token_delta);
            Self::log_api_event(&format!(
                "CACHE_HIT cached_5h={:.1}% est_inc={:.1}% tokens={} delta={}",
                cache.five_hour_utilization, five_inc, current_tokens, token_delta
            ));
            Some((
                (cache.five_hour_utilization + five_inc).min(100.0),
                (cache.seven_day_utilization + seven_inc).min(100.0),
                cache.five_hour_resets_at,
                cache.seven_day_resets_at,
            ))
        } else {
            Self::log_api_event(&format!(
                "CACHE_MISS has_stale={} cache_duration={}s",
                cached_data.is_some(), cache_duration
            ));
            match Self::fetch_api_usage(api_base_url, &token, timeout) {
                Some(response) => {
                    let cache = ApiUsageCache {
                        five_hour_utilization: response.five_hour.utilization,
                        seven_day_utilization: response.seven_day.utilization,
                        five_hour_resets_at: response.five_hour.resets_at.clone(),
                        seven_day_resets_at: response.seven_day.resets_at.clone(),
                        cached_at: Utc::now().to_rfc3339(),
                        tokens_at_sync: current_tokens,
                    };
                    Self::save_cache(&cache);
                    Some((
                        response.five_hour.utilization,
                        response.seven_day.utilization,
                        response.five_hour.resets_at,
                        response.seven_day.resets_at,
                    ))
                }
                None => {
                    Self::log_api_event("FALLBACK using stale cache with local estimation");
                    cached_data.map(|cache| {
                        let token_delta = current_tokens.saturating_sub(cache.tokens_at_sync);
                        let (five_inc, seven_inc) = Self::estimate_usage_increase(token_delta);
                        // Update cached_at so we don't hammer the API on every call
                        let refreshed = ApiUsageCache {
                            cached_at: Utc::now().to_rfc3339(),
                            ..cache
                        };
                        Self::save_cache(&refreshed);
                        (
                            (refreshed.five_hour_utilization + five_inc).min(100.0),
                            (refreshed.seven_day_utilization + seven_inc).min(100.0),
                            refreshed.five_hour_resets_at,
                            refreshed.seven_day_resets_at,
                        )
                    })
                }
            }
        }
    }
}

// ============ 5h Usage Segment ============

#[derive(Default)]
pub struct UsageSegment;

impl UsageSegment {
    pub fn new() -> Self {
        Self
    }
}

impl Segment for UsageSegment {
    fn collect(&self, input: &InputData) -> Option<SegmentData> {
        let (five_hour_util, _seven_day_util, five_hour_resets_at, _) =
            UsageUtils::get_usage_data_shared(input)?;

        let percent = five_hour_util.round() as u8;
        let remaining = UsageUtils::format_remaining_time(five_hour_resets_at.as_deref(), Some(5));

        let primary = format!("{}%", percent);
        let secondary = format!("· {}", remaining);

        let metadata = HashMap::new();

        Some(SegmentData {
            primary,
            secondary,
            metadata,
        })
    }

    fn id(&self) -> SegmentId {
        SegmentId::Usage
    }
}

// ============ 7d Usage Segment ============

#[derive(Default)]
pub struct Usage7dSegment;

impl Usage7dSegment {
    pub fn new() -> Self {
        Self
    }
}

impl Segment for Usage7dSegment {
    fn collect(&self, input: &InputData) -> Option<SegmentData> {
        let (_five_hour_util, seven_day_util, _, seven_day_resets_at) =
            UsageUtils::get_usage_data_shared(input)?;

        let percent = seven_day_util.round() as u8;
        let remaining = UsageUtils::format_remaining_time(seven_day_resets_at.as_deref(), Some(168));

        let primary = format!("{}%", percent);
        let secondary = format!("· {}", remaining);

        let metadata = HashMap::new();

        Some(SegmentData {
            primary,
            secondary,
            metadata,
        })
    }

    fn id(&self) -> SegmentId {
        SegmentId::Usage7d
    }
}
