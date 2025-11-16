use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use dirs::home_dir;
use toml::Value;

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

fn codex_config_backup_path() -> PathBuf {
    codex_home().join("config.toml.codex-proxy-backup")
}

fn read_config_text(path: &PathBuf) -> Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    let mut file = fs::File::open(path).with_context(|| format!("open {:?}", path))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)
        .with_context(|| format!("read {:?}", path))?;
    Ok(buf)
}

fn atomic_write(path: &PathBuf, data: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all {:?}", parent))?;
    }
    let tmp = path.with_extension("tmp.codex-proxy");
    {
        let mut f = fs::File::create(&tmp).with_context(|| format!("create {:?}", tmp))?;
        f.write_all(data.as_bytes())
            .with_context(|| format!("write {:?}", tmp))?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, path).with_context(|| format!("rename {:?} -> {:?}", tmp, path))?;
    Ok(())
}

/// Switch Codex to use the local codex-proxy model provider.
pub fn switch_on(port: u16) -> Result<()> {
    let cfg_path = codex_config_path();
    let backup_path = codex_config_backup_path();

    // Backup once if original exists and no backup yet.
    if cfg_path.exists() && !backup_path.exists() {
        fs::copy(&cfg_path, &backup_path)
            .with_context(|| format!("backup {:?} -> {:?}", cfg_path, backup_path))?;
    }

    let text = read_config_text(&cfg_path)?;
    let mut table: toml::Table = if text.trim().is_empty() {
        toml::Table::new()
    } else {
        text.parse::<Value>()?
            .as_table()
            .cloned()
            .ok_or_else(|| anyhow!("config.toml root must be table"))?
    };

    // Ensure [model_providers] table exists.
    let providers = table
        .entry("model_providers")
        .or_insert_with(|| Value::Table(toml::Table::new()));

    let providers_table = providers
        .as_table_mut()
        .ok_or_else(|| anyhow!("model_providers must be a table"))?;

    let base_url = format!("http://127.0.0.1:{}", port);
    let mut proxy_table = toml::Table::new();
    proxy_table.insert("name".into(), Value::String("codex-proxy".into()));
    proxy_table.insert("base_url".into(), Value::String(base_url));
    proxy_table.insert("wire_api".into(), Value::String("responses".into()));

    providers_table.insert("codex_proxy".into(), Value::Table(proxy_table));
    table.insert("model_provider".into(), Value::String("codex_proxy".into()));

    let new_text = toml::to_string_pretty(&table)?;
    atomic_write(&cfg_path, &new_text)?;
    Ok(())
}

/// Restore Codex config.toml from backup if present.
pub fn switch_off() -> Result<()> {
    let cfg_path = codex_config_path();
    let backup_path = codex_config_backup_path();
    if backup_path.exists() {
        fs::copy(&backup_path, &cfg_path)
            .with_context(|| format!("restore {:?} -> {:?}", backup_path, cfg_path))?;
    }
    Ok(())
}

