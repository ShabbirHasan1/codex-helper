use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use regex::bytes::Regex;
use serde::Deserialize;

use crate::config::proxy_home_dir;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilterOp {
    Replace,
    Remove,
}

#[derive(Debug, Deserialize)]
pub struct FilterRuleConfig {
    pub op: FilterOp,
    pub source: String,
    #[serde(default)]
    pub target: String,
}

#[derive(Debug)]
struct CompiledRule {
    op: FilterOp,
    source_bytes: Vec<u8>,
    target_bytes: Vec<u8>,
    regex: Option<Regex>,
}

#[derive(Debug, Default)]
struct Inner {
    last_check: Option<SystemTime>,
    last_mtime: Option<SystemTime>,
    rules: Vec<CompiledRule>,
}

/// 请求过滤器，仿照 cli_proxy 的 filter.json，实现敏感字符串替换/删除。
#[derive(Clone)]
pub struct RequestFilter {
    path: PathBuf,
    check_interval: Duration,
    inner: Arc<Mutex<Inner>>,
}

impl RequestFilter {
    pub fn new() -> Self {
        let path = proxy_home_dir().join("filter.json");
        Self {
            path,
            check_interval: Duration::from_secs(1),
            inner: Arc::new(Mutex::new(Inner::default())),
        }
    }

    fn reload_if_needed(&self, inner: &mut Inner) {
        let now = SystemTime::now();
        if let Some(last) = inner.last_check
            && now.duration_since(last).unwrap_or_default() < self.check_interval
        {
            return;
        }
        inner.last_check = Some(now);

        let meta = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(_) => {
                inner.rules.clear();
                inner.last_mtime = None;
                return;
            }
        };
        let mtime = meta.modified().ok();
        if mtime == inner.last_mtime {
            return;
        }

        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(_) => {
                inner.rules.clear();
                inner.last_mtime = mtime;
                return;
            }
        };

        let configs: Vec<FilterRuleConfig> = if text.trim_start().starts_with('[') {
            match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => {
                    inner.rules.clear();
                    inner.last_mtime = mtime;
                    return;
                }
            }
        } else {
            match serde_json::from_str::<FilterRuleConfig>(&text) {
                Ok(single) => vec![single],
                Err(_) => {
                    inner.rules.clear();
                    inner.last_mtime = mtime;
                    return;
                }
            }
        };

        let mut compiled = Vec::new();
        for c in configs {
            let source_bytes = c.source.as_bytes().to_vec();
            let target_bytes = c.target.as_bytes().to_vec();
            let regex = Regex::new(&c.source).ok();
            compiled.push(CompiledRule {
                op: c.op,
                source_bytes,
                target_bytes,
                regex,
            });
        }

        inner.rules = compiled;
        inner.last_mtime = mtime;
    }

    pub fn apply(&self, data: &[u8]) -> Vec<u8> {
        if data.is_empty() {
            return Vec::new();
        }

        let mut inner = match self.inner.lock() {
            Ok(i) => i,
            Err(_) => return data.to_vec(),
        };

        self.reload_if_needed(&mut inner);
        if inner.rules.is_empty() {
            return data.to_vec();
        }

        let mut buf = data.to_vec();
        for rule in &inner.rules {
            match rule.op {
                FilterOp::Replace => {
                    if let Some(re) = &rule.regex {
                        buf = re
                            .replace_all(&buf, rule.target_bytes.as_slice())
                            .into_owned();
                    } else {
                        buf = buf
                            .split(|b| b == &rule.source_bytes[0])
                            .flat_map(|chunk| chunk.to_vec())
                            .collect();
                    }
                }
                FilterOp::Remove => {
                    if let Some(re) = &rule.regex {
                        buf = re.replace_all(&buf, &[][..]).into_owned();
                    } else if !rule.source_bytes.is_empty() {
                        buf = buf
                            .windows(rule.source_bytes.len())
                            .enumerate()
                            .filter_map(|(i, window)| {
                                if window == rule.source_bytes.as_slice() {
                                    None
                                } else {
                                    Some(buf[i])
                                }
                            })
                            .collect();
                    }
                }
            }
        }
        buf
    }
}
