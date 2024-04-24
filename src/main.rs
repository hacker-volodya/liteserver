// Adopted from https://github.com/broxus/ton-indexer/blob/master/examples/simple_node.rs

mod liteserver;
mod web;
mod utils;

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use argh::FromArgs;
use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;

use ton_indexer::{Engine, GlobalConfig, NodeConfig, Subscriber};

#[global_allocator]
static GLOBAL: ton_indexer::alloc::Allocator = ton_indexer::alloc::allocator();

#[derive(Debug, FromArgs)]
#[argh(description = "")]
pub struct App {
    /// path to config
    #[argh(option, short = 'c', default = "String::from(\"config.yaml\")")]
    pub config: String,

    /// path to the global config with zerostate and static dht nodes
    #[argh(option, default = "String::from(\"ton-global.config.json\")")]
    pub global_config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let logger = tracing_subscriber::fmt().with_env_filter(
        EnvFilter::builder()
            .with_default_directive(tracing::Level::INFO.into())
            .from_env_lossy(),
    );

    logger.init();

    let any_signal = broxus_util::any_signal(broxus_util::TERMINATION_SIGNALS);

    let run = run(argh::from_env());

    tokio::select! {
        result = run => result,
        signal = any_signal => {
            if let Ok(signal) = signal {
                tracing::warn!(?signal, "received termination signal, flushing state...");
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
    // SAFETY: global allocator is set to jemalloc
    unsafe { ton_indexer::alloc::apply_config() };

    // TODO: option to configure using environment variables, auto-generate and store private keys
    let mut config: Config = broxus_util::read_config(app.config)?;
    config
        .indexer
        .ip_address
        .set_ip(broxus_util::resolve_public_ip(None).await?);

    let global_config = read_global_config(app.global_config)?;
    let engine = Engine::new(
        config.indexer,
        global_config.clone(),
        Arc::new(BlockSubscriber),
    )
    .await?;
    engine.start().await?;
    web::run(engine.clone()).await?;
    liteserver::run(engine.clone(), global_config);
    futures_util::future::pending().await
}

#[derive(Serialize, Deserialize)]
struct Config {
    indexer: NodeConfig,
}

fn read_global_config<T>(path: T) -> Result<GlobalConfig>
where
    T: AsRef<Path>,
{
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let config = serde_json::from_reader(reader)?;
    Ok(config)
}
