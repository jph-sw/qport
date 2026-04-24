use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use reqwest::Client;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::Instant;
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
    /// Delay between the 3 rapid attempts inside a single sync burst.
    sync_attempt_delay: Duration,
    /// How long to wait before the first background retry after all 3 attempts fail.
    initial_retry_delay: Duration,
    /// Maximum backoff cap for subsequent retries.
    max_retry_delay: Duration,
}

impl Config_ {
    fn from_env() -> Self {
        Self {
            qb_url: std::env::var("QB_URL").unwrap_or_else(|_| QB_URL_DEFAULT.to_string()),
            qb_user: std::env::var("QB_USER").unwrap_or_else(|_| QB_USER_DEFAULT.to_string()),
            qb_pass: std::env::var("QB_PASS").unwrap_or_else(|_| QB_PASS_DEFAULT.to_string()),
            port_file: std::env::var("PORT_FILE").unwrap_or_else(|_| PORT_FILE.to_string()),
            sync_attempt_delay: Duration::from_secs(5),
            initial_retry_delay: Duration::from_secs(10),
            max_retry_delay: Duration::from_secs(300),
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

/// Makes up to 3 attempts to log in and set the listen port.
/// Returns true if any attempt succeeded.
async fn sync_port(client: &Client, cfg: &Config_, port: u16) -> bool {
    for attempt in 1..=3u32 {
        match qb_login(client, cfg).await {
            Ok(_) => match qb_set_port(client, cfg, port).await {
                Ok(_) => return true,
                Err(e) => error!("Attempt {}/3 - Failed to set port: {}", attempt, e),
            },
            Err(e) => error!("Attempt {}/3 - Login failed: {}", attempt, e),
        }
        if attempt < 3 {
            tokio::time::sleep(cfg.sync_attempt_delay).await;
        }
    }
    error!("All attempts to sync port {} failed", port);
    false
}

/// Core event loop. Drives file-change events and background retries.
///
/// `last_synced_port` is only updated on a confirmed successful sync, so a
/// failed burst never causes the "port unchanged, skipping" short-circuit.
/// When all 3 burst attempts fail, `pending_port` is set and
/// `tokio::select!` fires a single retry after `retry_delay`, doubling on
/// each failure up to `max_retry_delay`.
async fn run_sync_loop(
    cfg: &Config_,
    client: &Client,
    initial_port: Option<u16>,
    mut event_rx: UnboundedReceiver<notify::Result<Event>>,
) {
    let watch_path = PathBuf::from(&cfg.port_file);
    let mut last_synced_port: Option<u16> = None;
    let mut pending_port: Option<u16> = None;
    let mut retry_deadline = Instant::now();
    let mut retry_delay = cfg.initial_retry_delay;

    if let Some(port) = initial_port {
        if sync_port(client, cfg, port).await {
            last_synced_port = Some(port);
        } else {
            pending_port = Some(port);
            retry_deadline = Instant::now() + cfg.initial_retry_delay;
            info!(
                "Initial sync failed, will retry port {} in {:?}",
                port, cfg.initial_retry_delay
            );
        }
    }

    loop {
        tokio::select! {
            biased; // prefer processing new file events over firing retries

            event = event_rx.recv() => {
                match event {
                    None => break,
                    Some(Err(e)) => error!("Watch error: {}", e),
                    Some(Ok(ev)) => {
                        if !ev.paths.iter().any(|p| p == &watch_path) {
                            continue;
                        }
                        match ev.kind {
                            EventKind::Create(_) | EventKind::Modify(_) => {
                                tokio::time::sleep(Duration::from_millis(200)).await;
                                let Some(port) = read_port(&cfg.port_file) else {
                                    continue;
                                };

                                if Some(port) == last_synced_port {
                                    info!("Port unchanged ({}), skipping update", port);
                                    continue;
                                }

                                info!("Port changed: {:?} → {}", last_synced_port, port);
                                // Cancel any pending retry for the old port
                                pending_port = None;
                                retry_delay = cfg.initial_retry_delay;

                                if sync_port(client, cfg, port).await {
                                    last_synced_port = Some(port);
                                } else {
                                    pending_port = Some(port);
                                    retry_deadline = Instant::now() + cfg.initial_retry_delay;
                                    info!(
                                        "Will retry port {} in {:?}",
                                        port, cfg.initial_retry_delay
                                    );
                                }
                            }
                            EventKind::Remove(_) => {
                                warn!("Port file was removed, waiting for it to reappear...");
                                last_synced_port = None;
                                pending_port = None;
                            }
                            _ => {}
                        }
                    }
                }
            }

            _ = tokio::time::sleep_until(retry_deadline), if pending_port.is_some() => {
                let port = pending_port.unwrap();
                info!("Retrying sync of port {}...", port);

                if sync_port(client, cfg, port).await {
                    info!("Port {} successfully synced after retry", port);
                    last_synced_port = Some(port);
                    pending_port = None;
                    retry_delay = cfg.initial_retry_delay;
                } else {
                    retry_delay = (retry_delay * 2).min(cfg.max_retry_delay);
                    retry_deadline = Instant::now() + retry_delay;
                    info!("Retry failed, next attempt in {:?}", retry_delay);
                }
            }
        }
    }
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

    let (async_tx, async_rx) = tokio::sync::mpsc::unbounded_channel();
    let (sync_tx, sync_rx) = channel();

    let mut watcher = RecommendedWatcher::new(sync_tx, Config::default())?;
    let watch_path = Path::new(&cfg.port_file);
    let watch_dir = watch_path.parent().unwrap_or(Path::new("/"));
    watcher.watch(watch_dir, RecursiveMode::NonRecursive)?;
    info!("Watching directory: {}", watch_dir.display());

    // Bridge notify's sync channel into tokio's async channel.
    tokio::task::spawn_blocking(move || {
        for event in sync_rx {
            if async_tx.send(event).is_err() {
                break;
            }
        }
    });

    let initial_port = read_port(&cfg.port_file);
    if initial_port.is_none() {
        warn!("Could not read initial port from '{}'", cfg.port_file);
    } else {
        info!("Initial port: {}", initial_port.unwrap());
    }

    run_sync_loop(&cfg, &client, initial_port, async_rx).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::ModifyKind;
    use tempfile::tempdir;
    use tokio::sync::mpsc::unbounded_channel;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn modify_event(p: &PathBuf) -> notify::Result<Event> {
        Ok(Event::new(EventKind::Modify(ModifyKind::Any)).add_path(p.clone()))
    }

    /// Simulates the bug scenario:
    ///   1. File is written with port 43897
    ///   2. All 3 immediate sync attempts fail (qBittorrent not yet reachable)
    ///   3. Background retry fires after initial_retry_delay
    ///   4. Retry succeeds — verifies qBittorrent ends up with port 43897 set
    #[tokio::test]
    async fn test_retry_succeeds_after_initial_failure() {
        let server = MockServer::start().await;

        // First 3 login calls fail — exhausted by the initial 3-attempt burst
        Mock::given(method("POST"))
            .and(path("/api/v2/auth/login"))
            .respond_with(ResponseTemplate::new(200).set_body_string("Fails."))
            .up_to_n_times(3)
            .mount(&server)
            .await;

        // All subsequent login calls succeed — picked up by the background retry
        Mock::given(method("POST"))
            .and(path("/api/v2/auth/login"))
            .respond_with(ResponseTemplate::new(200).set_body_string("Ok."))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/v2/app/setPreferences"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let dir = tempdir().unwrap();
        let port_file = dir.path().join("forwarded_port");
        std::fs::write(&port_file, "43897").unwrap();

        let cfg = Config_ {
            qb_url: server.uri(),
            qb_user: "admin".to_string(),
            qb_pass: "adminadmin".to_string(),
            port_file: port_file.to_str().unwrap().to_string(),
            sync_attempt_delay: Duration::from_millis(10),
            initial_retry_delay: Duration::from_millis(100),
            max_retry_delay: Duration::from_secs(1),
        };

        let client = Client::builder()
            .cookie_store(true)
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let (tx, rx) = unbounded_channel();
        tx.send(modify_event(&port_file)).unwrap();

        // Expected timeline: 200ms debounce + 2×10ms attempt gaps + 100ms retry ≈ 320ms
        tokio::time::timeout(
            Duration::from_secs(2),
            run_sync_loop(&cfg, &client, None, rx),
        )
        .await
        .ok(); // timeout is expected — the loop runs indefinitely until channel closes

        let requests = server
            .received_requests()
            .await
            .expect("request recording should be enabled");

        let set_port_calls: Vec<_> = requests
            .iter()
            .filter(|r| r.url.path() == "/api/v2/app/setPreferences")
            .collect();

        assert!(
            !set_port_calls.is_empty(),
            "setPreferences was never called — the retry loop did not recover"
        );
        assert!(
            set_port_calls
                .iter()
                .any(|r| String::from_utf8_lossy(&r.body).contains("43897")),
            "setPreferences was not called with port 43897"
        );
    }
}
