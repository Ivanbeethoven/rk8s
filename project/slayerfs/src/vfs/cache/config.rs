use std::path::PathBuf;

/// Write-back mode controls when data becomes globally visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteBackMode {
    /// Default safe mode: upload to S3 first, then metadata commit.
    /// fsync/close success guarantees data is in object store + metadata committed.
    UploadBeforeCommit,
    /// High-performance mode: metadata commit first, async upload.
    /// Risk: local SSD loss before upload = data loss. Other clients may
    /// see metadata-visible slices whose objects don't exist yet.
    /// Must be explicitly opted in.
    CommitBeforeUpload,
}

impl Default for WriteBackMode {
    fn default() -> Self {
        Self::UploadBeforeCommit
    }
}

/// Configuration for the SlayerFS local cache system.
///
/// Controls memory and SSD budgets for both read (clean block) and write
/// (dirty slice) caches, as well as prefetch and upload parameters.
#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub cache_root: PathBuf,

    // Read cache budgets
    pub read_memory_bytes: u64,
    pub read_ssd_bytes: u64,

    // Write cache budgets
    pub write_memory_bytes: u64,
    pub write_ssd_bytes: u64,

    // Dirty slice parameters
    pub dirty_slice_target_size: u64,
    pub dirty_slice_max_age_ms: u64,

    // Upload parameters
    pub upload_concurrency: usize,

    // Prefetch parameters
    pub prefetch_enabled: bool,
    pub prefetch_initial_bytes: u64,
    pub prefetch_max_bytes: u64,
    pub prefetch_concurrency: usize,

    // Semantics
    pub strict_posix: bool,
    pub writeback_mode: WriteBackMode,

    // Disk safety
    pub min_free_disk_bytes: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            cache_root: dirs::cache_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join("slayerfs"),
            read_memory_bytes: 512 * 1024 * 1024,
            read_ssd_bytes: 20 * 1024 * 1024 * 1024,
            write_memory_bytes: 512 * 1024 * 1024,
            write_ssd_bytes: 20 * 1024 * 1024 * 1024,
            dirty_slice_target_size: 8 * 1024 * 1024,
            dirty_slice_max_age_ms: 500,
            upload_concurrency: 32,
            prefetch_enabled: true,
            prefetch_initial_bytes: 1024 * 1024,
            prefetch_max_bytes: 64 * 1024 * 1024,
            prefetch_concurrency: 16,
            strict_posix: true,
            writeback_mode: WriteBackMode::UploadBeforeCommit,
            min_free_disk_bytes: 1024 * 1024 * 1024,
        }
    }
}
