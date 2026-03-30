mod api;
mod storage;
mod sync;

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use clap::Parser;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

use api::AppState;
use storage::PriceStore;
use sync::SyncConfig;

#[derive(Parser)]
#[command(name = "bitcoin-price-oracle")]
#[command(about = "Lightweight on-chain Bitcoin price oracle")]
struct Args {
    /// Bitcoin Core RPC host
    #[arg(long, env = "RPC_HOST", default_value = "127.0.0.1")]
    rpc_host: String,

    /// Bitcoin Core RPC port
    #[arg(long, env = "RPC_PORT", default_value = "8332")]
    rpc_port: u16,

    /// Bitcoin Core RPC user
    #[arg(long, env = "RPC_USER", default_value = "bitcoin")]
    rpc_user: String,

    /// Bitcoin Core RPC password
    #[arg(long, env = "RPC_PASS", default_value = "bitcoin")]
    rpc_pass: String,

    /// Bitcoin blocks directory (blk*.dat) for fast sync
    #[arg(long, env = "BLOCKS_DIR")]
    blocks_dir: Option<String>,

    /// Data directory for price storage
    #[arg(long, env = "DATA_DIR", default_value = "/data")]
    data_dir: String,

    /// HTTP server port
    #[arg(long, env = "PORT", default_value = "3200")]
    port: u16,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let store = Arc::new(PriceStore::open(std::path::Path::new(&args.data_dir)));
    let chain_tip = Arc::new(AtomicUsize::new(0));

    let state = AppState {
        store: store.clone(),
        chain_tip: chain_tip.clone(),
    };

    let sync_config = SyncConfig {
        rpc_url: format!("http://{}:{}", args.rpc_host, args.rpc_port),
        rpc_user: args.rpc_user,
        rpc_pass: args.rpc_pass,
        blocks_dir: args.blocks_dir.map(std::path::PathBuf::from),
    };

    // Spawn sync in background thread (blocking RPC calls)
    let sync_store = store.clone();
    let sync_tip = chain_tip.clone();
    tokio::task::spawn_blocking(move || {
        tokio::runtime::Handle::current().block_on(sync::run_sync(
            sync_store, sync_config, sync_tip,
        ));
    });

    // Start API server immediately
    let app = api::router(state);
    let addr = format!("0.0.0.0:{}", args.port);
    info!("API server starting on {}", addr);

    let listener = TcpListener::bind(&addr).await.expect("Failed to bind");
    axum::serve(listener, app)
        .await
        .expect("Server failed");
}
