use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct UsageMetrics {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub reasoning_tokens: i64,
    #[serde(default)]
    pub total_tokens: i64,
}

impl UsageMetrics {
    pub fn add_assign(&mut self, other: &UsageMetrics) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
    }
}

fn to_i64(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n.as_i64().unwrap_or(0),
        Value::String(s) => s.parse::<f64>().ok().map(|f| f as i64).unwrap_or(0),
        _ => 0,
    }
}

fn extract_usage_obj(payload: &Value) -> Option<&Value> {
    if let Some(u) = payload.get("usage") {
        return Some(u);
    }
    if let Some(resp) = payload.get("response")
        && let Some(u) = resp.get("usage")
    {
        return Some(u);
    }
    None
}

fn usage_from_value(usage_obj: &Value) -> UsageMetrics {
    let mut m = UsageMetrics::default();

    if let Some(v) = usage_obj.get("input_tokens") {
        m.input_tokens = to_i64(v);
    }
    if let Some(v) = usage_obj.get("output_tokens") {
        m.output_tokens = to_i64(v);
    }
    if let Some(v) = usage_obj.get("total_tokens") {
        m.total_tokens = to_i64(v);
    } else {
        m.total_tokens = m.input_tokens + m.output_tokens;
    }
    if let Some(details) = usage_obj
        .get("output_tokens_details")
        .and_then(|v| v.as_object())
        && let Some(v) = details.get("reasoning_tokens")
    {
        m.reasoning_tokens = to_i64(v);
    }
    m
}

pub fn extract_usage_from_bytes(data: &[u8]) -> Option<UsageMetrics> {
    let text = std::str::from_utf8(data).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    let json: Value = serde_json::from_str(text).ok()?;
    let usage_obj = extract_usage_obj(&json)?;
    Some(usage_from_value(usage_obj))
}

pub fn extract_usage_from_sse_bytes(data: &[u8]) -> Option<UsageMetrics> {
    let text = std::str::from_utf8(data).ok()?;
    let mut last: Option<UsageMetrics> = None;

    for chunk in text.split("\n\n") {
        let lines: Vec<&str> = chunk
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect();
        for line in lines {
            if let Some(rest) = line.strip_prefix("data:") {
                let payload_str = rest.trim();
                if payload_str.is_empty() {
                    continue;
                }
                if let Ok(json) = serde_json::from_str::<Value>(payload_str)
                    && let Some(usage_obj) = extract_usage_obj(&json)
                {
                    last = Some(usage_from_value(usage_obj));
                }
            }
        }
    }

    last
}
