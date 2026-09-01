//! Binary entry point. Everything else lives in the library so tests can serve the router in-process.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use app::domain::World;
use app::{router, AppState, Shared};
use clap::Parser;
use tokio::sync::broadcast;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:47310")]
    bind: SocketAddr,
    #[arg(long, default_value_t = 1)]
    seed: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let args = Args::parse();
    let (tx, _) = broadcast::channel(4096);
    let state: Shared = Arc::new(AppState { world: Mutex::new(World::seeded(args.seed)), events: tx });
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    eprintln!("target app on http://{}  (seed {})", args.bind, args.seed);
    axum::serve(listener, router(state)).await?;
    Ok(())
}
