//! HTTP/REST bridge for the clhorde daemon.
//!
//! Connects to the daemon via Unix domain socket and exposes a REST API
//! for querying state and controlling prompts/workers.

mod bridge;
mod routes;
mod state;

use clap::Parser;
use std::path::PathBuf;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "clhorde-web", about = "HTTP bridge for clhorde daemon")]
struct Args {
    /// Port to listen on
    #[arg(long, default_value = "3120")]
    port: u16,

    /// Host to bind to
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Override path for static file serving
    #[arg(long)]
    static_dir: Option<PathBuf>,

    /// Override daemon socket path
    #[arg(long)]
    daemon_socket: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let socket_path = args
        .daemon_socket
        .unwrap_or_else(|| clhorde_core::ipc::daemon_socket_path());

    let app_state = state::AppState::new(socket_path);
    let app = routes::router(app_state);

    let addr = format!("{}:{}", args.host, args.port);
    info!("Starting clhorde-web on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind to address");

    axum::serve(listener, app)
        .await
        .expect("Server error");
}
