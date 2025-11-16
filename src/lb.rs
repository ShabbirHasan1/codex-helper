use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rand::distributions::WeightedIndex;
use rand::prelude::*;

use crate::config::{ServiceConfig, UpstreamConfig};

const FAILURE_THRESHOLD: u32 = 3;
const COOLDOWN_SECS: u64 = 30;

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

        let mut map = self
            .states
            .lock()
            .expect("lb state mutex poisoned");
        let entry = map
            .entry(self.service.name.clone())
            .or_insert_with(LbState::default);
        entry.ensure_len(self.service.upstreams.len());

        let now = std::time::Instant::now();

        // First pass: respect both failure/cooldown and usage_exhausted
        let weights: Vec<f64> = self
            .service
            .upstreams
            .iter()
            .enumerate()
            .map(|(idx, u)| {
                if let Some(until) = entry.cooldown_until.get(idx).and_then(|v| *v) {
                    if now >= until {
                        entry.failure_counts[idx] = 0;
                        if let Some(slot) = entry.cooldown_until.get_mut(idx) {
                            *slot = None;
                        }
                    }
                }

                if entry.failure_counts[idx] >= FAILURE_THRESHOLD {
                    0.0
                } else if entry
                    .usage_exhausted
                    .get(idx)
                    .copied()
                    .unwrap_or(false)
                {
                    0.0
                } else if u.weight > 0.0 {
                    u.weight
                } else {
                    1.0
                }
            })
            .collect();

        let total_weight: f64 = weights.iter().sum();
        let idx = if total_weight > 0.0 {
            let dist = WeightedIndex::new(&weights).ok();
            match dist {
                Some(d) => {
                    let mut rng = thread_rng();
                    d.sample(&mut rng)
                }
                None => weights
                    .iter()
                    .enumerate()
                    .find(|(_, w)| **w > 0.0)
                    .map(|(i, _)| i)
                    .unwrap_or(0),
            }
        } else {
            // Fallback: ignore usage_exhausted, only respect failure/cooldown
            let fallback_weights: Vec<f64> = self
                .service
                .upstreams
                .iter()
                .enumerate()
                .map(|(idx, u)| {
                    if entry.failure_counts[idx] >= FAILURE_THRESHOLD {
                        0.0
                    } else if u.weight > 0.0 {
                        u.weight
                    } else {
                        1.0
                    }
                })
                .collect();
            let dist = WeightedIndex::new(&fallback_weights).ok();
            match dist {
                Some(d) => {
                    let mut rng = thread_rng();
                    d.sample(&mut rng)
                }
                None => fallback_weights
                    .iter()
                    .enumerate()
                    .find(|(_, w)| **w > 0.0)
                    .map(|(i, _)| i)
                    .unwrap_or(0),
            }
        };
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
            if entry.failure_counts[index] >= FAILURE_THRESHOLD {
                if let Some(slot) = entry.cooldown_until.get_mut(index) {
                    *slot = Some(std::time::Instant::now() + std::time::Duration::from_secs(COOLDOWN_SECS));
                }
            }
        }
    }
}
