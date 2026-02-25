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

    /// Number of currently registered sessions (for testing).
    #[cfg(test)]
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_session_returns_incrementing_ids() {
        let mut sm = SessionManager::new();
        let (tx1, _rx1) = mpsc::unbounded_channel();
        let (tx2, _rx2) = mpsc::unbounded_channel();
        let id1 = sm.add_session(tx1);
        let id2 = sm.add_session(tx2);
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(sm.session_count(), 2);
    }

    #[test]
    fn remove_session_drops_session() {
        let mut sm = SessionManager::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let id = sm.add_session(tx);
        assert_eq!(sm.session_count(), 1);
        sm.remove_session(id);
        assert_eq!(sm.session_count(), 0);
    }

    #[test]
    fn broadcast_only_sends_to_subscribed() {
        let mut sm = SessionManager::new();
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        let id1 = sm.add_session(tx1);
        let _id2 = sm.add_session(tx2);
        sm.set_subscribed(id1, true);
        // id2 is NOT subscribed

        sm.broadcast(&DaemonEvent::Pong);

        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_err()); // unsubscribed, no message
    }

    #[test]
    fn broadcast_removes_disconnected_sessions() {
        let mut sm = SessionManager::new();
        let (tx1, rx1) = mpsc::unbounded_channel();
        let (tx2, _rx2) = mpsc::unbounded_channel();
        let id1 = sm.add_session(tx1);
        let id2 = sm.add_session(tx2);
        sm.set_subscribed(id1, true);
        sm.set_subscribed(id2, true);

        // Drop rx1 to simulate disconnect
        drop(rx1);

        sm.broadcast(&DaemonEvent::Pong);
        // Session 1 should have been removed
        assert_eq!(sm.session_count(), 1);
    }

    #[test]
    fn send_to_unknown_session_returns_false() {
        let mut sm = SessionManager::new();
        let result = sm.send_to(999, DaemonEvent::Pong);
        assert!(!result);
    }

    #[test]
    fn send_to_disconnected_removes_and_returns_false() {
        let mut sm = SessionManager::new();
        let (tx, rx) = mpsc::unbounded_channel();
        let id = sm.add_session(tx);
        drop(rx); // simulate disconnect

        let result = sm.send_to(id, DaemonEvent::Pong);
        assert!(!result);
        assert_eq!(sm.session_count(), 0);
    }

    #[test]
    fn set_subscribed_toggles() {
        let mut sm = SessionManager::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let id = sm.add_session(tx);

        // Not subscribed — broadcast should not deliver
        sm.broadcast(&DaemonEvent::Pong);
        assert!(rx.try_recv().is_err());

        // Subscribe — broadcast should deliver
        sm.set_subscribed(id, true);
        sm.broadcast(&DaemonEvent::Pong);
        assert!(rx.try_recv().is_ok());

        // Unsubscribe — broadcast should not deliver
        sm.set_subscribed(id, false);
        sm.broadcast(&DaemonEvent::Pong);
        assert!(rx.try_recv().is_err());
    }
}
