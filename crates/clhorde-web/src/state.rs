//! Shared application state for axum handlers.

use std::path::PathBuf;
use std::sync::Arc;

use crate::bridge::DaemonBridge;

/// Shared state passed to all axum route handlers.
#[derive(Clone)]
pub struct AppState {
    pub bridge: Arc<DaemonBridge>,
}

impl AppState {
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            bridge: Arc::new(DaemonBridge::new(socket_path)),
        }
    }
}
