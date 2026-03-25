//! Shared application state for the axum server.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::bridge::DaemonBridge;

/// Shared state available to all axum handlers via `State<AppState>`.
#[derive(Clone)]
pub struct AppState {
    pub bridge: Arc<DaemonBridge>,
    /// Number of active WebSocket connections.
    pub ws_connections: Arc<AtomicUsize>,
}

impl AppState {
    pub fn new(bridge: Arc<DaemonBridge>) -> Self {
        Self {
            bridge,
            ws_connections: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Increment the WebSocket connection count, returning the new value.
    pub fn ws_connect(&self) -> usize {
        self.ws_connections.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Decrement the WebSocket connection count, returning the new value.
    pub fn ws_disconnect(&self) -> usize {
        self.ws_connections.fetch_sub(1, Ordering::Relaxed) - 1
    }

    /// Current number of active WebSocket connections.
    pub fn ws_count(&self) -> usize {
        self.ws_connections.load(Ordering::Relaxed)
    }
}
