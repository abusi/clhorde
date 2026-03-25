use std::net::SocketAddr;
use std::path::PathBuf;
use std::process;

use clap::Parser;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use clhorde_core::ipc::daemon_socket_path;

mod auth;
mod bridge;
mod routes;
mod state;
mod static_files;
mod ws;

/// clhorde-web — HTTP/WebSocket bridge for the clhorde daemon
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Port to listen on
    #[arg(long, default_value_t = 3120)]
    port: u16,

    /// Host address to bind to
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Override path to static files directory
    #[arg(long)]
    static_dir: Option<PathBuf>,

    /// Override path to daemon Unix socket
    #[arg(long)]
    daemon_socket: Option<PathBuf>,

    /// Require this token for API access (Bearer auth). Also reads CLHORDE_WEB_AUTH_TOKEN env var.
    #[arg(long, env = "CLHORDE_WEB_AUTH_TOKEN")]
    auth_token: Option<String>,

    /// Allow CORS requests from this origin (e.g. http://localhost:5173 for development)
    #[arg(long, env = "CLHORDE_WEB_CORS_ORIGIN")]
    cors_origin: Option<String>,

    /// Enable verbose logging (-v info, -vv debug)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

impl Cli {
    fn log_level(&self) -> tracing::Level {
        match self.verbose {
            0 => tracing::Level::WARN,
            1 => tracing::Level::INFO,
            _ => tracing::Level::DEBUG,
        }
    }

    fn socket_path(&self) -> PathBuf {
        self.daemon_socket
            .clone()
            .unwrap_or_else(daemon_socket_path)
    }
}

fn init_tracing(level: tracing::Level) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level.to_string()));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_tracing(cli.log_level());

    let socket_path = cli.socket_path();
    info!(
        daemon_socket = %socket_path.display(),
        static_dir = ?cli.static_dir,
        "starting clhorde-web"
    );

    let bridge = match bridge::DaemonBridge::connect(socket_path).await {
        Ok(b) => b,
        Err(e) => {
            error!(%e, "failed to connect to daemon");
            eprintln!(
                "Failed to connect to daemon. Is it running? Start with: clhorded"
            );
            process::exit(1);
        }
    };

    let static_source = match cli.static_dir {
        Some(dir) => {
            info!(path = %dir.display(), "serving static files from filesystem");
            static_files::StaticSource::Directory(dir)
        }
        None => {
            info!("serving embedded static files");
            static_files::StaticSource::Embedded
        }
    };

    let app_state = state::AppState::new(bridge);
    let app = routes::router(
        app_state,
        static_source,
        cli.auth_token.clone(),
        cli.cors_origin.clone(),
    );

    let addr: SocketAddr = match format!("{}:{}", cli.host, cli.port).parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("Invalid address {}:{} — {e}", cli.host, cli.port);
            process::exit(1);
        }
    };

    // Warn if binding to non-localhost without auth
    let is_localhost = cli.host == "127.0.0.1" || cli.host == "::1" || cli.host == "localhost";
    if !is_localhost && cli.auth_token.is_none() {
        eprintln!("Warning: serving without authentication on a network interface");
        tracing::warn!("serving without authentication on a network interface — consider --auth-token");
    }

    info!(%addr, "listening");

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind {addr}: {e}");
            process::exit(1);
        }
    };

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("Server error: {e}");
        process::exit(1);
    }
}
