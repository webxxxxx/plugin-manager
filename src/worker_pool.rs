//! A small bounded worker pool for blocking \`Wire::call\` implementations.
//!
//! The daemon event loop must never block on a plugin response. Calls move to
//! these workers; each worker submits a \`SlotRequest\` and waits for the event
//! loop to complete it. The queue is bounded so a slow backend turns into
//! \`Busy\` instead of unbounded thread growth.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::threads;

type Job = Box<dyn FnOnce() + Send + 'static>;

pub(crate) struct WorkerPool {
    tx: Option<SyncSender<Job>>,
    workers: Vec<JoinHandle<()>>,
}

impl WorkerPool {
    pub(crate) fn new(workers: usize, queue: usize) -> WorkerPool {
        let workers = workers.max(1);
        let (tx, rx) = mpsc::sync_channel(queue.max(1));
        let rx = Arc::new(Mutex::new(rx));
        let mut handles = Vec::with_capacity(workers);
        for index in 0..workers {
            let rx = rx.clone();
            let handle = thread::Builder::new()
                .name(format!("svcd-call-{index}"))
                .stack_size(threads::io_stack())
                .spawn(move || loop {
                    let job = {
                        let receiver = rx.lock().unwrap_or_else(|error| error.into_inner());
                        receiver.recv()
                    };
                    match job {
                        Ok(job) => {
                            let _ = catch_unwind(AssertUnwindSafe(job));
                        }
                        Err(_) => break,
                    }
                })
                .expect("spawn call worker");
            handles.push(handle);
        }
        WorkerPool {
            tx: Some(tx),
            workers: handles,
        }
    }

    /// Queue one blocking call. Returns the job back when the queue is full so
    /// the caller can answer \`Busy\` without dropping the request silently.
    pub(crate) fn execute(&self, job: Job) -> Result<(), Job> {
        let Some(tx) = &self.tx else {
            return Err(job);
        };
        match tx.try_send(job) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(job)) | Err(TrySendError::Disconnected(job)) => Err(job),
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        self.tx.take();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn workers_run_queued_jobs() {
        let pool = WorkerPool::new(2, 8);
        let done = Arc::new(AtomicUsize::new(0));
        for _ in 0..6 {
            let done = done.clone();
            pool.execute(Box::new(move || {
                done.fetch_add(1, Ordering::SeqCst);
            }))
            .unwrap_or_else(|_| panic!("queue must accept a job"));
        }
        drop(pool);
        assert_eq!(done.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn a_full_queue_returns_the_job() {
        let pool = WorkerPool::new(1, 1);
        let (release_tx, release_rx) = mpsc::channel();
        pool.execute(Box::new(move || {
            let _ = release_rx.recv();
        }))
        .unwrap_or_else(|_| panic!("first job"));
        assert!(pool.execute(Box::new(|| {})).is_err());
        let _ = release_tx.send(());
    }
}
