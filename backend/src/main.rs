use mailcrab::{Error, MailMessage, MessageId, Result, mail_server};
use rust_embed::{EmbeddedFile, RustEmbed};
use std::{
    collections::HashMap,
    env,
    net::IpAddr,
    process,
    str::FromStr,
    sync::{Arc, RwLock},
};
use tokio::{signal, sync::broadcast::Receiver, task::JoinSet, time::Duration};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::{prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt};

use crate::{storage::storage, web_server::web_server};

mod storage;
mod web_server;

#[cfg(test)]
mod tests;

/// retrieve the version from Cargo.toml, note that this will yield an error
/// when compiling without cargo
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// application state, holds all messages, a message queue and configuration
pub struct AppState {
    rx: Receiver<MailMessage>,
    storage: RwLock<HashMap<MessageId, MailMessage>>,
    prefix: String,
    index: Option<String>,
    retention_period: Duration,
}

#[derive(RustEmbed)]
#[folder = "../frontend/dist"]
pub struct Asset;

/// get a configuration from the environment or return default value
fn parse_env_var<T: FromStr>(name: &'static str, default: T) -> T {
    env::var(name)
        .unwrap_or_default()
        .parse::<T>()
        .unwrap_or(default)
}

/// preload the HTML for the index, replace dynamic values
fn load_index(path_prefix: &str) -> Result<String> {
    let index: EmbeddedFile = Asset::get("index.html")
        .ok_or_else(|| Error::WebServer("Could not load index.html".to_owned()))?;
    let index = String::from_utf8_lossy(&index.data);
    let path_prefix = if path_prefix == "/" { "" } else { path_prefix };

    // add path prefix to asset includes
    Ok(index
        .replace("href=\"/", &format!("href=\"{path_prefix}/static/"))
        .replace(
            "'/mailcrab-frontend",
            &format!("'{path_prefix}/static/mailcrab-frontend"),
        ))
}

async fn run() -> i32 {
    let smtp_host: IpAddr = parse_env_var("SMTP_HOST", [0, 0, 0, 0].into());
    let http_host: IpAddr = parse_env_var("HTTP_HOST", [127, 0, 0, 1].into());
    let smtp_port: u16 = parse_env_var("SMTP_PORT", 1025);
    let http_port: u16 = parse_env_var("HTTP_PORT", 1080);
    let queue_capacity: usize = parse_env_var("QUEUE_CAPACITY", 32);

    // Enable auth implicitly enable TLS
    let enable_tls_auth: bool = std::env::var("ENABLE_TLS_AUTH").map_or_else(
        |_| false,
        |v| v.to_ascii_lowercase().parse().unwrap_or(false),
    );

    // construct path prefix
    let prefix = std::env::var("MAILCRAB_PREFIX").unwrap_or_default();
    let prefix = format!("/{}", prefix.trim_matches('/'));

    // optional retention period, the default is 0 - which means messages are kept forever
    let retention_period: u64 = parse_env_var("MAILCRAB_RETENTION_PERIOD", 0);

    info!(
        "MailCrab HTTP server starting on {http_host}:{http_port} and SMTP server on {smtp_host}:{smtp_port}"
    );

    // initialize internal broadcast queue
    let (tx, rx) = tokio::sync::broadcast::channel::<MailMessage>(queue_capacity);
    let storage_rx = rx.resubscribe();
    let app_state = Arc::new(AppState {
        rx,
        storage: Default::default(),
        index: load_index(&prefix).ok(),
        prefix,
        retention_period: Duration::from_secs(retention_period),
    });

    // store broadcasted messages in a key/value store
    let state = app_state.clone();

    // join multiple tasks and handle graceful shutdown on signals
    let token = CancellationToken::new();
    let abort_token = CancellationToken::new();
    let mut set = JoinSet::new();

    set.spawn(storage(storage_rx, state, token.clone()));
    set.spawn(mail_server(
        smtp_host,
        smtp_port,
        tx,
        enable_tls_auth,
        token.clone(),
    ));
    set.spawn(web_server(http_host, http_port, app_state, token.clone()));

    tokio::spawn({
        let abort_token = abort_token.clone();
        async move {
            shutdown_signal().await;
            info!("Received shutdown signal");
            token.cancel();
            tokio::time::sleep(Duration::from_secs(5)).await;
            abort_token.cancel();
        }
    });

    loop {
        tokio::select! {
            r = set.join_next() => match r {
                Some(Ok(_)) => {},
                Some(Err(e)) => error!("{e}"),
                None => {
                    info!("MailCrab graceful shutdown successful");

                    return 0;
                },
            },
            _ = abort_token.cancelled() => {
                set.abort_all();
                error!("MailCrab service aborted");

                return 1;
            }
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[tokio::main]
async fn main() {
    // initialize logging
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "mailcrab_backend=info,tower_http=info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let exit_code = run().await;

    process::exit(exit_code);
}
