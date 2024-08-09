// Adopted from https://github.com/broxus/ton-indexer/blob/master/examples/simple_node.rs

mod liteserver;
mod web;
mod utils;
mod tracing_utils;
mod tvm;

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use argh::FromArgs;
use metrics_exporter_prometheus::PrometheusBuilder;
use serde::{Deserialize, Serialize};

use ton_indexer::{Engine, GlobalConfig, NodeConfig, Subscriber};

#[global_allocator]
static GLOBAL: ton_indexer::alloc::Allocator = ton_indexer::alloc::allocator();

#[derive(Debug, FromArgs)]
#[argh(description = "")]
pub struct App {
    /// path to node config
    #[argh(option, short = 'c')]
    pub config: Option<String>,

    /// path to the global config with zerostate and static dht nodes
    #[argh(option)]
    pub global_config_path: Option<String>,

    /// url to the global config with zerostate and static dht nodes
    #[argh(option)]
    pub global_config_url: Option<String>,

    /// if path or url are not specified, download global config from https://ton.org/global.config.json (default) or https://ton.org/testnet-global.config.json (--testnet flag)
    #[argh(switch)]
    pub testnet: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_utils::init();

    let any_signal = broxus_util::any_signal(broxus_util::TERMINATION_SIGNALS);

    let run = run(argh::from_env());

    tokio::select! {
        result = run => result,
        signal = any_signal => {
            if let Ok(signal) = signal {
                tracing::warn!(?signal, "received termination signal, flushing state...");
                tracing_utils::shutdown();
            }
            // NOTE: engine future is safely dropped here so rocksdb method
            // `rocksdb_close` is called in DB object destructor
            Ok(())
        }
    }
}

struct BlockSubscriber;

impl Subscriber for BlockSubscriber {}

async fn run(app: App) -> Result<()> {
    if app.global_config_path.is_some() && app.global_config_url.is_some() {
        anyhow::bail!("only one of --global-config-path and --global-config-url can be specified");
    } else if app.testnet && (app.global_config_path.is_some() || app.global_config_url.is_some()) {
        anyhow::bail!("--testnet flag cannot be used with --global-config-path or --global-config-url");
    }

    // SAFETY: global allocator is set to jemalloc
    tracing::info!("Applying allocator config...");
    unsafe { ton_indexer::alloc::apply_config() };

    // TODO: option to configure using environment variables, auto-generate and store private keys
    let mut config = if let Some(config) = app.config {
        broxus_util::read_config(config)?
    } else {
        tracing::info!("-c option is not set, using default node configuration");
        Config::default()
    };
    tracing::info!(?config, "Node configuration is done");

    tracing::info!("RocksDB size is {} GB", fs_extra::dir::get_size(&config.indexer.rocks_db_path).unwrap_or(0) >> 30);
    tracing::info!("File DB size is {} GB", fs_extra::dir::get_size(&config.indexer.file_db_path).unwrap_or(0) >> 30);

    tracing::info!("Resolving public IPv4 for node ADNL->IP DHT entry...");
    config
        .indexer
        .ip_address
        .set_ip(broxus_util::resolve_public_ip(None).await?);
    tracing::info!(?config.indexer.ip_address, "IP is resolved");

    // prometheus metrics
    let builder = PrometheusBuilder::new();
    let recorder = builder.install_recorder()?;
    prometheus::default_registry()
        .register(Box::new(
            tokio_metrics_collector::default_runtime_collector(),
        ))?;

    let global_config = if let Some(path) = app.global_config_path {
        read_global_config_from_path(path)?
    } else if let Some(url) = app.global_config_url {
        read_global_config_from_url(&url)?
    } else if app.testnet {
        read_global_config_from_url("https://ton.org/testnet-global.config.json")?
    } else {
        read_global_config_from_url("https://ton.org/global.config.json")?
    };

    tracing::info!("Initializing engine...");
    let engine = Engine::new(
        config.indexer,
        global_config.clone(),
        Arc::new(BlockSubscriber),
    )
    .await?;

    tracing::info!("Initializing web server...");
    web::run(engine.clone(), recorder).await?;

    tracing::info!("Initializing liteserver...");
    liteserver::run(engine.clone(), global_config);

    tracing::info!("Starting engine...");
    engine.start().await?;

    tracing::info!("Engine is running");
    futures_util::future::pending().await
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct Config {
    indexer: NodeConfig,
}

fn read_global_config_from_path<T>(path: T) -> Result<GlobalConfig>
where
    T: AsRef<Path> + std::fmt::Display,
{
    tracing::info!("Loading global config from path {path}");
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let config = serde_json::from_reader(reader)?;
    Ok(config)
}

fn read_global_config_from_url(url: &str) -> Result<GlobalConfig> {
    tracing::info!("Loading global config from url {url}");
    let response = ureq::get(url).call()
        .map_err(|e| anyhow::anyhow!("Error occurred while fetching config from {}: {:?}. Use --global-config-path if you have local config.", url, e))?;
    if response.status() != 200 {
        anyhow::bail!("Error occurred while fetching config from {}: {}", url, response.status());
    }
    let config = serde_json::from_reader(response.into_reader())?;
    Ok(config)
}