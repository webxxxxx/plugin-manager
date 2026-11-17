//! Bounded blocking-job pool for face handlers that cannot run on the server
//! loop.
//!
//! A job is one face request. The pool owns a fixed set of worker threads and a
//! bounded FIFO queue; it never grows the queue and never spawns a temporary
//! thread. At most one job per connection may be queued or running, so a
//! pipelined request cannot pile up behind a slow control-channel round trip.
//!
//! The pool is generic on purpose: it knows a connection key and a job, not a
//! face, an HTTP socket, or a response. The server loop owns the completion
//! slot and the write; the pool only reports how each accepted job ended
//! (`Finished`, `Cancelled`, or `Dropped`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;

use crate::log;

/// Maximum jobs waiting for a worker. The queue never grows past this; a full
/// queue is a visible busy result, not an unbounded allocation.
pub const QUEUE_LIMIT: usize = 64;

/// Why the pool refused a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    /// The bounded queue is full.
    QueueFull,
    /// This connection already owns a queued or running job.
    AlreadyInFlight,
    /// The pool is shutting down and accepts no new work.
    Closed,
}

/// How a job ended. The pool always reports one outcome per accepted job, so
/// the server loop can decide whether to write a response or drop it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobOutcome {
    /// The job ran; the completion slot holds the face result (or is empty if
    /// the handler failed to complete).
    Finished,
    /// The job was running when its connection closed. The result is dropped.
    Cancelled,
    /// The job was still queued when the connection closed or the pool shut
    /// down. It never ran.
    Dropped,
}

/// A boxed pool job. The stream path needs the job back when the queue is
/// full, so the pool accepts it already boxed.
pub type PoolJob = Box<dyn FnOnce(CancelToken) + Send + 'static>;

/// A cancellation token for one job. A disconnect flips it; the job checks it
/// at natural boundaries instead of being killed from another thread.
#[derive(Clone)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    fn new() -> CancelToken {
        CancelToken {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Whether the server already dropped this request's completion.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

/// A fixed-size worker pool with a bounded queue.
pub struct BlockingPool {
    inner: Arc<Inner>,
}

impl BlockingPool {
    /// Start `workers` threads. `queue_limit` bounds waiting jobs;
    /// `finished` runs once for every accepted job (finished, cancelled or
    /// dropped) so the server loop can take or discard the completion slot.
    pub fn start(
        workers: usize,
        queue_limit: usize,
        finished: impl Fn(u64, JobOutcome) + Send + Sync + 'static,
    ) -> io::Result<BlockingPool> {
        let workers = workers.max(1);
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                queue: VecDeque::new(),
                inflight: HashSet::new(),
                running: HashMap::new(),
                closed: false,
            }),
            ready: Condvar::new(),
            queue_limit: queue_limit.max(1),
            finished: Box::new(finished),
        });

        for index in 0..workers {
            let worker = inner.clone();
            let spawned = thread::Builder::new()
                .name(format!("svcd-blocking-{}", index + 1))
                // A blocking job is not just a management call any more: since
                // #121 it can parse a package manifest and tar entries. The
                // transfer floor keeps that work inside the stack budget even
                // when svcd.conf puts request_stack_bytes at its 64 KiB floor.
                .stack_size(crate::threads::transfer_stack())
                .spawn(move || run_worker(worker));
            if let Err(error) = spawned {
                inner.shutdown();
                return Err(error);
            }
        }
        Ok(BlockingPool { inner })
    }

    /// Queue one job for `key`, or hand it back as busy. The caller keeps the
    /// completion slot; the pool only enforces queue and per-connection limits.
    pub fn try_submit(
        &self,
        key: u64,
        job: impl FnOnce(CancelToken) + Send + 'static,
    ) -> Result<(), Busy> {
        self.try_submit_boxed(key, Box::new(job))
            .map_err(|(busy, _job)| busy)
    }

    /// Like `try_submit`, but returns the boxed job when the pool refuses it.
    /// A stream handler that runs out of queue space keeps the job in its
    /// pending slot and retries later, so an upload chunk is never dropped.
    pub fn try_submit_boxed(&self, key: u64, job: PoolJob) -> Result<(), (Busy, PoolJob)> {
        let mut state = self.inner.lock();
        if state.closed {
            return Err((Busy::Closed, job));
        }
        if !state.inflight.insert(key) {
            return Err((Busy::AlreadyInFlight, job));
        }
        if state.queue.len() >= self.inner.queue_limit {
            state.inflight.remove(&key);
            return Err((Busy::QueueFull, job));
        }
        state.queue.push_back(Job {
            key,
            run: job,
            cancel: CancelToken::new(),
        });
        self.inner.ready.notify_one();
        Ok(())
    }

    /// Drop a queued job, or flip a running job's cancellation token. The
    /// server calls this when a connection closes, so a disconnected request
    /// never writes its completion and a queued job does not occupy a slot.
    pub fn cancel(&self, key: u64) {
        let dropped = {
            let mut state = self.inner.lock();
            if !state.inflight.contains(&key) {
                return;
            }
            if let Some(position) = state.queue.iter().position(|job| job.key == key) {
                state.queue.remove(position);
                state.inflight.remove(&key);
                true
            } else {
                if let Some(token) = state.running.get(&key) {
                    token.cancel();
                }
                false
            }
        };
        if dropped {
            (self.inner.finished)(key, JobOutcome::Dropped);
        }
    }

    /// Stop accepting jobs. Queued jobs are dropped; running jobs finish and
    /// report through the normal completion path. Idempotent.
    pub fn shutdown(&self) {
        self.inner.shutdown();
    }
}

impl Drop for BlockingPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Inner {
    state: Mutex<State>,
    ready: Condvar,
    queue_limit: usize,
    finished: Box<dyn Fn(u64, JobOutcome) + Send + Sync>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn shutdown(&self) {
        let dropped: Vec<u64> = {
            let mut state = self.lock();
            state.closed = true;
            let dropped: Vec<u64> = state.queue.drain(..).map(|job| job.key).collect();
            for key in &dropped {
                state.inflight.remove(key);
            }
            dropped
        };
        // Queued jobs never run; report them as dropped. Running jobs finish
        // and report their own outcome from the worker.
        for key in dropped {
            (self.finished)(key, JobOutcome::Dropped);
        }
        self.ready.notify_all();
    }
}

struct State {
    queue: VecDeque<Job>,
    /// Queued plus running keys. A key is removed only after its job finishes
    /// or its queued job is cancelled.
    inflight: HashSet<u64>,
    /// Cancellation tokens for running jobs, so a disconnect can flip them.
    running: HashMap<u64, CancelToken>,
    closed: bool,
}

struct Job {
    key: u64,
    run: Box<dyn FnOnce(CancelToken) + Send + 'static>,
    cancel: CancelToken,
}

fn run_worker(inner: Arc<Inner>) {
    loop {
        let job = {
            let mut state = inner.lock();
            loop {
                if state.closed {
                    return;
                }
                if let Some(job) = state.queue.pop_front() {
                    state.running.insert(job.key, job.cancel.clone());
                    break job;
                }
                state = inner
                    .ready
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner());
            }
        };

        let key = job.key;
        let token = job.cancel.clone();
        let outcome = if token.is_cancelled() {
            Ok(())
        } else {
            // A face handler panic must not shrink the pool. In release builds
            // the crate aborts on panic anyway; in test/debug builds this keeps
            // the remaining workers alive and frees the connection key.
            catch_unwind(AssertUnwindSafe(|| (job.run)(token.clone())))
        };

        let cancelled = {
            let mut state = inner.lock();
            state.inflight.remove(&key);
            state.running.remove(&key);
            token.is_cancelled()
        };

        if outcome.is_err() {
            log::svcd(&format!("svcd: blocking job {} panicked", key));
        }
        let finished = &inner.finished;
        let outcome = if cancelled {
            JobOutcome::Cancelled
        } else {
            JobOutcome::Finished
        };
        let _ = catch_unwind(AssertUnwindSafe(|| finished(key, outcome)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::time::Duration;

    struct Gate {
        open: Mutex<bool>,
        changed: Condvar,
    }

    impl Gate {
        fn new() -> Arc<Gate> {
            Arc::new(Gate {
                open: Mutex::new(false),
                changed: Condvar::new(),
            })
        }

        fn wait(&self) {
            let mut open = self.open.lock().unwrap_or_else(|error| error.into_inner());
            while !*open {
                open = self
                    .changed
                    .wait(open)
                    .unwrap_or_else(|error| error.into_inner());
            }
        }

        fn release(&self) {
            *self.open.lock().unwrap_or_else(|error| error.into_inner()) = true;
            self.changed.notify_all();
        }
    }

    fn finished_channel() -> (Sender<(u64, JobOutcome)>, Receiver<(u64, JobOutcome)>) {
        mpsc::channel()
    }

    fn recv(receiver: &Receiver<(u64, JobOutcome)>) -> (u64, JobOutcome) {
        receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("a finished job")
    }

    #[test]
    fn a_full_queue_is_busy_instead_of_growing() {
        let (finished, finished_rx) = finished_channel();
        let pool = BlockingPool::start(1, 1, move |key, outcome| {
            let _ = finished.send((key, outcome));
        })
        .expect("pool");

        let started = Gate::new();
        let release = Gate::new();
        let started_by_job = started.clone();
        let release_job = release.clone();
        pool.try_submit(1, move |_| {
            started_by_job.release();
            release_job.wait();
        })
        .expect("the worker takes the first job");
        started.wait();

        pool.try_submit(2, |_| {}).expect("the queue has one slot");
        assert_eq!(pool.try_submit(3, |_| {}), Err(Busy::QueueFull));

        release.release();
        assert_eq!(recv(&finished_rx), (1, JobOutcome::Finished));
        assert_eq!(recv(&finished_rx), (2, JobOutcome::Finished));
    }

    #[test]
    fn one_job_per_connection_is_busy_while_queued_or_running() {
        let (finished, finished_rx) = finished_channel();
        let pool = BlockingPool::start(1, 4, move |key, outcome| {
            let _ = finished.send((key, outcome));
        })
        .expect("pool");

        let started = Gate::new();
        let release = Gate::new();
        let started_by_job = started.clone();
        let release_job = release.clone();
        pool.try_submit(7, move |_| {
            started_by_job.release();
            release_job.wait();
        })
        .expect("the worker takes the first job");
        started.wait();

        assert_eq!(pool.try_submit(7, |_| {}), Err(Busy::AlreadyInFlight));

        pool.try_submit(8, |_| {}).expect("queued");
        assert_eq!(
            pool.try_submit(8, |_| {}),
            Err(Busy::AlreadyInFlight),
            "a queued job also owns its connection key"
        );

        release.release();
        assert_eq!(recv(&finished_rx), (7, JobOutcome::Finished));
        assert_eq!(recv(&finished_rx), (8, JobOutcome::Finished));

        pool.try_submit(7, |_| {}).expect("the key is reusable");
        assert_eq!(recv(&finished_rx), (7, JobOutcome::Finished));
    }

    #[test]
    fn a_disconnect_drops_a_queued_job_and_reports_it_dropped() {
        let (finished, finished_rx) = finished_channel();
        let pool = BlockingPool::start(1, 4, move |key, outcome| {
            let _ = finished.send((key, outcome));
        })
        .expect("pool");

        let started = Gate::new();
        let release = Gate::new();
        let started_by_job = started.clone();
        let release_job = release.clone();
        pool.try_submit(1, move |_| {
            started_by_job.release();
            release_job.wait();
        })
        .expect("the worker takes the first job");
        started.wait();

        let (ran, ran_rx) = mpsc::channel();
        pool.try_submit(2, move |_| {
            let _ = ran.send(());
        })
        .expect("queued");

        pool.cancel(2);
        assert_eq!(
            recv(&finished_rx),
            (2, JobOutcome::Dropped),
            "a queued job is reported dropped immediately"
        );

        release.release();
        assert_eq!(recv(&finished_rx), (1, JobOutcome::Finished));
        assert!(
            ran_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "a queued job for a closed connection must not run"
        );
    }

    #[test]
    fn cancelling_a_running_job_reports_cancelled_and_frees_its_key() {
        let (finished, finished_rx) = finished_channel();
        let pool = BlockingPool::start(1, 4, move |key, outcome| {
            let _ = finished.send((key, outcome));
        })
        .expect("pool");

        let started = Gate::new();
        let release = Gate::new();
        let saw_cancel = Arc::new(Mutex::new(None));
        let started_by_job = started.clone();
        let release_job = release.clone();
        let saw_cancel_by_job = saw_cancel.clone();
        pool.try_submit(3, move |cancel| {
            started_by_job.release();
            release_job.wait();
            *saw_cancel_by_job
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(cancel.is_cancelled());
        })
        .expect("the worker takes the job");
        started.wait();

        pool.cancel(3);
        release.release();
        assert_eq!(
            recv(&finished_rx),
            (3, JobOutcome::Cancelled),
            "a cancelled running job reports its outcome without a completion"
        );
        assert_eq!(
            *saw_cancel.lock().unwrap_or_else(|error| error.into_inner()),
            Some(true),
            "the running job sees the cancellation token"
        );

        pool.try_submit(3, |_| {})
            .expect("the cancelled key is reusable");
        assert_eq!(recv(&finished_rx), (3, JobOutcome::Finished));
    }

    #[test]
    fn a_refused_boxed_job_is_handed_back_to_its_stream() {
        let (finished, _finished_rx) = finished_channel();
        let pool = BlockingPool::start(1, 1, move |key, outcome| {
            let _ = finished.send((key, outcome));
        })
        .expect("pool");

        let started = Gate::new();
        let release = Gate::new();
        let started_by_job = started.clone();
        let release_job = release.clone();
        pool.try_submit(1, move |_| {
            started_by_job.release();
            release_job.wait();
        })
        .expect("the worker takes the first job");
        started.wait();
        pool.try_submit(2, |_| {}).expect("fills the queue");

        let ran = Arc::new(AtomicBool::new(false));
        let ran_by_job = ran.clone();
        let refused = pool.try_submit_boxed(
            3,
            Box::new(move |_| {
                ran_by_job.store(true, Ordering::SeqCst);
            }),
        );
        let (busy, job) = refused.expect_err("the queue is full");
        assert_eq!(busy, Busy::QueueFull);
        drop(job);

        release.release();
        assert!(!ran.load(Ordering::SeqCst), "a refused job must not run");
    }

    #[test]
    fn a_panicking_job_keeps_the_worker_and_reports_finished() {
        let (finished, finished_rx) = finished_channel();
        let pool = BlockingPool::start(1, 2, move |key, outcome| {
            let _ = finished.send((key, outcome));
        })
        .expect("pool");

        pool.try_submit(1, |_| panic!("test job panics"))
            .expect("job");
        assert_eq!(recv(&finished_rx), (1, JobOutcome::Finished));

        pool.try_submit(2, |_| {}).expect("the worker survived");
        assert_eq!(recv(&finished_rx), (2, JobOutcome::Finished));
    }

    #[test]
    fn shutdown_rejects_new_jobs_and_reports_queued_jobs_dropped() {
        let (finished, finished_rx) = finished_channel();
        let pool = BlockingPool::start(1, 2, move |key, outcome| {
            let _ = finished.send((key, outcome));
        })
        .expect("pool");

        let started = Gate::new();
        let release = Gate::new();
        let started_by_job = started.clone();
        let release_job = release.clone();
        pool.try_submit(1, move |_| {
            started_by_job.release();
            release_job.wait();
        })
        .expect("the worker takes the first job");
        started.wait();

        let (ran, ran_rx) = mpsc::channel();
        pool.try_submit(2, move |_| {
            let _ = ran.send(());
        })
        .expect("queued");

        pool.shutdown();
        assert_eq!(pool.try_submit(3, |_| {}), Err(Busy::Closed));
        assert_eq!(
            recv(&finished_rx),
            (2, JobOutcome::Dropped),
            "shutdown fails queued jobs instead of leaving them pending"
        );

        release.release();
        assert_eq!(recv(&finished_rx), (1, JobOutcome::Finished));
        assert!(
            ran_rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "shutdown drops queued jobs"
        );
    }
}
