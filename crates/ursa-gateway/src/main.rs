mod admin;
mod cache;
mod cli;
mod config;
mod resolver;
mod server;
mod util;
mod worker;

use std::{path::PathBuf, str::FromStr, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use clap::Parser;
use cli::{Cli, Commands};
use config::{init_config, load_config};
use hyper::Body;
use hyper_tls::HttpsConnector;
use resolver::Resolver;
use tokio::{
    select,
    signal::{
        ctrl_c,
        unix::{signal, SignalKind},
    },
    spawn,
    sync::{
        broadcast::{self, Sender},
        mpsc::{self},
        RwLock,
    },
    task::JoinHandle,
};
use tracing::{error, info, info_span, Instrument, Level};
use ursa_telemetry::TelemetryConfig;
use worker::cache::Cache;

#[tokio::main]
async fn main() -> Result<()> {
    let Cli {
        log,
        config,
        command,
    } = Cli::parse();

    let config_path = PathBuf::from(config);
    init_config(&config_path)
        .with_context(|| format!("Failed to init config from: {config_path:?}"))?;
    let mut gateway_config = load_config(&config_path)
        .with_context(|| format!("Failed to load config from: {config_path:?}"))?;

    // sync
    gateway_config.merge_log_level(log);

    // override log level if present in cli opts
    let log_level = log.unwrap_or(Level::from_str(&gateway_config.log_level)?);

    TelemetryConfig::new("ursa-gateway")
        .with_log_level(log_level.as_str())
        .with_pretty_log()
        .with_jaeger_tracer()
        .init()?;

    match command {
        Commands::Daemon(opts) => {
            let _s = info_span!("Daemon start").entered();

            // sync
            gateway_config.merge_daemon_opts(opts);

            let ttl_cache_interval = gateway_config.worker.ttl_cache_interval;

            let resolver = Arc::new(Resolver::new(
                String::from(&gateway_config.indexer.cid_url),
                hyper::Client::builder().build::<_, Body>(HttpsConnector::new()),
            ));

            let (worker_tx, worker_rx) = mpsc::unbounded_channel();
            let cache = Arc::new(RwLock::new(Cache::new(
                gateway_config.cache.max_size,
                gateway_config.cache.ttl_buf as u128 * 1_000_000, // ms to ns
                worker_tx.clone(),                                // cache command producer
                gateway_config.server.stream_buf,
                gateway_config.server.cache_control_max_size,
            )));
            let server_cache = Arc::clone(&cache);
            let admin_cache = Arc::clone(&server_cache);

            let server_config = Arc::new(RwLock::new(gateway_config));
            let admin_config = Arc::clone(&server_config);

            let (shutdown_tx, shutdown_rx) = broadcast::channel(3);

            let (server_worker, mut server_worker_signal_rx) = {
                let (signal_tx, signal_rx) = mpsc::channel(1);
                let worker = async move {
                    if let Err(e) = server::start(server_config, server_cache, shutdown_rx).await {
                        error!("[Server]: {e:?}");
                        signal_tx.send(()).await.expect("Send signal successfully");
                    };
                    info!("Server stopped");
                };
                (
                    spawn(worker.instrument(info_span!("Server worker"))),
                    signal_rx,
                )
            };

            let (admin_worker, mut admin_worker_signal_rx) = {
                let shutdown_rx = shutdown_tx.subscribe();
                let (signal_tx, signal_rx) = mpsc::channel(1);
                let worker = async move {
                    if let Err(e) = admin::start(admin_config, admin_cache, shutdown_rx).await {
                        error!("[Admin server]: {e:?}");
                        signal_tx.send(()).await.expect("Send signal successfully");
                    };
                    info!("Admin server stopped");
                };
                (
                    spawn(worker.instrument(info_span!("Admin worker"))),
                    signal_rx,
                )
            };

            let (ttl_cache_worker, mut ttl_cache_worker_signal_rx) = {
                let mut shutdown_rx = shutdown_tx.subscribe();
                let (signal_tx, signal_rx) = mpsc::channel(1);
                let worker = async move {
                    let duration_ms = Duration::from_millis(ttl_cache_interval);
                    info!("[Cache TTL Worker]: Interval: {duration_ms:?}");
                    loop {
                        let signal_tx = signal_tx.clone(); // move to ttl worker thread
                        select! {
                            _ = tokio::time::sleep(duration_ms) => {
                                if let Err(e) = worker_tx.send(worker::cache::CacheCommand::TtlCleanUp) {
                                    error!("[Cache TTL Worker]: {e:?}");
                                    signal_tx
                                        .send(())
                                        .await
                                        .expect("Send signal successfully");
                                }
                            },
                            _ = shutdown_rx.recv() => {
                                break;
                            }
                        }
                    }
                    info!("TTL cache worker stopped");
                };
                (
                    spawn(worker.instrument(info_span!("TTL cache worker"))),
                    signal_rx,
                )
            };

            // main worker to stop last
            let (main_worker, main_shutdown_tx, mut worker_signal_rx) = {
                let (signal_tx, signal_rx) = mpsc::channel(1);
                let (main_shutdown_tx, shutdown_rx) = mpsc::channel(1);
                let worker = worker::start(worker_rx, cache, resolver, signal_tx, shutdown_rx);
                (worker, main_shutdown_tx, signal_rx)
            };

            let workers = vec![server_worker, admin_worker, ttl_cache_worker];

            #[cfg(unix)]
            let terminate = async {
                signal(SignalKind::terminate())
                    .expect("Failed to install signal handler")
                    .recv()
                    .await;
            };

            #[cfg(not(unix))]
            let terminate = std::future::pending::<()>();

            select! {
                _ = ctrl_c() => graceful_shutdown(shutdown_tx, workers, main_shutdown_tx, main_worker).await,
                _ = terminate => graceful_shutdown(shutdown_tx, workers, main_shutdown_tx, main_worker).await,
                _ = server_worker_signal_rx.recv() => graceful_shutdown(shutdown_tx, workers, main_shutdown_tx, main_worker).await,
                _ = admin_worker_signal_rx.recv() => graceful_shutdown(shutdown_tx, workers, main_shutdown_tx, main_worker).await,
                _ = ttl_cache_worker_signal_rx.recv() => graceful_shutdown(shutdown_tx, workers, main_shutdown_tx, main_worker).await,
                _ = worker_signal_rx.recv() => graceful_shutdown(shutdown_tx, workers, main_shutdown_tx, main_worker).await
            }
            info!("Gateway shut down successfully")
        }
    }
    TelemetryConfig::teardown();
    Ok(())
}

async fn graceful_shutdown(
    shutdown_tx: Sender<()>,
    workers: Vec<JoinHandle<()>>,
    main_shutdown_tx: mpsc::Sender<()>,
    main_worker: JoinHandle<()>,
) {
    info!("Gateway shutting down...");
    shutdown_tx
        .send(())
        .expect("Send shutdown signal successfully");
    for worker in workers {
        worker.await.expect("Worker to shut down successfully");
    }
    main_shutdown_tx
        .send(())
        .await
        .expect("Send shutdown signal successfully");
    main_worker.await.expect("Worker to shut down successfully");
}
