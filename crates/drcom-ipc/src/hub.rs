//! Fan-out of service notifications to several connections.
//!
//! A daemon subscribes to the service **once**. Every notification is serialised
//! once by [`Hub::publish`] and pushed as the same `Arc<str>` line into bounded
//! per-client queues, so a client that stops reading cannot block the service
//! thread: its queue fills and the hub silently drops it from the broadcast. Dead
//! clients are removed the same way.

use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use drcom_service::Notification;

use crate::protocol::encode_json_line;

/// How many encoded notification lines may wait for one client before the hub
/// gives up on it.
pub const CLIENT_QUEUE_LEN: usize = 64;

/// Opaque identifier for one subscribed client.
pub type ClientId = u64;

/// Broadcast hub shared by all TCP clients of one daemon.
#[derive(Clone, Default)]
pub struct Hub {
    inner: Arc<Mutex<HubInner>>,
}

#[derive(Default)]
struct HubInner {
    next_id: ClientId,
    clients: Vec<ClientEntry>,
}

struct ClientEntry {
    id: ClientId,
    sender: SyncSender<Arc<str>>,
}

impl Hub {
    /// Creates an empty hub.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a client. The receiver yields encoded JSON lines (including the
    /// trailing newline), in publish order.
    pub fn subscribe(&self) -> (ClientId, Receiver<Arc<str>>) {
        let (sender, receiver) = mpsc::sync_channel(CLIENT_QUEUE_LEN);
        let mut inner = self.lock();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.clients.push(ClientEntry { id, sender });
        (id, receiver)
    }

    /// Removes a client; the writer thread sees the channel close after the
    /// already-queued lines.
    pub fn unsubscribe(&self, id: ClientId) {
        self.lock().clients.retain(|client| client.id != id);
    }

    /// Number of currently registered clients (including slow ones that will be
    /// dropped on the next publish).
    pub fn client_count(&self) -> usize {
        self.lock().clients.len()
    }

    /// Serialises one notification once and queues it for every client.
    ///
    /// Returns the serialisation error, if any; clients whose queue is full or
    /// whose receiver is gone are removed in the same pass and never block the
    /// caller.
    pub fn publish(&self, notification: &Notification) -> Result<(), serde_json::Error> {
        let line: Arc<str> = encode_json_line(notification)?.into();
        let mut inner = self.lock();
        let mut removed = Vec::new();
        for (index, client) in inner.clients.iter().enumerate() {
            match client.sender.try_send(Arc::clone(&line)) {
                Ok(()) => {}
                Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => removed.push(index),
            }
        }
        for index in removed.into_iter().rev() {
            inner.clients.remove(index);
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HubInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drcom_service::{Service, ServiceOptions};

    fn state_notification() -> Notification {
        Notification::State {
            snapshot: Service::new(ServiceOptions::for_test()).snapshot(),
        }
    }

    #[test]
    fn publishes_the_same_line_to_every_client() {
        let hub = Hub::new();
        let (first_id, first) = hub.subscribe();
        let (second_id, second) = hub.subscribe();
        assert_eq!(hub.client_count(), 2);

        hub.publish(&state_notification()).expect("serialise");

        let first_line = first.recv().expect("first client line");
        let second_line = second.recv().expect("second client line");
        assert!(Arc::ptr_eq(&first_line, &second_line));
        assert!(first_line.starts_with("{\"event\":\"state\""));
        assert!(first_line.ends_with('\n'));

        hub.unsubscribe(first_id);
        hub.unsubscribe(second_id);
        assert_eq!(hub.client_count(), 0);
    }

    #[test]
    fn drops_a_slow_client_without_blocking_others() {
        let hub = Hub::new();
        let (_slow_id, slow) = hub.subscribe();
        let (_fast_id, fast) = hub.subscribe();

        // Fill the slow client's queue (and a little more) while draining the
        // fast one; the slow client must be dropped instead of blocking.
        for _ in 0..(CLIENT_QUEUE_LEN + 5) {
            hub.publish(&state_notification()).expect("serialise");
            // Draining may race with the publish that drops the client, so a
            // closed channel is expected after the cap is exceeded.
            let _ = fast.recv();
        }

        assert_eq!(hub.client_count(), 1);
        assert!(slow.recv().is_ok(), "queued lines stay available");
        hub.publish(&state_notification()).expect("serialise");
        assert!(fast.recv().is_ok());
    }

    #[test]
    fn removes_a_client_whose_receiver_is_gone() {
        let hub = Hub::new();
        let (id, receiver) = hub.subscribe();
        drop(receiver);
        hub.publish(&state_notification()).expect("serialise");
        assert_eq!(hub.client_count(), 0);
        hub.unsubscribe(id);
    }
}
