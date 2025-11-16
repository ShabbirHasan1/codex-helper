use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::config::proxy_home_dir;
use crate::usage::UsageMetrics;

#[derive(Debug, Serialize)]
pub struct RequestLog<'a> {
    pub timestamp_ms: u64,
    pub service: &'a str,
    pub method: &'a str,
    pub path: &'a str,
    pub status_code: u16,
    pub duration_ms: u64,
    pub config_name: &'a str,
    pub upstream_base_url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageMetrics>,
}

fn log_path() -> PathBuf {
    proxy_home_dir().join("logs").join("requests.jsonl")
}

pub fn log_request(
    service: &str,
    method: &str,
    path: &str,
    status_code: u16,
    duration_ms: u64,
    config_name: &str,
    upstream_base_url: &str,
    usage: Option<UsageMetrics>,
) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let entry = RequestLog {
        timestamp_ms: ts,
        service,
        method,
        path,
        status_code,
        duration_ms,
        config_name,
        upstream_base_url,
        usage,
    };

    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    if let Ok(line) = serde_json::to_string(&entry) {
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(file, "{}", line);
        }
    }
}

