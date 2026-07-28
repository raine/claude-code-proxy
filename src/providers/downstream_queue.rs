use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SendOutcome {
    Sent,
    Closed,
    Deadline,
    Stalled,
    TooLarge,
}

#[derive(Clone)]
pub(crate) struct ByteBudget {
    max_bytes: usize,
    permits: Arc<Semaphore>,
}

impl ByteBudget {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            permits: Arc::new(Semaphore::new(max_bytes)),
        }
    }

    #[cfg(test)]
    pub(crate) fn available_permits(&self) -> usize {
        self.permits.available_permits()
    }
}

pub(crate) struct BudgetedChunk {
    bytes: Bytes,
    _permit: Option<OwnedSemaphorePermit>,
}

impl BudgetedChunk {
    fn new(bytes: Vec<u8>, permit: OwnedSemaphorePermit) -> Self {
        Self {
            bytes: Bytes::from(bytes),
            _permit: Some(permit),
        }
    }

    pub(crate) fn unbudgeted(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Bytes::from(bytes),
            _permit: None,
        }
    }

    pub(crate) fn into_bytes(self) -> Bytes {
        self.bytes
    }
}

pub(crate) async fn send_before_deadline<T>(
    tx: &mpsc::Sender<T>,
    byte_budget: &ByteBudget,
    bytes: Vec<u8>,
    deadline: Instant,
    stall_timeout: Duration,
    wrap: impl FnOnce(BudgetedChunk) -> T,
) -> SendOutcome {
    if bytes.len() > byte_budget.max_bytes {
        return SendOutcome::TooLarge;
    }
    let Ok(byte_count) = u32::try_from(bytes.len()) else {
        return SendOutcome::TooLarge;
    };
    if Instant::now() >= deadline {
        return SendOutcome::Deadline;
    }

    let stall_deadline = Instant::now() + stall_timeout;
    let byte_permit = tokio::select! {
        biased;
        _ = tokio::time::sleep_until(deadline) => return SendOutcome::Deadline,
        _ = tokio::time::sleep_until(stall_deadline) => return SendOutcome::Stalled,
        _ = tx.closed() => return SendOutcome::Closed,
        permit = byte_budget.permits.clone().acquire_many_owned(byte_count) => {
            match permit {
                Ok(permit) => permit,
                Err(_) => return SendOutcome::Closed,
            }
        }
    };

    tokio::select! {
        biased;
        _ = tokio::time::sleep_until(deadline) => SendOutcome::Deadline,
        _ = tokio::time::sleep_until(stall_deadline) => SendOutcome::Stalled,
        result = tx.send(wrap(BudgetedChunk::new(bytes, byte_permit))) => {
            if result.is_ok() {
                SendOutcome::Sent
            } else {
                SendOutcome::Closed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn filled_channel() -> (mpsc::Sender<BudgetedChunk>, mpsc::Receiver<BudgetedChunk>) {
        let (tx, rx) = mpsc::channel(1);
        tx.send(BudgetedChunk::unbudgeted(Vec::new()))
            .await
            .unwrap();
        (tx, rx)
    }

    #[tokio::test]
    async fn queued_payload_holds_its_byte_permit_until_dropped() {
        let (tx, mut rx) = mpsc::channel(2);
        let budget = ByteBudget::new(4);
        let deadline = Instant::now() + Duration::from_secs(1);

        assert_eq!(
            send_before_deadline(
                &tx,
                &budget,
                vec![1; 4],
                deadline,
                Duration::from_secs(1),
                std::convert::identity,
            )
            .await,
            SendOutcome::Sent
        );
        assert_eq!(budget.available_permits(), 0);

        let payload = rx.recv().await.expect("payload should be queued");
        assert_eq!(payload.into_bytes(), Bytes::from(vec![1; 4]));
        assert_eq!(budget.available_permits(), 4);
    }

    #[tokio::test]
    async fn oversized_payload_is_rejected_without_consuming_budget() {
        let (tx, mut rx) = mpsc::channel(1);
        let budget = ByteBudget::new(4);

        assert_eq!(
            send_before_deadline(
                &tx,
                &budget,
                vec![0; 5],
                Instant::now() + Duration::from_secs(1),
                Duration::from_secs(1),
                std::convert::identity,
            )
            .await,
            SendOutcome::TooLarge
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(budget.available_permits(), 4);
    }

    #[tokio::test]
    async fn deadline_and_closed_receiver_are_reported_without_enqueueing() {
        let (deadline_tx, mut deadline_rx) = mpsc::channel(1);
        let budget = ByteBudget::new(4);
        assert_eq!(
            send_before_deadline(
                &deadline_tx,
                &budget,
                vec![1],
                Instant::now(),
                Duration::from_secs(1),
                std::convert::identity,
            )
            .await,
            SendOutcome::Deadline
        );
        assert!(deadline_rx.try_recv().is_err());

        let (closed_tx, closed_rx) = mpsc::channel(1);
        drop(closed_rx);
        assert_eq!(
            send_before_deadline(
                &closed_tx,
                &budget,
                vec![1],
                Instant::now() + Duration::from_secs(1),
                Duration::from_secs(1),
                std::convert::identity,
            )
            .await,
            SendOutcome::Closed
        );
        assert_eq!(budget.available_permits(), 4);
    }

    #[tokio::test]
    async fn exhausted_byte_budget_reports_stalled() {
        let (tx, _rx) = mpsc::channel(1);
        let budget = ByteBudget::new(1);
        let held_permit = budget.permits.clone().acquire_owned().await.unwrap();

        assert_eq!(
            send_before_deadline(
                &tx,
                &budget,
                vec![1],
                Instant::now() + Duration::from_secs(1),
                Duration::from_millis(10),
                std::convert::identity,
            )
            .await,
            SendOutcome::Stalled
        );

        drop(held_permit);
        assert_eq!(budget.available_permits(), 1);
    }

    #[tokio::test]
    async fn full_channel_releases_acquired_budget_on_deadline_or_stall() {
        for (deadline, stall_timeout, expected) in [
            (
                Instant::now() + Duration::from_millis(10),
                Duration::from_secs(1),
                SendOutcome::Deadline,
            ),
            (
                Instant::now() + Duration::from_secs(1),
                Duration::from_millis(10),
                SendOutcome::Stalled,
            ),
        ] {
            let (tx, _rx) = filled_channel().await;
            let budget = ByteBudget::new(1);

            assert_eq!(
                send_before_deadline(
                    &tx,
                    &budget,
                    vec![1],
                    deadline,
                    stall_timeout,
                    std::convert::identity,
                )
                .await,
                expected
            );
            assert_eq!(budget.available_permits(), 1);
        }
    }

    #[tokio::test]
    async fn full_channel_releases_acquired_budget_when_receiver_closes() {
        let (tx, rx) = filled_channel().await;
        let budget = ByteBudget::new(1);
        let task_budget = budget.clone();
        let task = tokio::spawn(async move {
            send_before_deadline(
                &tx,
                &task_budget,
                vec![1],
                Instant::now() + Duration::from_secs(1),
                Duration::from_secs(1),
                std::convert::identity,
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            while budget.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("sender should acquire the byte permit");
        drop(rx);

        assert_eq!(task.await.unwrap(), SendOutcome::Closed);
        assert_eq!(budget.available_permits(), 1);
    }
}
