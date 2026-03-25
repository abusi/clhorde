//! Shared application state for the axum server.

use std::sync::Arc;

use crate::bridge::DaemonBridge;

/// Shared state available to all axum handlers via `State<AppState>`.
#[derive(Clone)]
pub struct AppState {
    pub bridge: Arc<DaemonBridge>,
}

impl AppState {
    pub fn new(bridge: Arc<DaemonBridge>) -> Self {
        Self { bridge }
    }
}
