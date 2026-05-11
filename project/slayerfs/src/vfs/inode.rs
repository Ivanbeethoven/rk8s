use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::watch;

/// The `Inode`, which holds file attribute state, as a local cache.
/// Slayerfs ensures `close-to-open` semantics; each `open` must see the newest file state.
#[derive(Clone)]
pub(crate) struct Inode {
    ino: i64,
    length_rx: watch::Receiver<u64>,
    length_tx: watch::Sender<u64>,
    /// Bytes actually committed (uploaded + metadata written) for this inode.
    /// Used to compute `st_blocks` correctly for sparse files: blocks are derived
    /// from `committed_bytes`, not from the logical file size.
    ///
    /// Initialised to the file size at open time (assuming pre-existing data is
    /// fully committed). Reset to zero on truncate. Incremented by each
    /// successful `commit_chunk`.
    committed_bytes: Arc<AtomicU64>,
}

impl Inode {
    pub fn new(ino: i64, size: u64) -> Arc<Inode> {
        let (tx, rx) = watch::channel(size);

        Arc::new(Self {
            ino,
            length_rx: rx,
            length_tx: tx,
            // Conservative initialisation: assume all current bytes are committed.
            // This is correct for newly-created files (size=0) and gives a reasonable
            // approximation for files opened for the first time (no local write state yet).
            committed_bytes: Arc::new(AtomicU64::new(size)),
        })
    }

    pub fn ino(&self) -> i64 {
        self.ino
    }

    pub fn file_size(&self) -> u64 {
        *self.length_rx.borrow()
    }

    pub fn update_size(&self, new_size: u64) {
        self.length_tx
            .send(new_size)
            .expect("Inode invariant violated: all receivers dropped in update_size");
    }

    /// Actual committed bytes for this inode (used for `st_blocks`).
    pub fn committed_bytes(&self) -> u64 {
        self.committed_bytes.load(Ordering::Relaxed)
    }

    /// Record that `n` additional bytes have been successfully committed to the
    /// metadata layer.  Called from `commit_chunk` after the slice metadata write
    /// succeeds.
    pub fn add_committed_bytes(&self, n: u64) {
        self.committed_bytes.fetch_add(n, Ordering::Relaxed);
    }

    /// Reset the committed-bytes counter after a truncate.  The new value is the
    /// target size: 0 for a full truncate-to-zero, or the retained portion for a
    /// partial shrink / extend.
    pub fn reset_committed_bytes(&self, size: u64) {
        self.committed_bytes.store(size, Ordering::Relaxed);
    }
}
