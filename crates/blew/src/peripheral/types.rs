use crate::types::DeviceId;
use tokio::sync::oneshot;
use uuid::Uuid;

/// Non-request events from the peripheral role. Clone-able; multiple subscribers welcome.
#[derive(Debug, Clone)]
pub enum PeripheralStateEvent {
    /// The local Bluetooth adapter was powered on or off.
    AdapterStateChanged { powered: bool },

    /// A remote central subscribed to or unsubscribed from a characteristic's notifications.
    SubscriptionChanged {
        client_id: DeviceId,
        char_uuid: Uuid,
        subscribed: bool,
    },
}

/// Inbound GATT requests from remote centrals.
///
/// Each variant carries an owned responder that must be consumed exactly once,
/// or dropped (which sends an ATT Application Error automatically). Single-consumer:
/// the request stream is handed out via [`Peripheral::take_requests`].
#[derive(Debug)]
pub enum PeripheralRequest {
    /// A remote central sent an ATT Read Request.
    Read {
        client_id: DeviceId,
        service_uuid: Uuid,
        char_uuid: Uuid,
        offset: u16,
        responder: ReadResponder,
    },

    /// A remote central sent an ATT Write Request or Write Command.
    /// `responder` is `Some` for Write Request (needs an ATT response) and
    /// `None` for Write Without Response.
    Write {
        client_id: DeviceId,
        service_uuid: Uuid,
        char_uuid: Uuid,
        value: Vec<u8>,
        responder: Option<WriteResponder>,
    },
}

pub(crate) type ReadResponseTx = oneshot::Sender<Result<Vec<u8>, ()>>;
pub(crate) type WriteResponseTx = oneshot::Sender<bool>;

#[derive(Debug)]
struct ResponderInner<T: Send + Clone + 'static> {
    tx: Option<oneshot::Sender<T>>,
    error_value: T,
}

impl<T: Send + Clone + 'static> ResponderInner<T> {
    fn new(tx: oneshot::Sender<T>, error_value: T) -> Self {
        Self {
            tx: Some(tx),
            error_value,
        }
    }

    fn send(&mut self, value: T) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(value);
        }
    }
}

impl<T: Send + Clone + 'static> Drop for ResponderInner<T> {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(self.error_value.clone());
        }
    }
}

/// Owned handle for responding to a GATT read request.
///
/// Call [`respond`](Self::respond) to send a value or [`error`](Self::error) to send an ATT
/// Application Error. If this is dropped without being consumed, an ATT Application Error is
/// sent automatically.
#[derive(Debug)]
pub struct ReadResponder(ResponderInner<Result<Vec<u8>, ()>>);

impl ReadResponder {
    pub(crate) fn new(tx: ReadResponseTx) -> Self {
        Self(ResponderInner::new(tx, Err(())))
    }

    /// Send `value` as the read response.
    pub fn respond(mut self, value: Vec<u8>) {
        self.0.send(Ok(value));
    }

    /// Send an ATT Application Error response.
    pub fn error(mut self) {
        self.0.send(Err(()));
    }
}

/// Owned handle for acknowledging a GATT write request.
///
/// Call [`success`](Self::success) or [`error`](Self::error). Dropping without consuming
/// sends an error response automatically.
#[derive(Debug)]
pub struct WriteResponder(ResponderInner<bool>);

impl WriteResponder {
    pub(crate) fn new(tx: WriteResponseTx) -> Self {
        Self(ResponderInner::new(tx, false))
    }

    /// Acknowledge the write successfully.
    pub fn success(mut self) {
        self.0.send(true);
    }

    /// Send an ATT Application Error response.
    pub fn error(mut self) {
        self.0.send(false);
    }
}

/// Configuration for a BLE advertisement.
#[derive(Debug, Clone)]
pub struct AdvertisingConfig {
    pub local_name: String,
    pub service_uuids: Vec<Uuid>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn test_read_responder_respond() {
        let (tx, rx) = oneshot::channel();
        let responder = ReadResponder::new(tx);
        responder.respond(b"hello".to_vec());
        assert_eq!(rx.await.unwrap().unwrap(), b"hello");
    }

    #[tokio::test]
    async fn test_read_responder_error() {
        let (tx, rx) = oneshot::channel();
        let responder = ReadResponder::new(tx);
        responder.error();
        assert!(rx.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn test_read_responder_drop_sends_error() {
        let (tx, rx) = oneshot::channel();
        {
            let _responder = ReadResponder::new(tx);
        }
        assert!(
            rx.await.unwrap().is_err(),
            "drop must send error automatically"
        );
    }

    #[tokio::test]
    async fn test_read_responder_respond_empty() {
        let (tx, rx) = oneshot::channel();
        let responder = ReadResponder::new(tx);
        responder.respond(vec![]);
        assert_eq!(rx.await.unwrap().unwrap(), Vec::<u8>::new());
    }

    #[tokio::test]
    async fn test_write_responder_success() {
        let (tx, rx) = oneshot::channel();
        let responder = WriteResponder::new(tx);
        responder.success();
        assert!(rx.await.unwrap());
    }

    #[tokio::test]
    async fn test_write_responder_error() {
        let (tx, rx) = oneshot::channel();
        let responder = WriteResponder::new(tx);
        responder.error();
        assert!(!rx.await.unwrap());
    }

    #[tokio::test]
    async fn test_write_responder_drop_sends_error() {
        let (tx, rx) = oneshot::channel();
        {
            let _responder = WriteResponder::new(tx);
        }
        assert!(!rx.await.unwrap(), "drop must send false");
    }
    #[tokio::test]
    async fn test_read_responder_consumed_once() {
        let (tx, rx) = oneshot::channel();
        let responder = ReadResponder::new(tx);
        responder.respond(b"data".to_vec());
        let result = rx.await.unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_write_responder_consumed_once() {
        let (tx, rx) = oneshot::channel();
        let responder = WriteResponder::new(tx);
        responder.success();
        let result = rx.await.unwrap();
        assert!(result);
    }
}
