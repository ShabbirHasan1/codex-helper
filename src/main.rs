mod codex_integration;
mod config;
mod filter;
mod logging;
mod lb;
mod usage_providers;
mod usage;
mod proxy;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use clap::{Parser, Subcommand};
use std::collections::HashMap;
use std::sync::Mutex;
use reqwest::Client;
use tracing_subscriber::EnvFilter;

use crate::config::load_or_bootstrap_from_codex;
use crate::proxy::{router as proxy_router, ProxyService};

#[derive(Parser, Debug)]
#[command(name = "codex-proxy")]
#[command(about = "Local proxy for Codex CLI", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start HTTP proxy server (default)
    Serve {
        #[arg(long, default_value_t = 3211)]
        port: u16,
    },
    /// Patch ~/.codex/config.toml to use local proxy
    SwitchOn {
        #[arg(long, default_value_t = 3211)]
        port: u16,
    },
    /// Restore ~/.codex/config.toml from backup
    SwitchOff,
    /// Manage proxy configs for Codex
    Config {
        #[command(subcommand)]
        cmd: ConfigCommand,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// List Codex configs in ~/.codex-proxy/config.json
    List,
    /// Add a new Codex config
    Add {
        name: String,
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        auth_token: Option<String>,
        #[arg(long, default_value_t = 1.0)]
        weight: f64,
        /// Optional alias for this config
        #[arg(long)]
        alias: Option<String>,
    },
    /// Set active Codex config
    SetActive {
        name: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // 默认启用 info 级别日志，若用户设置了 RUST_LOG 则按其配置。
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let cli = Cli::parse();

    match cli.command.unwrap_or(Command::Serve { port: 3211 }) {
        Command::SwitchOn { port } => {
            codex_integration::switch_on(port)?;
            return Ok(());
        }
        Command::SwitchOff => {
            codex_integration::switch_off()?;
            return Ok(());
        }
        Command::Config { cmd } => {
            handle_config_cmd(cmd).await?;
            return Ok(());
        }
        Command::Serve { port } => {
            run_server(port).await?;
        }
    }

    Ok(())
}

async fn run_server(port: u16) -> Result<()> {
    // 自动将 Codex 切换到本地代理
    match codex_integration::switch_on(port) {
        Ok(()) => {
            tracing::info!("Codex config switched to local proxy on port {}", port);
        }
        Err(err) => {
            tracing::warn!("Failed to switch Codex config to local proxy: {}", err);
        }
    }

    let cfg = Arc::new(load_or_bootstrap_from_codex().await?);
    let client = Client::builder().build()?;

    // 统一的 LB 状态（失败计数、冷却、用量状态）
    let lb_states = Arc::new(Mutex::new(HashMap::new()));

    // 目前只实现 codex 服务，未来可根据端口/子命令拆分
    let codex_proxy = ProxyService::new(client, cfg.clone(), "codex", lb_states.clone());
    let app: Router = proxy_router(codex_proxy);

    let addr: SocketAddr = SocketAddr::from(([127, 0, 0, 1], port));
    tracing::info!("codex-proxy listening on http://{}", addr);

    axum::serve(tokio::net::TcpListener::bind(addr).await?, app.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn handle_config_cmd(cmd: ConfigCommand) -> Result<()> {
    use crate::config::{save_config, ServiceConfig, UpstreamAuth, UpstreamConfig};

    let mut cfg = load_or_bootstrap_from_codex().await?;

    match cmd {
        ConfigCommand::List => {
            if cfg.codex.configs.is_empty() {
                println!("No Codex configs in ~/.codex-proxy/config.json");
            } else {
                let active = cfg.codex.active.clone();
                println!("Codex configs:");
                for (name, service) in &cfg.codex.configs {
                    let marker = if Some(name) == active.as_ref() {
                        "*"
                    } else {
                        " "
                    };
                    if let Some(alias) = &service.alias {
                        println!(
                            "  {} {} [{}] ({} upstreams)",
                            marker,
                            name,
                            alias,
                            service.upstreams.len()
                        );
                    } else {
                        println!(
                            "  {} {} ({} upstreams)",
                            marker,
                            name,
                            service.upstreams.len()
                        );
                    }
                }
            }
        }
        ConfigCommand::Add {
            name,
            base_url,
            auth_token,
            weight,
            alias,
        } => {
            let upstream = UpstreamConfig {
                base_url,
                weight,
                auth: UpstreamAuth {
                    auth_token,
                    api_key: None,
                },
                tags: Default::default(),
            };
            let service = ServiceConfig {
                name: name.clone(),
                alias,
                upstreams: vec![upstream],
            };
            cfg.codex.configs.insert(name.clone(), service);
            if cfg.codex.active.is_none() {
                cfg.codex.active = Some(name.clone());
            }
            save_config(&cfg).await?;
            println!("Added Codex config '{}'", name);
        }
        ConfigCommand::SetActive { name } => {
            if !cfg.codex.configs.contains_key(&name) {
                println!("Config '{}' not found", name);
            } else {
                cfg.codex.active = Some(name.clone());
                save_config(&cfg).await?;
                println!("Active Codex config set to '{}'", name);
            }
        }
    }

    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
        let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
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
