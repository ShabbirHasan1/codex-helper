use std::collections::HashMap;
use std::env;
use std::fs as stdfs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dirs::home_dir;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use toml::Value as TomlValue;
use tokio::fs;
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpstreamAuth {
    /// Bearer token, e.g. OpenAI style
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    /// Optional API key header for some providers
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamConfig {
    pub base_url: String,
    #[serde(default)]
    pub weight: f64,
    #[serde(default)]
    pub auth: UpstreamAuth,
    /// Optional free-form metadata, e.g. region / label
    #[serde(default)]
    pub tags: HashMap<String, String>,
}

/// A logical config entry (roughly corresponds to cli_proxy 的一个配置名)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceConfig {
    /// 配置标识（map key），保持稳定
    pub name: String,
    /// 可选别名，便于展示/记忆
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub upstreams: Vec<UpstreamConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServiceConfigManager {
    /// 当前激活配置名
    #[serde(default)]
    pub active: Option<String>,
    /// 配置集合
    #[serde(default)]
    pub configs: HashMap<String, ServiceConfig>,
}

impl ServiceConfigManager {
    pub fn active_config(&self) -> Option<&ServiceConfig> {
        self.active
            .as_ref()
            .and_then(|name| self.configs.get(name))
            .or_else(|| self.configs.values().next())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProxyConfig {
    /// Codex 服务配置
    #[serde(default)]
    pub codex: ServiceConfigManager,
    /// Claude Code 等其他服务配置，后续扩展
    #[serde(default)]
    pub claude: ServiceConfigManager,
}

fn config_dir() -> PathBuf {
    proxy_home_dir()
}

fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

pub async fn load_config() -> Result<ProxyConfig> {
    let path = config_path();
    if !path.exists() {
        return Ok(ProxyConfig::default());
    }
    let bytes = fs::read(path).await?;
    let cfg = serde_json::from_slice::<ProxyConfig>(&bytes)?;
    Ok(cfg)
}

pub async fn save_config(cfg: &ProxyConfig) -> Result<()> {
    let dir = config_dir();
    fs::create_dir_all(&dir).await?;
    let path = config_path();
    let data = serde_json::to_vec_pretty(cfg)?;
    fs::write(path, data).await?;
    Ok(())
}

pub fn codex_list_configs(cfg: &ProxyConfig) -> Vec<String> {
    cfg.codex.configs.keys().cloned().collect()
}

pub fn codex_set_active(cfg: &mut ProxyConfig, name: &str) -> bool {
    if cfg.codex.configs.contains_key(name) {
        cfg.codex.active = Some(name.to_string());
        true
    } else {
        false
    }
}

/// 获取 codex-proxy 的主目录（用于配置、日志等）
pub fn proxy_home_dir() -> PathBuf {
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex-proxy")
}

fn codex_home() -> PathBuf {
    if let Ok(dir) = env::var("CODEX_HOME") {
        return PathBuf::from(dir);
    }
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
}

fn codex_config_path() -> PathBuf {
    codex_home().join("config.toml")
}

fn codex_auth_path() -> PathBuf {
    codex_home().join("auth.json")
}

fn read_file_if_exists(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let s = stdfs::read_to_string(path)
        .with_context(|| format!("failed to read {:?}", path))?;
    Ok(Some(s))
}

fn resolve_auth_token(env_key: &str, auth_json: &Option<JsonValue>) -> Option<String> {
    if let Ok(val) = env::var(env_key) {
        if !val.trim().is_empty() {
            return Some(val);
        }
    }
    if let Some(json) = auth_json {
        if let Some(obj) = json.as_object() {
            if let Some(v) = obj.get(env_key).and_then(|v| v.as_str()) {
                if !v.trim().is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

fn bootstrap_from_codex(cfg: &mut ProxyConfig) -> Result<()> {
    if !cfg.codex.configs.is_empty() {
        return Ok(());
    }

    let cfg_path = codex_config_path();
    let cfg_text_opt = read_file_if_exists(&cfg_path)?;
    let cfg_text = match cfg_text_opt {
        Some(s) if !s.trim().is_empty() => s,
        _ => {
            anyhow::bail!(
                "未找到 ~/.codex/config.toml 或文件为空，无法自动推导 Codex 上游"
            );
        }
    };

    let value: TomlValue = cfg_text.parse()?;
    let table = value
        .as_table()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Codex config root must be table"))?;

    let provider_id = table
        .get("model_provider")
        .and_then(|v| v.as_str())
        .unwrap_or("openai")
        .to_string();

    let providers_table = table
        .get("model_providers")
        .and_then(|v| v.as_table())
        .cloned()
        .unwrap_or_default();

    let provider_table = providers_table.get(&provider_id);

    let base_url = provider_table
        .and_then(|t| t.get("base_url"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            if provider_id == "openai" {
                "https://api.openai.com/v1"
            } else {
                "https://api.openai.com/v1"
            }
        })
        .to_string();

    let env_key = provider_table
        .and_then(|t| t.get("env_key"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let auth_json_path = codex_auth_path();
    let auth_json: Option<JsonValue> = match read_file_if_exists(&auth_json_path)? {
        Some(s) if !s.trim().is_empty() => serde_json::from_str(&s).ok(),
        _ => None,
    };

    // 仅按 Codex 官方语义解析 token：必须有 env_key 才会尝试从环境变量或 auth.json 读取。
    let auth_token = env_key
        .as_deref()
        .and_then(|key| resolve_auth_token(key, &auth_json));

    if auth_token.is_none() {
        if let Some(key) = env_key.as_deref() {
            warn!(
                "未在环境变量或 ~/.codex/auth.json 中找到 `{}`, 将以无 Authorization 头的方式转发请求；上游可能返回 401/403",
                key
            );
        } else {
            warn!(
                "当前 model_provider 未声明 env_key，且无法从 ~/.codex/auth.json 推断唯一的 token，将以无 Authorization 头的方式转发请求；如需鉴权请在 ~/.codex-proxy/config.json 中手动添加 auth_token"
            );
        }
    } else {
        info!("已从 ~/.codex/auth.json 或环境变量解析到 Codex 上游 token");
    }

    let mut tags = HashMap::new();
    tags.insert("source".into(), "codex-config".into());
    tags.insert("provider_id".into(), provider_id.clone());

    let upstream = UpstreamConfig {
        base_url,
        weight: 1.0,
        auth: UpstreamAuth {
            auth_token,
            api_key: None,
        },
        tags,
    };

    let service = ServiceConfig {
        name: provider_id.clone(),
        alias: None,
        upstreams: vec![upstream],
    };

    cfg.codex.configs.insert(provider_id.clone(), service);
    cfg.codex.active = Some(provider_id);

    Ok(())
}

/// 加载代理配置，如有必要从 ~/.codex 自动初始化 codex 配置。
pub async fn load_or_bootstrap_from_codex() -> Result<ProxyConfig> {
    let mut cfg = load_config().await?;
    if cfg.codex.configs.is_empty() {
        match bootstrap_from_codex(&mut cfg) {
            Ok(()) => {
                let _ = save_config(&cfg).await;
                info!("已根据 ~/.codex/config.toml 自动创建默认 Codex 上游配置");
            }
            Err(err) => {
                warn!(
                    "无法从 ~/.codex 引导 Codex 配置: {err}; \
                     如果尚未安装或配置 Codex CLI 可以忽略，否则请检查 ~/.codex/config.toml 和 ~/.codex/auth.json，或使用 `codex-proxy config add` 手动添加上游"
                );
            }
        }
    } else {
        // 已存在配置但没有 active，提示用户检查
        if cfg.codex.active.is_none() && !cfg.codex.configs.is_empty() {
            warn!(
                "检测到 Codex 配置但没有激活项，将使用任意一条配置作为默认；如需指定，请使用 `codex-proxy config set-active <name>`"
            );
        }
    }
    Ok(cfg)
}
