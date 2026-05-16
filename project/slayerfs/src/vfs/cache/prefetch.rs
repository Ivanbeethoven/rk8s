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
///
/// Not yet implemented — this is the scaffold for Phase 2.
#[async_trait::async_trait]
pub trait Prefetcher: Send + Sync {
    /// Submit a prefetch task. May be dropped if the queue is full.
    async fn submit(&self, task: PrefetchTask);

    /// Cancel all pending prefetch tasks for a specific file handle.
    async fn cancel_for_handle(&self, ino: i64, fh: u64);
}
