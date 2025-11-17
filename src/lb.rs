use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::config::{ServiceConfig, UpstreamConfig};

pub const FAILURE_THRESHOLD: u32 = 3;
pub const COOLDOWN_SECS: u64 = 30;

#[derive(Debug, Default)]
pub struct LbState {
    pub failure_counts: Vec<u32>,
    pub cooldown_until: Vec<Option<std::time::Instant>>,
    pub usage_exhausted: Vec<bool>,
}

impl LbState {
    fn ensure_len(&mut self, len: usize) {
        if self.failure_counts.len() != len {
            self.failure_counts = vec![0; len];
            self.cooldown_until = vec![None; len];
            self.usage_exhausted = vec![false; len];
        }
    }
}

/// Upstream selection result
#[derive(Debug, Clone)]
pub struct SelectedUpstream {
    pub config_name: String,
    pub index: usize,
    pub upstream: UpstreamConfig,
}

/// 简单的负载选择器，当前仅按权重随机，未来可扩展为按 usage / 失败次数等切换。
#[derive(Clone)]
pub struct LoadBalancer {
    pub service: Arc<ServiceConfig>,
    pub states: Arc<Mutex<HashMap<String, LbState>>>,
}

impl LoadBalancer {
    pub fn new(service: Arc<ServiceConfig>, states: Arc<Mutex<HashMap<String, LbState>>>) -> Self {
        Self { service, states }
    }

    pub fn select_upstream(&self) -> Option<SelectedUpstream> {
        if self.service.upstreams.is_empty() {
            return None;
        }

        let mut map = self.states.lock().expect("lb state mutex poisoned");
        let entry = map
            .entry(self.service.name.clone())
            .or_default();
        entry.ensure_len(self.service.upstreams.len());

        let now = std::time::Instant::now();

        // 更新冷却状态：如果冷却期已过，重置失败计数和冷却时间。
        for idx in 0..self.service.upstreams.len() {
            if let Some(until) = entry.cooldown_until.get(idx).and_then(|v| *v)
                && now >= until {
                    entry.failure_counts[idx] = 0;
                    if let Some(slot) = entry.cooldown_until.get_mut(idx) {
                        *slot = None;
                    }
                }
        }

        // 第一轮：按顺序选择第一个「未熔断 + 未标记用量用尽」的 upstream。
        if let Some(idx) = self
            .service
            .upstreams
            .iter()
            .enumerate()
            .find_map(|(idx, _)| {
                if entry.failure_counts[idx] >= FAILURE_THRESHOLD {
                    return None;
                }
                if entry.usage_exhausted.get(idx).copied().unwrap_or(false) {
                    return None;
                }
                Some(idx)
            })
        {
            let upstream = self.service.upstreams[idx].clone();
            return Some(SelectedUpstream {
                config_name: self.service.name.clone(),
                index: idx,
                upstream,
            });
        }

        // 第二轮：忽略 usage_exhausted，只看失败阈值，仍然按顺序选第一个。
        if let Some(idx) = self
            .service
            .upstreams
            .iter()
            .enumerate()
            .find_map(|(idx, _)| {
                if entry.failure_counts[idx] >= FAILURE_THRESHOLD {
                    None
                } else {
                    Some(idx)
                }
            })
        {
            let upstream = self.service.upstreams[idx].clone();
            return Some(SelectedUpstream {
                config_name: self.service.name.clone(),
                index: idx,
                upstream,
            });
        }

        // 兜底：所有 upstream 都已达到失败阈值时，仍然返回第一个，以保证永远有兜底。
        let idx = 0;
        let upstream = self.service.upstreams[idx].clone();
        Some(SelectedUpstream {
            config_name: self.service.name.clone(),
            index: idx,
            upstream,
        })
    }

    pub fn record_result(&self, index: usize, success: bool) {
        let mut map = match self.states.lock() {
            Ok(m) => m,
            Err(_) => return,
        };
        let entry = map
            .entry(self.service.name.clone())
            .or_insert_with(LbState::default);
        entry.ensure_len(self.service.upstreams.len());
        if index >= entry.failure_counts.len() {
            return;
        }
        if success {
            entry.failure_counts[index] = 0;
            if let Some(slot) = entry.cooldown_until.get_mut(index) {
                *slot = None;
            }
        } else {
            entry.failure_counts[index] = entry.failure_counts[index].saturating_add(1);
            if entry.failure_counts[index] >= FAILURE_THRESHOLD
                && let Some(slot) = entry.cooldown_until.get_mut(index) {
                    *slot = Some(
                        std::time::Instant::now() + std::time::Duration::from_secs(COOLDOWN_SECS),
                    );
                }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ServiceConfig, UpstreamAuth, UpstreamConfig};

    fn make_service(name: &str, urls: &[&str]) -> ServiceConfig {
        ServiceConfig {
            name: name.to_string(),
            alias: None,
            upstreams: urls
                .iter()
                .map(|u| UpstreamConfig {
                    base_url: u.to_string(),
                    auth: UpstreamAuth {
                        auth_token: Some("sk-test".to_string()),
                        api_key: None,
                    },
                    tags: HashMap::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn lb_prefers_non_exhausted_upstream_when_available() {
        let service = make_service(
            "codex-main",
            &["https://primary.example", "https://backup.example"],
        );
        let states = Arc::new(Mutex::new(HashMap::new()));
        let lb = LoadBalancer::new(Arc::new(service), states.clone());

        // 初次选择应选第一个 upstream（index 0）。
        let first = lb.select_upstream().expect("should select an upstream");
        assert_eq!(first.index, 0);

        // 标记 index 0 为 usage_exhausted，index 1 为可用。
        {
            let mut guard = states.lock().unwrap();
            let entry = guard
                .entry("codex-main".to_string())
                .or_insert_with(LbState::default);
            entry.ensure_len(2);
            entry.usage_exhausted[0] = true;
            entry.usage_exhausted[1] = false;
        }

        // 此时应优先选择未 exhausted 的 index 1。
        let second = lb.select_upstream().expect("should select backup upstream");
        assert_eq!(second.index, 1);
    }

    #[test]
    fn lb_falls_back_when_all_exhausted() {
        let service = make_service(
            "codex-main",
            &["https://primary.example", "https://backup.example"],
        );
        let states = Arc::new(Mutex::new(HashMap::new()));
        let lb = LoadBalancer::new(Arc::new(service), states.clone());

        // 初始化状态
        let _ = lb.select_upstream();

        {
            let mut guard = states.lock().unwrap();
            let entry = guard
                .entry("codex-main".to_string())
                .or_insert_with(LbState::default);
            entry.ensure_len(2);
            entry.usage_exhausted[0] = true;
            entry.usage_exhausted[1] = true;
        }

        // 所有 upstream 都 exhausted 时，仍然应返回 index 0 做兜底。
        let selected = lb
            .select_upstream()
            .expect("should still select an upstream");
        assert_eq!(selected.index, 0);
    }

    #[test]
    fn lb_avoids_upstreams_past_failure_threshold() {
        let service = make_service(
            "codex-main",
            &["https://primary.example", "https://backup.example"],
        );
        let states = Arc::new(Mutex::new(HashMap::new()));
        let lb = LoadBalancer::new(Arc::new(service), states.clone());

        // 对 primary 连续记录 FAILURE_THRESHOLD 次失败。
        for _ in 0..FAILURE_THRESHOLD {
            lb.record_result(0, false);
        }

        // 此时应选择 backup（index 1），因为 index 0 已达到失败阈值。
        let selected = lb
            .select_upstream()
            .expect("should select backup after failures");
        assert_eq!(selected.index, 1);
    }
}
