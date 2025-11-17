mod codex_integration;
mod config;
mod filter;
mod lb;
mod logging;
mod proxy;
mod sessions;
mod usage;
mod usage_providers;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use clap::{Parser, Subcommand};
use reqwest::Client;
use std::collections::HashMap;
use std::sync::Mutex;
use tracing_subscriber::EnvFilter;

use crate::config::{load_config, load_or_bootstrap_from_claude, load_or_bootstrap_from_codex};
use crate::proxy::{ProxyService, router as proxy_router};

#[derive(Parser, Debug)]
#[command(name = "codex-helper")]
#[command(about = "Helper proxy for Codex CLI", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start HTTP proxy server (default Codex; use --claude for Claude)
    Serve {
        /// Listen port; defaults to 3211 for Codex, 3210 for Claude
        #[arg(long)]
        port: Option<u16>,
        /// Use Codex service (default if neither flag is set)
        #[arg(long)]
        codex: bool,
        /// Use Claude service (experimental; config must be provided manually)
        #[arg(long)]
        claude: bool,
    },
    /// Patch ~/.codex/config.toml to use local proxy
    SwitchOn {
        #[arg(long, default_value_t = 3211)]
        port: u16,
        /// Target Codex config (default)
        #[arg(long)]
        codex: bool,
        /// Target Claude settings (experimental)
        #[arg(long)]
        claude: bool,
    },
    /// Restore ~/.codex/config.toml from backup
    SwitchOff {
        /// Target Codex config (default)
        #[arg(long)]
        codex: bool,
        /// Target Claude settings (experimental)
        #[arg(long)]
        claude: bool,
    },
    /// Manage proxy configs for Codex / Claude
    Config {
        #[command(subcommand)]
        cmd: ConfigCommand,
    },
    /// Session-related helper commands (Codex sessions)
    Session {
        #[command(subcommand)]
        cmd: SessionCommand,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// List configs in ~/.codex-proxy/config.json
    List {
        /// Target Codex configs (default if neither flag is set)
        #[arg(long)]
        codex: bool,
        /// Target Claude configs
        #[arg(long)]
        claude: bool,
    },
    /// Add a new config
    Add {
        name: String,
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        auth_token: Option<String>,
        /// Optional alias for this config
        #[arg(long)]
        alias: Option<String>,
        /// Target Codex configs (default if neither flag is set)
        #[arg(long)]
        codex: bool,
        /// Target Claude configs
        #[arg(long)]
        claude: bool,
    },
    /// Set active config
    SetActive {
        name: String,
        /// Target Codex configs (default if neither flag is set)
        #[arg(long)]
        codex: bool,
        /// Target Claude configs
        #[arg(long)]
        claude: bool,
    },
}

#[derive(Subcommand, Debug)]
enum SessionCommand {
    /// List recent Codex sessions for the current project
    List {
        /// Maximum number of sessions to show
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Optional directory to search sessions for; defaults to current dir
        #[arg(long)]
        path: Option<String>,
    },
    /// Show the last Codex session for the current project
    Last {
        /// Optional directory to search sessions for; defaults to current dir
        #[arg(long)]
        path: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // 默认启用 info 级别日志，若用户设置了 RUST_LOG 则按其配置。
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let cli = Cli::parse();

    match cli.command.unwrap_or(Command::Serve {
        port: None,
        codex: false,
        claude: false,
    }) {
        Command::SwitchOn {
            port,
            codex,
            claude,
        } => {
            if codex && claude {
                anyhow::bail!("Please specify at most one of --codex / --claude");
            }
            if claude {
                if let Err(err) =
                    codex_integration::guard_claude_settings_before_switch_on_interactive()
                {
                    tracing::warn!("Failed to guard Claude settings before switch-on: {}", err);
                }
                codex_integration::claude_switch_on(port)?;
            } else {
                codex_integration::guard_codex_config_before_switch_on_interactive()?;
                codex_integration::switch_on(port)?;
            }
            return Ok(());
        }
        Command::SwitchOff { codex, claude } => {
            if codex && claude {
                anyhow::bail!("Please specify at most one of --codex / --claude");
            }
            if claude {
                codex_integration::claude_switch_off()?;
            } else {
                codex_integration::switch_off()?;
            }
            return Ok(());
        }
        Command::Config { cmd } => {
            handle_config_cmd(cmd).await?;
            return Ok(());
        }
        Command::Session { cmd } => {
            handle_session_cmd(cmd).await?;
            return Ok(());
        }
        Command::Serve {
            port,
            codex: _,
            claude,
        } => {
            // 默认使用 Codex；如显式指定 --claude 则切换到 Claude。
            let service_name = if claude { "claude" } else { "codex" };
            let port = port.unwrap_or_else(|| if service_name == "codex" { 3211 } else { 3210 });
            run_server(service_name, port).await?;
        }
    }

    Ok(())
}

async fn run_server(service_name: &'static str, port: u16) -> Result<()> {
    // Codex 模式下，自动将 Codex 切换到本地代理；Claude 模式也会尝试修改 settings.json（实验性）。
    if service_name == "codex" {
        // 在切换前做一次守护性检查：如发现 Codex 已指向本地代理且存在备份，则在交互模式下询问是否先恢复。
        if let Err(err) = codex_integration::guard_codex_config_before_switch_on_interactive() {
            tracing::warn!("Failed to guard Codex config before switch-on: {}", err);
        }
        match codex_integration::switch_on(port) {
            Ok(()) => {
                tracing::info!("Codex config switched to local proxy on port {}", port);
            }
            Err(err) => {
                tracing::warn!("Failed to switch Codex config to local proxy: {}", err);
            }
        }
    } else if service_name == "claude" {
        if let Err(err) = codex_integration::guard_claude_settings_before_switch_on_interactive() {
            tracing::warn!("Failed to guard Claude settings before switch-on: {}", err);
        }
        match codex_integration::claude_switch_on(port) {
            Ok(()) => {
                tracing::info!(
                    "Claude settings updated to use local proxy on port {}",
                    port
                );
            }
            Err(err) => {
                tracing::warn!("Failed to update Claude settings for local proxy: {}", err);
            }
        }
    }

    let cfg = if service_name == "codex" {
        // Codex：如有必要从 ~/.codex 进行自动引导。
        Arc::new(load_or_bootstrap_from_codex().await?)
    } else {
        // Claude：如有必要从 ~/.claude 进行自动引导。
        Arc::new(load_or_bootstrap_from_claude().await?)
    };

    // 严格要求存在至少一个有效的上游配置，否则直接报错退出，
    // 避免在运行时才发现无可用上游。
    if service_name == "codex" {
        if cfg.codex.configs.is_empty() || cfg.codex.active_config().is_none() {
            anyhow::bail!(
                "未找到任何可用的 Codex 上游配置，请先确保 ~/.codex/config.toml 与 ~/.codex/auth.json 配置完整，或手动编辑 ~/.codex-proxy/config.json 添加配置"
            );
        }
    } else if service_name == "claude" {
        if cfg.claude.configs.is_empty() || cfg.claude.active_config().is_none() {
            anyhow::bail!(
                "未找到任何可用的 Claude 上游配置，请先确保 ~/.claude/settings.json 配置完整，\
或在 ~/.codex-proxy/config.json 的 `claude` 段下手动添加上游配置"
            );
        }
    }
    let client = Client::builder().build()?;

    // 统一的 LB 状态（失败计数、冷却、用量状态）
    let lb_states = Arc::new(Mutex::new(HashMap::new()));

    // 根据 service_name 选择对应服务配置。
    let proxy = ProxyService::new(client, cfg.clone(), service_name, lb_states.clone());
    let app: Router = proxy_router(proxy);

    let addr: SocketAddr = SocketAddr::from(([127, 0, 0, 1], port));
    tracing::info!(
        "codex-helper listening on http://{} (service: {})",
        addr,
        service_name
    );

    axum::serve(
        tokio::net::TcpListener::bind(addr).await?,
        app.into_make_service(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    Ok(())
}

async fn handle_config_cmd(cmd: ConfigCommand) -> Result<()> {
    use crate::config::{ServiceConfig, UpstreamAuth, UpstreamConfig, save_config};

    fn resolve_service(codex: bool, claude: bool) -> Result<&'static str> {
        if codex && claude {
            anyhow::bail!("Please specify at most one of --codex / --claude");
        }
        if claude { Ok("claude") } else { Ok("codex") }
    }

    match cmd {
        ConfigCommand::List { codex, claude } => {
            let service = resolve_service(codex, claude)?;
            let cfg = load_config().await?;
            let (mgr, label) = if service == "claude" {
                (&cfg.claude, "Claude")
            } else {
                (&cfg.codex, "Codex")
            };

            if mgr.configs.is_empty() {
                println!("No {} configs in ~/.codex-proxy/config.json", label);
            } else {
                let active = mgr.active.clone();
                println!("{} configs:", label);
                for (name, service_cfg) in &mgr.configs {
                    let marker = if Some(name) == active.as_ref() {
                        "*"
                    } else {
                        " "
                    };
                    if let Some(alias) = &service_cfg.alias {
                        println!(
                            "  {} {} [{}] ({} upstreams)",
                            marker,
                            name,
                            alias,
                            service_cfg.upstreams.len()
                        );
                    } else {
                        println!(
                            "  {} {} ({} upstreams)",
                            marker,
                            name,
                            service_cfg.upstreams.len()
                        );
                    }
                }
            }
        }
        ConfigCommand::Add {
            name,
            base_url,
            auth_token,
            alias,
            codex,
            claude,
        } => {
            let service = resolve_service(codex, claude)?;
            let mut cfg = load_config().await?;

            let upstream = UpstreamConfig {
                base_url,
                auth: UpstreamAuth {
                    auth_token,
                    api_key: None,
                },
                tags: Default::default(),
            };
            let service_cfg = ServiceConfig {
                name: name.clone(),
                alias,
                upstreams: vec![upstream],
            };

            if service == "claude" {
                cfg.claude.configs.insert(name.clone(), service_cfg);
                if cfg.claude.active.is_none() {
                    cfg.claude.active = Some(name.clone());
                }
                save_config(&cfg).await?;
                println!("Added Claude config '{}'", name);
            } else {
                cfg.codex.configs.insert(name.clone(), service_cfg);
                if cfg.codex.active.is_none() {
                    cfg.codex.active = Some(name.clone());
                }
                save_config(&cfg).await?;
                println!("Added Codex config '{}'", name);
            }
        }
        ConfigCommand::SetActive {
            name,
            codex,
            claude,
        } => {
            let service = resolve_service(codex, claude)?;
            let mut cfg = load_config().await?;

            if service == "claude" {
                if !cfg.claude.configs.contains_key(&name) {
                    println!("Claude config '{}' not found", name);
                } else {
                    cfg.claude.active = Some(name.clone());
                    save_config(&cfg).await?;
                    println!("Active Claude config set to '{}'", name);
                }
            } else {
                if !cfg.codex.configs.contains_key(&name) {
                    println!("Codex config '{}' not found", name);
                } else {
                    cfg.codex.active = Some(name.clone());
                    save_config(&cfg).await?;
                    println!("Active Codex config set to '{}'", name);
                }
            }
        }
    }

    Ok(())
}

async fn handle_session_cmd(cmd: SessionCommand) -> Result<()> {
    use crate::sessions::{
        SessionSummary, find_codex_sessions_for_current_dir, find_codex_sessions_for_dir,
    };

    match cmd {
        SessionCommand::List { limit, path } => {
            let sessions: Vec<SessionSummary> = if let Some(p) = path {
                let root = std::path::PathBuf::from(p);
                find_codex_sessions_for_dir(&root, limit).await?
            } else {
                find_codex_sessions_for_current_dir(limit).await?
            };
            if sessions.is_empty() {
                println!("No Codex sessions found under ~/.codex/sessions");
            } else {
                println!("Recent Codex sessions (newest first):");
                for s in sessions {
                    let updated = s.updated_at.as_deref().unwrap_or("-");
                    let cwd = s.cwd.as_deref().unwrap_or("-");
                    let preview_raw = s
                        .first_user_message
                        .as_deref()
                        .unwrap_or("")
                        .replace('\n', " ");
                    let preview = truncate_for_display(&preview_raw, 80);

                    // 第一行仅显示 id，方便复制
                    println!("- id: {}", s.id);
                    // 第二行展示更新时间和 cwd
                    println!("  updated: {} | cwd: {}", updated, cwd);
                    // 如有首条用户消息，第三行展示简短预览
                    if !preview.is_empty() {
                        println!("  prompt: {}", preview);
                    }
                    println!();
                }
            }
        }
        SessionCommand::Last { path } => {
            let mut sessions = if let Some(p) = path {
                let root = std::path::PathBuf::from(p);
                find_codex_sessions_for_dir(&root, 1).await?
            } else {
                find_codex_sessions_for_current_dir(1).await?
            };
            if let Some(s) = sessions.pop() {
                println!("Last Codex session for current project:");
                println!("  id: {}", s.id);
                println!("  updated_at: {}", s.updated_at.as_deref().unwrap_or("-"));
                println!("  cwd: {}", s.cwd.as_deref().unwrap_or("-"));
                if let Some(msg) = s.first_user_message.as_deref() {
                    let msg_single = msg.replace('\n', " ");
                    // 对 last 命令，完整展示首条用户消息（仅去除换行），方便复制和回顾上下文。
                    println!("  first_prompt: {}", msg_single);
                }
                println!();
                println!("Resume with:");
                println!("  codex resume {}", s.id);
            } else {
                println!("No Codex sessions found under ~/.codex/sessions");
            }
        }
    }

    Ok(())
}

fn truncate_for_display(s: &str, max_chars: usize) -> String {
    let mut result = String::new();
    let mut count = 0usize;
    for ch in s.chars() {
        if count >= max_chars {
            break;
        }
        result.push(ch);
        count += 1;
    }
    if count < s.chars().count() {
        result.push_str("...");
    }
    result
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = sigint.recv() => {},
            _ = sigterm.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }

    // 优雅退出时自动恢复 Codex 配置
    match codex_integration::switch_off() {
        Ok(()) => {
            tracing::info!("Codex config restored from backup");
        }
        Err(err) => {
            tracing::warn!("Failed to restore Codex config from backup: {}", err);
        }
    }
}
