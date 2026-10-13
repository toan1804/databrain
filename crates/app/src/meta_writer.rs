//! Explorer cache writes in the background.
//!
//! Listing a schema returns what the server sent at once; saving it to the
//! app-data SQLite (and reading object versions for the next incremental
//! refresh) happens afterwards, so the explorer never waits for the disk.
//! Jobs run one at a time in the order they were queued, so an older listing
//! never overwrites a newer one.

use std::future::Future;
use std::pin::Pin;

use tokio::sync::{mpsc, oneshot};

type Job = Pin<Box<dyn Future<Output = ()> + Send>>;

#[derive(Default)]
pub struct MetaWriter {
    tx: parking_lot::Mutex<Option<mpsc::UnboundedSender<Job>>>,
}

impl MetaWriter {
    /// Queue a write. Needs a Tokio runtime (the worker starts on first use,
    /// and again if the runtime it ran on has gone, e.g. between tests).
    pub fn push(&self, job: impl Future<Output = ()> + Send + 'static) {
        let mut job: Job = Box::pin(job);
        let mut tx = self.tx.lock();
        for _ in 0..2 {
            let sender = tx.get_or_insert_with(|| {
                let (s, mut rx) = mpsc::unbounded_channel::<Job>();
                tokio::spawn(async move {
                    while let Some(j) = rx.recv().await {
                        j.await;
                    }
                });
                s
            });
            match sender.send(job) {
                Ok(()) => return,
                Err(mpsc::error::SendError(j)) => {
                    job = j;
                    *tx = None;
                }
            }
        }
    }

    /// Queue blocking SQLite work.
    pub fn push_blocking(&self, f: impl FnOnce() + Send + 'static) {
        self.push(async move {
            let _ = tokio::task::spawn_blocking(f).await;
        });
    }

    /// Wait until everything queued so far is written.
    pub async fn flush(&self) {
        if self.tx.lock().is_none() {
            return;
        }
        let (t, r) = oneshot::channel();
        self.push(async move {
            let _ = t.send(());
        });
        let _ = r.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn runs_in_order_and_flushes() {
        let w = MetaWriter::default();
        let log = Arc::new(parking_lot::Mutex::new(Vec::new()));
        for i in 0..20 {
            let log = log.clone();
            w.push(async move {
                // Later jobs must not overtake a slow earlier one.
                if i == 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                }
                log.lock().push(i);
            });
        }
        let l2 = log.clone();
        w.push_blocking(move || l2.lock().push(99));
        w.flush().await;
        let want: Vec<i32> = (0..20).chain([99]).collect();
        assert_eq!(*log.lock(), want);
    }
}
