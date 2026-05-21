use std::collections::HashSet;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::{Semaphore, mpsc};

/// Priority level for prefetch tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PrefetchPriority {
    /// Synchronous demand read — highest priority.
    Demand,
    /// Sequential readahead — medium priority.
    Sequential,
    /// Background warmup — lowest priority.
    Background,
}

/// A prefetch task submitted to the global prefetcher.
#[derive(Debug, Clone)]
pub struct PrefetchTask {
    pub ino: i64,
    pub start: u64,
    pub len: u64,
    pub priority: PrefetchPriority,
    pub owner_fh: u64,
}

/// Trait for a global prefetch scheduler.
///
/// The prefetcher accepts tasks from FileReader sessions and schedules them
/// against the ReadCache, respecting concurrency limits and priorities.
#[async_trait::async_trait]
pub trait Prefetcher: Send + Sync {
    /// Submit a prefetch task. May be dropped if the queue is full.
    async fn submit(&self, task: PrefetchTask);

    /// Cancel all pending prefetch tasks for a specific file handle.
    async fn cancel_for_handle(&self, ino: i64, fh: u64);
}

/// Unique key for deduplicating in-flight prefetch ranges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct RangeKey {
    ino: i64,
    start: u64,
    end: u64,
}

/// Global prefetch scheduler that coordinates readahead across all handles.
///
/// Design:
/// - Bounded task queue (mpsc channel) prevents unbounded memory growth
/// - Semaphore limits concurrent prefetch I/O to avoid saturating the backend
/// - In-flight set deduplicates overlapping ranges
/// - Tasks are processed FIFO (priority is advisory for future scheduling)
pub struct GlobalPrefetcher {
    tx: mpsc::Sender<PrefetchTask>,
    in_flight: Arc<Mutex<HashSet<RangeKey>>>,
    cancelled: Arc<Mutex<HashSet<(i64, u64)>>>,
}

impl GlobalPrefetcher {
    /// Create a new prefetcher with the given concurrency limit and queue depth.
    pub fn new<F, Fut>(concurrency: usize, queue_depth: usize, fetch_fn: F) -> Self
    where
        F: Fn(i64, u64, u64) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let (tx, rx) = mpsc::channel(queue_depth);
        let in_flight = Arc::new(Mutex::new(HashSet::new()));
        let cancelled = Arc::new(Mutex::new(HashSet::<(i64, u64)>::new()));

        let worker_in_flight = in_flight.clone();
        let worker_cancelled = cancelled.clone();
        let sem = Arc::new(Semaphore::new(concurrency));
        let fetch_fn = Arc::new(fetch_fn);

        tokio::spawn(Self::worker_loop(
            rx,
            sem,
            worker_in_flight,
            worker_cancelled,
            fetch_fn,
        ));

        Self {
            tx,
            in_flight,
            cancelled,
        }
    }

    async fn worker_loop<F, Fut>(
        mut rx: mpsc::Receiver<PrefetchTask>,
        sem: Arc<Semaphore>,
        in_flight: Arc<Mutex<HashSet<RangeKey>>>,
        cancelled: Arc<Mutex<HashSet<(i64, u64)>>>,
        fetch_fn: Arc<F>,
    ) where
        F: Fn(i64, u64, u64) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        while let Some(task) = rx.recv().await {
            // Skip if handle was cancelled.
            if cancelled.lock().contains(&(task.ino, task.owner_fh)) {
                continue;
            }

            let key = RangeKey {
                ino: task.ino,
                start: task.start,
                end: task.start + task.len,
            };

            // Deduplicate: skip if already in flight.
            {
                let mut set = in_flight.lock();
                if set.contains(&key) {
                    continue;
                }
                set.insert(key);
            }

            let permit = match sem.clone().acquire_owned().await {
                Ok(p) => p,
                Err(_) => break,
            };

            let in_flight_done = in_flight.clone();
            let fetch = fetch_fn.clone();
            tokio::spawn(async move {
                fetch(task.ino, task.start, task.len).await;
                in_flight_done.lock().remove(&key);
                drop(permit);
            });
        }
    }
}

#[async_trait::async_trait]
impl Prefetcher for GlobalPrefetcher {
    async fn submit(&self, task: PrefetchTask) {
        let _ = self.tx.try_send(task);
    }

    async fn cancel_for_handle(&self, ino: i64, fh: u64) {
        self.cancelled.lock().insert((ino, fh));
    }
}
