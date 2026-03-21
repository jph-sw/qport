use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use reqwest::Client;
use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::channel;
use std::time::Duration;
use tracing::{error, info, warn};

const PORT_FILE: &str = "/gluetun/forwarded_port";
const QB_URL_DEFAULT: &str = "http://localhost:8080";
const QB_USER_DEFAULT: &str = "admin";
const QB_PASS_DEFAULT: &str = "adminadmin";

struct Config_ {
    qb_url: String,
    qb_user: String,
    qb_pass: String,
    port_file: String,
}

impl Config_ {
    fn from_env() -> Self {
        Self {
            qb_url: std::env::var("QB_URL").unwrap_or_else(|_| QB_URL_DEFAULT.to_string()),
            qb_user: std::env::var("QB_USER").unwrap_or_else(|_| QB_USER_DEFAULT.to_string()),
            qb_pass: std::env::var("QB_PASS").unwrap_or_else(|_| QB_PASS_DEFAULT.to_string()),
            port_file: std::env::var("PORT_FILE").unwrap_or_else(|_| PORT_FILE.to_string()),
        }
    }
}

fn read_port(path: &str) -> Option<u16> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| warn!("Failed to read port file '{}': {}", path, e))
        .ok()?;

    let trimmed = content.trim();
    trimmed
        .parse::<u16>()
        .map_err(|e| warn!("Invalid port value '{}': {}", trimmed, e))
        .ok()
}

async fn qb_login(client: &Client, cfg: &Config_) -> anyhow::Result<()> {
    let url = format!("{}/api/v2/auth/login", cfg.qb_url);
    let mut params = HashMap::new();
    params.insert("username", cfg.qb_user.as_str());
    params.insert("password", cfg.qb_pass.as_str());

    let resp = client.post(&url).form(&params).send().await?;
    let body = resp.text().await?;

    if body.trim() == "Ok." {
        info!("Logged in to qBittorrent");
        Ok(())
    } else if body.trim() == "Fails." {
        anyhow::bail!("qBittorrent login failed: bad credentials")
    } else {
        anyhow::bail!("qBittorrent login unexpected response: {}", body)
    }
}

async fn qb_set_port(client: &Client, cfg: &Config_, port: u16) -> anyhow::Result<()> {
    let url = format!("{}/api/v2/app/setPreferences", cfg.qb_url);
    let json = format!("{{\"listen_port\":{}}}", port);
    let mut params = HashMap::new();
    params.insert("json", json.as_str());

    let resp = client.post(&url).form(&params).send().await?;
    let status = resp.status();

    if status.is_success() {
        info!("Successfully set qBittorrent listen port to {}", port);
        Ok(())
    } else {
        anyhow::bail!("Failed to set port, HTTP {}", status)
    }
}

async fn sync_port(client: &Client, cfg: &Config_, port: u16) {
    for attempt in 1..=3 {
        match qb_login(client, cfg).await {
            Ok(_) => match qb_set_port(client, cfg, port).await {
                Ok(_) => return,
                Err(e) => error!("Attempt {}/3 - Failed to set port: {}", attempt, e),
            },
            Err(e) => error!("Attempt {}/3 - Login failed: {}", attempt, e),
        }

        if attempt < 3 {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
    error!("All attempts to sync port {} failed", port);
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cfg = Config_::from_env();

    info!("Starting gluetun → qBittorrent port sync");
    info!("Watching: {}", cfg.port_file);
    info!("qBittorrent: {}", cfg.qb_url);

    let client = Client::builder()
        .cookie_store(true)
        .timeout(Duration::from_secs(10))
        .build()?;

    if let Some(port) = read_port(&cfg.port_file) {
        info!("Initial port: {}", port);
        sync_port(&client, &cfg, port).await;
    } else {
        warn!("Could not read initial port from '{}'", cfg.port_file);
    }

    let (tx, rx) = channel();
    let mut watcher = RecommendedWatcher::new(tx, Config::default())?;

    let watch_path = Path::new(&cfg.port_file);

    let watch_dir = watch_path.parent().unwrap_or(Path::new("/"));
    watcher.watch(watch_dir, RecursiveMode::NonRecursive)?;
    info!("Watching directory: {}", watch_dir.display());

    let mut last_port: Option<u16> = None;

    for res in rx {
        match res {
            Ok(event) => {
                let relevant = event.paths.iter().any(|p| p == watch_path);
                if !relevant {
                    continue;
                }

                match event.kind {
                    EventKind::Create(_) | EventKind::Modify(_) => {
                        tokio::time::sleep(Duration::from_millis(200)).await;

                        if let Some(port) = read_port(&cfg.port_file) {
                            if Some(port) == last_port {
                                info!("Port unchanged ({}), skipping update", port);
                                continue;
                            }
                            info!("Port changed: {:?} → {}", last_port, port);
                            last_port = Some(port);
                            sync_port(&client, &cfg, port).await;
                        }
                    }
                    EventKind::Remove(_) => {
                        warn!("Port file was removed, waiting for it to reappear...");
                        last_port = None;
                    }
                    _ => {}
                }
            }
            Err(e) => error!("Watch error: {}", e),
        }
    }

    Ok(())
}
