use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

use super::{
    scheduler::{PauseReason, Tracker},
    ConnectionEvents,
};

pub struct Graveyard {
    connections: HashMap<String, SavedState>,
}

impl Graveyard {
    pub fn new() -> Graveyard {
        Graveyard {
            connections: HashMap::new(),
        }
    }

    /// Add a new connection.
    /// Return tracker of previous connection if connection id already exists
    pub fn retrieve(&mut self, id: &str) -> Option<SavedState> {
        self.cleanup_expired_sessions();
        self.connections.remove(id)
    }

    fn cleanup_expired_sessions(&mut self) {
        let now = Instant::now();
        self.connections.retain(|_, state| {
            if let Some(expiry) = state.expiry {
                expiry > now
            } else {
                true
            }
        });
    }

    /// Save connection tracker
    pub fn save(
        &mut self,
        mut tracker: Tracker,
        subscriptions: HashSet<String>,
        metrics: ConnectionEvents,
        session_expiry_interval: Option<Instant>,
    ) {
        tracker.pause(PauseReason::Busy);
        let id = tracker.id.clone();

        self.connections.insert(
            id,
            SavedState {
                tracker,
                subscriptions,
                metrics,
                expiry: session_expiry_interval,
            },
        );
    }
}

#[derive(Debug)]
pub struct SavedState {
    pub tracker: Tracker,
    pub subscriptions: HashSet<String>,
    pub metrics: ConnectionEvents,
    pub expiry: Option<Instant>,
}

impl SavedState {
    pub fn new(client_id: String) -> SavedState {
        SavedState {
            tracker: Tracker::new(client_id),
            subscriptions: HashSet::new(),
            metrics: ConnectionEvents::default(),
            expiry: None,
        }
    }
}
