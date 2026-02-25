use tokio::sync::mpsc;

use clhorde_core::protocol::DaemonEvent;

pub struct ClientSession {
    pub id: usize,
    pub event_tx: mpsc::UnboundedSender<DaemonEvent>,
    pub subscribed: bool,
}

pub struct SessionManager {
    sessions: Vec<ClientSession>,
    #[allow(dead_code)]
    next_session_id: usize,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            next_session_id: 1,
        }
    }

    /// Register a new client, returning its session ID.
    #[allow(dead_code)]
    pub fn add_session(&mut self, event_tx: mpsc::UnboundedSender<DaemonEvent>) -> usize {
        let id = self.next_session_id;
        self.next_session_id += 1;
        self.sessions.push(ClientSession {
            id,
            event_tx,
            subscribed: false,
        });
        id
    }

    /// Register a client with a pre-assigned session ID.
    pub fn add_session_with_id(&mut self, id: usize, event_tx: mpsc::UnboundedSender<DaemonEvent>) {
        self.sessions.push(ClientSession {
            id,
            event_tx,
            subscribed: false,
        });
    }

    /// Remove a client session by ID.
    pub fn remove_session(&mut self, id: usize) {
        self.sessions.retain(|s| s.id != id);
    }

    /// Toggle subscription for a client.
    pub fn set_subscribed(&mut self, id: usize, subscribed: bool) {
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == id) {
            session.subscribed = subscribed;
        }
    }

    /// Broadcast a DaemonEvent to all subscribed clients.
    /// Removes clients whose channels have disconnected.
    pub fn broadcast(&mut self, event: &DaemonEvent) {
        self.sessions.retain(|session| {
            if !session.subscribed {
                return true; // keep unsubscribed sessions
            }
            session.event_tx.send(event.clone()).is_ok()
        });
    }

    /// Send an event to a specific client by session ID.
    /// Returns false if the client is disconnected (and removes it).
    pub fn send_to(&mut self, session_id: usize, event: DaemonEvent) -> bool {
        if let Some(pos) = self.sessions.iter().position(|s| s.id == session_id) {
            if self.sessions[pos].event_tx.send(event).is_ok() {
                return true;
            }
            self.sessions.remove(pos);
        }
        false
    }
}
