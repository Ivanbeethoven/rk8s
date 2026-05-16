use std::path::PathBuf;

use bytes::Bytes;

use super::keys::{DirtySliceKey, DirtySliceState};

/// Record describing a dirty slice persisted to local SSD.
#[derive(Debug, Clone)]
pub struct DirtySliceRecord {
    pub key: DirtySliceKey,
    pub ino: i64,
    pub chunk_id: u64,
    pub chunk_offset: u64,
    pub length: u64,
    pub remote_slice_id: Option<u64>,
    pub state: DirtySliceState,
    pub path: PathBuf,
    pub retry_count: u32,
    pub last_error: Option<String>,
}

/// Trait for a local SSD write-back cache.
///
/// Sealed (frozen) slices are persisted here before upload to the object store.
/// This provides crash recovery and decouples write latency from upload latency.
///
/// Not yet implemented — this is the scaffold for Phase 2.
#[async_trait::async_trait]
pub trait WriteBackCache: Send + Sync {
    /// Persist a sealed slice to local SSD. Returns the local file path.
    async fn persist_slice(
        &self,
        key: DirtySliceKey,
        data: Vec<Bytes>,
    ) -> anyhow::Result<PathBuf>;

    /// Open a persisted slice for reading (used by the uploader).
    async fn open_slice(
        &self,
        key: &DirtySliceKey,
    ) -> anyhow::Result<Box<dyn tokio::io::AsyncRead + Send + Unpin>>;

    /// Update the state of a dirty slice record.
    async fn mark_state(
        &self,
        key: &DirtySliceKey,
        state: DirtySliceState,
    ) -> anyhow::Result<()>;

    /// Recover all non-terminal dirty slice records after a crash.
    async fn recover(&self) -> anyhow::Result<Vec<DirtySliceRecord>>;

    /// Remove a committed or obsolete slice from local storage.
    async fn remove(&self, key: &DirtySliceKey) -> anyhow::Result<()>;
}
