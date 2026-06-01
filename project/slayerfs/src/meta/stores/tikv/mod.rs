//! TiKV-backed metadata store.
//!
//! The store uses TiKV's transactional API for namespace and file-layout
//! metadata. Read-only operations use optimistic transactions. Mutating paths
//! use pessimistic transactions and `get_for_update` on the keys that
//! participate in the metadata CAS.

use crate::chunk::SliceDesc;
use crate::meta::INODE_ID_KEY;
use crate::meta::config::{Config, DatabaseType, default_tikv_namespace};
use crate::meta::store::{
    DirEntry, FileAttr, FileType, MetaError, MetaStore, MetaStoreCapabilities, RetryReason,
};
use async_trait::async_trait;
use chrono::Utc;
use rand::{RngCore, rng};
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::fmt;
use std::future::Future;
use std::ops::Bound;
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;
use tikv_client::{BoundRange, Key, KvPair, Transaction, TransactionClient};

const ROOT_INODE: i64 = 1;
const ROOT_SIZE: u64 = 4096;
const FIRST_ALLOCATED_INODE: i64 = 2;
const SCAN_BATCH_LIMIT: u32 = 1024;
const TXN_MAX_RETRIES: usize = 10;

type TiKvTxnFuture<'txn, T> = Pin<Box<dyn Future<Output = Result<T, MetaError>> + Send + 'txn>>;

#[derive(Clone, Copy)]
enum TiKvTxnMode {
    Read,
    Write,
}

/// TiKV metadata backend.
#[derive(Clone)]
pub struct TiKvMetaStore {
    pd_endpoints: Vec<String>,
    namespace: String,
    client: TransactionClient,
    _config: Config,
}

impl fmt::Debug for TiKvMetaStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TiKvMetaStore")
            .field("pd_endpoints", &self.pd_endpoints)
            .field("namespace", &self.namespace)
            .finish_non_exhaustive()
    }
}

impl TiKvMetaStore {
    async fn from_config_inner(config: Config) -> Result<Self, MetaError> {
        let (pd_endpoints, namespace) = match &config.database.db_config {
            DatabaseType::TiKv {
                pd_endpoints,
                namespace,
            } => (pd_endpoints.clone(), normalize_namespace(namespace)),
            _ => {
                return Err(MetaError::Config(
                    "TiKvMetaStore requires database.type = tikv".to_string(),
                ));
            }
        };

        if pd_endpoints.is_empty() {
            return Err(MetaError::Config(
                "TiKvMetaStore requires at least one PD endpoint".to_string(),
            ));
        }

        let client = TransactionClient::new(pd_endpoints.clone())
            .await
            .map_err(|e| Self::tikv_err("connect", e))?;

        Ok(Self {
            pd_endpoints,
            namespace,
            client,
            _config: config,
        })
    }

    /// Create or open the store from a backend path containing `slayerfs.yml`.
    #[allow(dead_code)]
    pub async fn new(backend_path: &Path) -> Result<Self, MetaError> {
        let config =
            Config::from_path(backend_path).map_err(|e| MetaError::Config(e.to_string()))?;
        Self::from_config_inner(config).await
    }

    /// Build a TiKV metadata store from an already parsed configuration.
    #[allow(dead_code)]
    pub async fn from_config(config: Config) -> Result<Self, MetaError> {
        Self::from_config_inner(config).await
    }

    #[allow(dead_code)]
    pub fn pd_endpoints(&self) -> &[String] {
        &self.pd_endpoints
    }

    #[allow(dead_code)]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    #[allow(dead_code)]
    pub(crate) fn inode_key(&self, ino: i64) -> Vec<u8> {
        self.key_bytes(&format!("inode/{ino}"))
    }

    pub(crate) fn dentry_key(&self, parent: i64, name: &str) -> Vec<u8> {
        self.key_bytes(&format!("dentry/{parent}/{name}"))
    }

    fn dentry_prefix(&self, parent: i64) -> Vec<u8> {
        self.key_bytes(&format!("dentry/{parent}/"))
    }

    pub(crate) fn chunk_key(&self, chunk_id: u64) -> Vec<u8> {
        self.key_bytes(&format!("chunk/{chunk_id}"))
    }

    fn chunk_prefix(&self) -> Vec<u8> {
        self.key_bytes("chunk/")
    }

    pub(crate) fn counter_key(&self, name: &str) -> Vec<u8> {
        self.key_bytes(&format!("counter/{name}"))
    }

    fn key_bytes(&self, suffix: &str) -> Vec<u8> {
        Self::scoped_key(&self.namespace, suffix)
    }

    pub(crate) fn scoped_key(namespace: &str, suffix: &str) -> Vec<u8> {
        format!("{}/{}", namespace, suffix).into_bytes()
    }

    fn unsupported(&self, operation: &str) -> MetaError {
        MetaError::NotSupported(format!(
            "TiKV metadata backend operation `{operation}` is not implemented yet"
        ))
    }

    fn tikv_err(operation: &str, error: tikv_client::Error) -> MetaError {
        let message = error.to_string();
        let lower = message.to_ascii_lowercase();
        if lower.contains("write conflict")
            || lower.contains("pessimisticlock")
            || lower.contains("lock conflict")
            || lower.contains("txnlock")
        {
            MetaError::ContinueRetry(RetryReason::TransactionConflict)
        } else {
            MetaError::Internal(format!("TiKV {operation} failed: {message}"))
        }
    }

    async fn begin_read(&self, operation: &str) -> Result<Transaction, MetaError> {
        self.client
            .begin_optimistic()
            .await
            .map_err(|e| Self::tikv_err(operation, e))
    }

    async fn begin_write(&self, operation: &str) -> Result<Transaction, MetaError> {
        self.client
            .begin_pessimistic()
            .await
            .map_err(|e| Self::tikv_err(operation, e))
    }

    async fn commit_write(&self, txn: &mut Transaction, operation: &str) -> Result<(), MetaError> {
        txn.commit()
            .await
            .map(|_| ())
            .map_err(|e| Self::tikv_err(operation, e))
    }

    async fn rollback_best_effort(txn: &mut Transaction, operation: &str) {
        if let Err(err) = txn.rollback().await {
            log::debug!("TiKV {operation} rollback failed: {err}");
        }
    }

    async fn retry_delay(attempt: usize) {
        let jitter_bound = ((attempt + 1) * (attempt + 1)).max(1) as u64;
        let jitter = rng().next_u64() % jitter_bound;
        tokio::time::sleep(Duration::from_millis(20 + jitter)).await;
    }

    /// Run a retry-safe TiKV transaction closure.
    ///
    /// The closure may execute more than once when TiKV reports a retryable
    /// conflict, so it should only perform TiKV reads/writes and in-memory
    /// staging. External side effects belong after this helper returns.
    async fn run_txn<T, F>(
        &self,
        operation: &'static str,
        mode: TiKvTxnMode,
        mut task: F,
    ) -> Result<T, MetaError>
    where
        F: for<'txn> FnMut(&'txn TiKvMetaStore, &'txn mut Transaction) -> TiKvTxnFuture<'txn, T>,
    {
        for attempt in 0..TXN_MAX_RETRIES {
            let mut txn = match mode {
                TiKvTxnMode::Read => self.begin_read(operation).await?,
                TiKvTxnMode::Write => self.begin_write(operation).await?,
            };

            let result = task(self, &mut txn).await;
            match (mode, result) {
                (TiKvTxnMode::Read, Ok(value)) => {
                    Self::rollback_best_effort(&mut txn, operation).await;
                    return Ok(value);
                }
                (TiKvTxnMode::Read, Err(MetaError::ContinueRetry(_))) => {
                    Self::rollback_best_effort(&mut txn, operation).await;
                }
                (TiKvTxnMode::Read, Err(err)) => {
                    Self::rollback_best_effort(&mut txn, operation).await;
                    return Err(err);
                }
                (TiKvTxnMode::Write, Ok(value)) => {
                    match self.commit_write(&mut txn, operation).await {
                        Ok(()) => return Ok(value),
                        Err(MetaError::ContinueRetry(_)) => {}
                        Err(err) => return Err(err),
                    }
                }
                (TiKvTxnMode::Write, Err(MetaError::ContinueRetry(_))) => {
                    Self::rollback_best_effort(&mut txn, operation).await;
                }
                (TiKvTxnMode::Write, Err(err)) => {
                    Self::rollback_best_effort(&mut txn, operation).await;
                    return Err(err);
                }
            }

            if attempt + 1 < TXN_MAX_RETRIES {
                Self::retry_delay(attempt).await;
            }
        }

        Err(MetaError::MaxRetriesExceeded)
    }

    async fn read_txn<T, F>(&self, operation: &'static str, task: F) -> Result<T, MetaError>
    where
        F: for<'txn> FnMut(&'txn TiKvMetaStore, &'txn mut Transaction) -> TiKvTxnFuture<'txn, T>,
    {
        self.run_txn(operation, TiKvTxnMode::Read, task).await
    }

    async fn write_txn<T, F>(&self, operation: &'static str, task: F) -> Result<T, MetaError>
    where
        F: for<'txn> FnMut(&'txn TiKvMetaStore, &'txn mut Transaction) -> TiKvTxnFuture<'txn, T>,
    {
        self.run_txn(operation, TiKvTxnMode::Write, task).await
    }

    fn now() -> i64 {
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    }

    fn root_node() -> StoredNode {
        let now = Self::now();
        StoredNode {
            ino: ROOT_INODE,
            parent: ROOT_INODE,
            name: "/".to_string(),
            kind: StoredNodeKind::Dir,
            size: ROOT_SIZE,
            blocks: ROOT_SIZE.div_ceil(512),
            mode: 0o40755,
            uid: 0,
            gid: 0,
            atime: now,
            mtime: now,
            ctime: now,
            nlink: 2,
        }
    }

    fn decode_node(bytes: &[u8]) -> Result<StoredNode, MetaError> {
        serde_json::from_slice(bytes)
            .map_err(|e| MetaError::Serialization(format!("TiKV node decode failed: {e}")))
    }

    fn decode_dentry(bytes: &[u8]) -> Result<StoredDentry, MetaError> {
        serde_json::from_slice(bytes)
            .map_err(|e| MetaError::Serialization(format!("TiKV dentry decode failed: {e}")))
    }

    fn decode_slices(bytes: &[u8]) -> Result<Vec<SliceDesc>, MetaError> {
        serde_json::from_slice(bytes)
            .map_err(|e| MetaError::Serialization(format!("TiKV slice list decode failed: {e}")))
    }

    fn decode_counter(bytes: &[u8]) -> Result<i64, MetaError> {
        serde_json::from_slice(bytes)
            .map_err(|e| MetaError::Serialization(format!("TiKV counter decode failed: {e}")))
    }

    fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, MetaError> {
        serde_json::to_vec(value)
            .map_err(|e| MetaError::Serialization(format!("TiKV value encode failed: {e}")))
    }

    async fn txn_get_raw(
        txn: &mut Transaction,
        key: Vec<u8>,
        lock: bool,
        operation: &str,
    ) -> Result<Option<Vec<u8>>, MetaError> {
        let result = if lock {
            txn.get_for_update(key).await
        } else {
            txn.get(key).await
        };
        result.map_err(|e| Self::tikv_err(operation, e))
    }

    async fn txn_put_raw(
        txn: &mut Transaction,
        key: Vec<u8>,
        value: Vec<u8>,
        operation: &str,
    ) -> Result<(), MetaError> {
        txn.put(key, value)
            .await
            .map_err(|e| Self::tikv_err(operation, e))
    }

    async fn txn_delete_raw(
        txn: &mut Transaction,
        key: Vec<u8>,
        operation: &str,
    ) -> Result<(), MetaError> {
        txn.delete(key)
            .await
            .map_err(|e| Self::tikv_err(operation, e))
    }

    async fn txn_scan_prefix(
        txn: &mut Transaction,
        prefix: Vec<u8>,
        limit: Option<usize>,
        operation: &str,
    ) -> Result<Vec<KvPair>, MetaError> {
        let mut out = Vec::new();
        let mut remaining = limit.unwrap_or(usize::MAX);
        if remaining == 0 {
            return Ok(out);
        }

        let upper = match prefix_range_end(&prefix) {
            Some(end) => Bound::Excluded(Key::from(end)),
            None => Bound::Unbounded,
        };
        let mut lower = Bound::Included(Key::from(prefix));

        while remaining > 0 {
            let batch_limit = remaining.min(SCAN_BATCH_LIMIT as usize) as u32;
            let range = BoundRange::new(lower.clone(), upper.clone());
            let batch: Vec<KvPair> = txn
                .scan(range, batch_limit)
                .await
                .map_err(|e| Self::tikv_err(operation, e))?
                .collect();

            let batch_len = batch.len();
            if batch_len == 0 {
                break;
            }

            for pair in batch {
                let last_key: Vec<u8> = pair.key().clone().into();
                lower = Bound::Excluded(Key::from(last_key));
                out.push(pair);
            }

            remaining = remaining.saturating_sub(batch_len);
            if batch_len < batch_limit as usize {
                break;
            }
        }

        Ok(out)
    }

    async fn txn_get_node(
        &self,
        txn: &mut Transaction,
        ino: i64,
        lock: bool,
        operation: &str,
    ) -> Result<Option<StoredNode>, MetaError> {
        Self::txn_get_raw(txn, self.inode_key(ino), lock, operation)
            .await?
            .as_deref()
            .map(Self::decode_node)
            .transpose()
    }

    async fn txn_put_node(
        &self,
        txn: &mut Transaction,
        node: &StoredNode,
        operation: &str,
    ) -> Result<(), MetaError> {
        Self::txn_put_raw(
            txn,
            self.inode_key(node.ino),
            Self::encode(node)?,
            operation,
        )
        .await
    }

    async fn txn_get_dentry(
        &self,
        txn: &mut Transaction,
        parent: i64,
        name: &str,
        lock: bool,
        operation: &str,
    ) -> Result<Option<StoredDentry>, MetaError> {
        Self::txn_get_raw(txn, self.dentry_key(parent, name), lock, operation)
            .await?
            .as_deref()
            .map(Self::decode_dentry)
            .transpose()
    }

    async fn txn_put_dentry(
        &self,
        txn: &mut Transaction,
        parent: i64,
        name: &str,
        dentry: &StoredDentry,
        operation: &str,
    ) -> Result<(), MetaError> {
        Self::txn_put_raw(
            txn,
            self.dentry_key(parent, name),
            Self::encode(dentry)?,
            operation,
        )
        .await
    }

    async fn txn_require_dir(
        &self,
        txn: &mut Transaction,
        ino: i64,
        lock: bool,
        operation: &str,
    ) -> Result<StoredNode, MetaError> {
        let node = self
            .txn_get_node(txn, ino, lock, operation)
            .await?
            .ok_or(MetaError::ParentNotFound(ino))?;
        if node.kind != StoredNodeKind::Dir {
            return Err(MetaError::NotDirectory(ino));
        }
        Ok(node)
    }

    async fn txn_next_counter(
        &self,
        txn: &mut Transaction,
        name: &str,
        first_value: i64,
        operation: &str,
    ) -> Result<i64, MetaError> {
        let key = self.counter_key(name);
        let current = Self::txn_get_raw(txn, key.clone(), true, operation)
            .await?
            .as_deref()
            .map(Self::decode_counter)
            .transpose()?
            .unwrap_or(first_value);
        let next = current
            .checked_add(1)
            .ok_or_else(|| MetaError::Internal(format!("TiKV counter overflow: {name}")))?;
        Self::txn_put_raw(txn, key, Self::encode(&next)?, operation).await?;
        Ok(current)
    }

    async fn txn_next_inode(
        &self,
        txn: &mut Transaction,
        operation: &str,
    ) -> Result<i64, MetaError> {
        self.txn_next_counter(txn, INODE_ID_KEY, FIRST_ALLOCATED_INODE, operation)
            .await
    }

    async fn txn_create_node(
        &self,
        txn: &mut Transaction,
        parent: i64,
        name: String,
        kind: StoredNodeKind,
        operation: &str,
    ) -> Result<i64, MetaError> {
        let mut parent_node = self.txn_require_dir(txn, parent, true, operation).await?;
        if self
            .txn_get_dentry(txn, parent, &name, true, operation)
            .await?
            .is_some()
        {
            return Err(MetaError::AlreadyExists { parent, name });
        }

        let ino = self.txn_next_inode(txn, operation).await?;
        let now = Self::now();
        let (size, blocks, mode, nlink) = match kind {
            StoredNodeKind::File => (0, 0, 0o100644, 1),
            StoredNodeKind::Dir => (ROOT_SIZE, ROOT_SIZE.div_ceil(512), 0o40755, 2),
            StoredNodeKind::Symlink => (0, 0, 0o120777, 1),
        };
        let node = StoredNode {
            ino,
            parent,
            name: name.clone(),
            kind,
            size,
            blocks,
            mode,
            uid: 0,
            gid: 0,
            atime: now,
            mtime: now,
            ctime: now,
            nlink,
        };
        let dentry = StoredDentry { ino, kind };

        if kind == StoredNodeKind::Dir {
            parent_node.nlink = parent_node.nlink.saturating_add(1);
        }
        parent_node.mtime = now;
        parent_node.ctime = now;

        self.txn_put_node(txn, &parent_node, operation).await?;
        self.txn_put_node(txn, &node, operation).await?;
        self.txn_put_dentry(txn, parent, &name, &dentry, operation)
            .await?;
        Ok(ino)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum StoredNodeKind {
    File,
    Dir,
    Symlink,
}

impl From<StoredNodeKind> for FileType {
    fn from(kind: StoredNodeKind) -> Self {
        match kind {
            StoredNodeKind::File => FileType::File,
            StoredNodeKind::Dir => FileType::Dir,
            StoredNodeKind::Symlink => FileType::Symlink,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct StoredDentry {
    ino: i64,
    kind: StoredNodeKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredNode {
    ino: i64,
    parent: i64,
    name: String,
    kind: StoredNodeKind,
    size: u64,
    blocks: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    atime: i64,
    mtime: i64,
    ctime: i64,
    nlink: u32,
}

impl StoredNode {
    fn to_attr(&self) -> FileAttr {
        FileAttr {
            ino: self.ino,
            size: self.size,
            blocks: self.blocks,
            kind: self.kind.into(),
            mode: self.mode,
            uid: self.uid,
            gid: self.gid,
            atime: self.atime,
            mtime: self.mtime,
            ctime: self.ctime,
            nlink: self.nlink,
        }
    }
}

#[async_trait]
impl MetaStore for TiKvMetaStore {
    fn name(&self) -> &'static str {
        "tikv"
    }

    fn capabilities(&self) -> MetaStoreCapabilities {
        MetaStoreCapabilities {
            namespace: true,
            file_data: true,
            stat_fs: false,
            ..MetaStoreCapabilities::default()
        }
    }

    async fn from_config(config: Config) -> Result<Self, MetaError> {
        Self::from_config_inner(config).await
    }

    async fn stat(&self, ino: i64) -> Result<Option<FileAttr>, MetaError> {
        let operation = "stat";
        self.read_txn(operation, |store, txn| {
            Box::pin(async move {
                store
                    .txn_get_node(txn, ino, false, operation)
                    .await
                    .map(|node| node.map(|node| node.to_attr()))
            })
        })
        .await
    }

    async fn lookup(&self, parent: i64, name: &str) -> Result<Option<i64>, MetaError> {
        let operation = "lookup";
        let name = name.to_string();
        self.read_txn(operation, |store, txn| {
            let name = name.clone();
            Box::pin(async move {
                store
                    .txn_get_dentry(txn, parent, &name, false, operation)
                    .await
                    .map(|dentry| dentry.map(|dentry| dentry.ino))
            })
        })
        .await
    }

    async fn lookup_path(&self, path: &str) -> Result<Option<(i64, FileType)>, MetaError> {
        if path.is_empty() {
            return Ok(None);
        }
        if path == "/" {
            return Ok(Some((ROOT_INODE, FileType::Dir)));
        }

        let operation = "lookup_path";
        let path = path.to_string();
        self.read_txn(operation, |store, txn| {
            let path = path.clone();
            Box::pin(async move {
                let mut current = ROOT_INODE;
                for segment in path.split('/').filter(|part| !part.is_empty()) {
                    let Some(dentry) = store
                        .txn_get_dentry(txn, current, segment, false, operation)
                        .await?
                    else {
                        return Ok(None);
                    };
                    current = dentry.ino;
                }

                Ok(store
                    .txn_get_node(txn, current, false, operation)
                    .await?
                    .map(|node| (node.ino, node.kind.into())))
            })
        })
        .await
    }

    async fn readdir(&self, ino: i64) -> Result<Vec<DirEntry>, MetaError> {
        let operation = "readdir";
        self.read_txn(operation, |store, txn| {
            Box::pin(async move {
                let node = store
                    .txn_get_node(txn, ino, false, operation)
                    .await?
                    .ok_or(MetaError::NotFound(ino))?;
                if node.kind != StoredNodeKind::Dir {
                    return Err(MetaError::NotDirectory(ino));
                }

                let prefix = store.dentry_prefix(ino);
                let pairs = Self::txn_scan_prefix(txn, prefix.clone(), None, operation).await?;
                let mut out = Vec::with_capacity(pairs.len());
                for pair in pairs {
                    let (key, value): (Key, Vec<u8>) = pair.into();
                    let key: Vec<u8> = key.into();
                    let name = String::from_utf8(key[prefix.len()..].to_vec()).map_err(|e| {
                        MetaError::Serialization(format!("TiKV dentry key is not UTF-8: {e}"))
                    })?;
                    let dentry = Self::decode_dentry(&value)?;
                    out.push(DirEntry {
                        name,
                        ino: dentry.ino,
                        kind: dentry.kind.into(),
                    });
                }
                Ok(out)
            })
        })
        .await
    }

    async fn mkdir(&self, parent: i64, name: String) -> Result<i64, MetaError> {
        let operation = "mkdir";
        self.write_txn(operation, |store, txn| {
            let name = name.clone();
            Box::pin(async move {
                store
                    .txn_create_node(txn, parent, name, StoredNodeKind::Dir, operation)
                    .await
            })
        })
        .await
    }

    async fn rmdir(&self, parent: i64, name: &str) -> Result<(), MetaError> {
        let operation = "rmdir";
        let name = name.to_string();
        self.write_txn(operation, |store, txn| {
            let name = name.clone();
            Box::pin(async move {
                let mut parent_node = store.txn_require_dir(txn, parent, true, operation).await?;
                let dentry = store
                    .txn_get_dentry(txn, parent, &name, true, operation)
                    .await?
                    .ok_or(MetaError::NotFound(parent))?;
                if dentry.kind != StoredNodeKind::Dir {
                    return Err(MetaError::NotDirectory(dentry.ino));
                }
                let dir_node = store
                    .txn_get_node(txn, dentry.ino, true, operation)
                    .await?
                    .ok_or(MetaError::NotFound(dentry.ino))?;
                if dir_node.kind != StoredNodeKind::Dir {
                    return Err(MetaError::NotDirectory(dentry.ino));
                }

                let child_prefix = store.dentry_prefix(dentry.ino);
                if !Self::txn_scan_prefix(txn, child_prefix, Some(1), operation)
                    .await?
                    .is_empty()
                {
                    return Err(MetaError::DirectoryNotEmpty(dentry.ino));
                }

                let now = Self::now();
                parent_node.nlink = parent_node.nlink.saturating_sub(1);
                parent_node.mtime = now;
                parent_node.ctime = now;
                store.txn_put_node(txn, &parent_node, operation).await?;
                Self::txn_delete_raw(txn, store.dentry_key(parent, &name), operation).await?;
                Self::txn_delete_raw(txn, store.inode_key(dentry.ino), operation).await
            })
        })
        .await
    }

    async fn create_file(&self, parent: i64, name: String) -> Result<i64, MetaError> {
        let operation = "create_file";
        self.write_txn(operation, |store, txn| {
            let name = name.clone();
            Box::pin(async move {
                store
                    .txn_create_node(txn, parent, name, StoredNodeKind::File, operation)
                    .await
            })
        })
        .await
    }

    async fn unlink(&self, parent: i64, name: &str) -> Result<(), MetaError> {
        let operation = "unlink";
        let name = name.to_string();
        self.write_txn(operation, |store, txn| {
            let name = name.clone();
            Box::pin(async move {
                let mut parent_node = store.txn_require_dir(txn, parent, true, operation).await?;
                let dentry = store
                    .txn_get_dentry(txn, parent, &name, true, operation)
                    .await?
                    .ok_or(MetaError::NotFound(parent))?;
                if dentry.kind == StoredNodeKind::Dir {
                    return Err(MetaError::NotSupported(
                        "TiKV unlink for directories is not supported; use rmdir".to_string(),
                    ));
                }

                let now = Self::now();
                parent_node.mtime = now;
                parent_node.ctime = now;
                store.txn_put_node(txn, &parent_node, operation).await?;
                Self::txn_delete_raw(txn, store.dentry_key(parent, &name), operation).await?;
                Self::txn_delete_raw(txn, store.inode_key(dentry.ino), operation).await
            })
        })
        .await
    }

    async fn rename(
        &self,
        old_parent: i64,
        old_name: &str,
        new_parent: i64,
        new_name: String,
    ) -> Result<(), MetaError> {
        if old_parent == new_parent && old_name == new_name {
            return Ok(());
        }

        let operation = "rename";
        let old_name = old_name.to_string();
        self.write_txn(operation, |store, txn| {
            let old_name = old_name.clone();
            let new_name = new_name.clone();
            Box::pin(async move {
                let mut old_parent_node = store
                    .txn_require_dir(txn, old_parent, true, operation)
                    .await?;
                let mut new_parent_node = if old_parent == new_parent {
                    old_parent_node.clone()
                } else {
                    store
                        .txn_require_dir(txn, new_parent, true, operation)
                        .await?
                };
                let dentry = store
                    .txn_get_dentry(txn, old_parent, &old_name, true, operation)
                    .await?
                    .ok_or(MetaError::NotFound(old_parent))?;
                if store
                    .txn_get_dentry(txn, new_parent, &new_name, true, operation)
                    .await?
                    .is_some()
                {
                    return Err(MetaError::AlreadyExists {
                        parent: new_parent,
                        name: new_name,
                    });
                }

                let mut node = store
                    .txn_get_node(txn, dentry.ino, true, operation)
                    .await?
                    .ok_or(MetaError::NotFound(dentry.ino))?;
                let now = Self::now();
                node.parent = new_parent;
                node.name = new_name.clone();
                node.ctime = now;
                node.mtime = now;

                old_parent_node.mtime = now;
                old_parent_node.ctime = now;
                new_parent_node.mtime = now;
                new_parent_node.ctime = now;
                if dentry.kind == StoredNodeKind::Dir && old_parent != new_parent {
                    old_parent_node.nlink = old_parent_node.nlink.saturating_sub(1);
                    new_parent_node.nlink = new_parent_node.nlink.saturating_add(1);
                }

                Self::txn_delete_raw(txn, store.dentry_key(old_parent, &old_name), operation)
                    .await?;
                store.txn_put_node(txn, &node, operation).await?;
                if old_parent == new_parent {
                    store.txn_put_node(txn, &old_parent_node, operation).await?;
                } else {
                    store.txn_put_node(txn, &old_parent_node, operation).await?;
                    store.txn_put_node(txn, &new_parent_node, operation).await?;
                }
                store
                    .txn_put_dentry(txn, new_parent, &new_name, &dentry, operation)
                    .await
            })
        })
        .await
    }

    async fn rename_exchange(
        &self,
        old_parent: i64,
        old_name: &str,
        new_parent: i64,
        new_name: &str,
    ) -> Result<(), MetaError> {
        let _ = (old_parent, old_name, new_parent, new_name);
        Err(self.unsupported("rename_exchange"))
    }

    async fn set_file_size(&self, ino: i64, size: u64) -> Result<(), MetaError> {
        let operation = "set_file_size";
        self.write_txn(operation, |store, txn| {
            Box::pin(async move {
                let mut node = store
                    .txn_get_node(txn, ino, true, operation)
                    .await?
                    .ok_or(MetaError::NotFound(ino))?;
                if node.kind != StoredNodeKind::File {
                    return Err(MetaError::NotSupported(
                        "TiKV set_file_size currently supports only regular files".to_string(),
                    ));
                }
                let now = Self::now();
                node.size = size;
                node.blocks = size.div_ceil(512);
                node.mtime = now;
                node.ctime = now;
                store.txn_put_node(txn, &node, operation).await
            })
        })
        .await
    }

    async fn extend_file_size(&self, ino: i64, size: u64) -> Result<(), MetaError> {
        let operation = "extend_file_size";
        self.write_txn(operation, |store, txn| {
            Box::pin(async move {
                let mut node = store
                    .txn_get_node(txn, ino, true, operation)
                    .await?
                    .ok_or(MetaError::NotFound(ino))?;
                if node.kind != StoredNodeKind::File {
                    return Err(MetaError::NotSupported(
                        "TiKV extend_file_size currently supports only regular files".to_string(),
                    ));
                }

                if size > node.size {
                    let now = Self::now();
                    node.size = size;
                    node.blocks = size.div_ceil(512);
                    node.mtime = now;
                    node.ctime = now;
                    store.txn_put_node(txn, &node, operation).await?;
                }
                Ok(())
            })
        })
        .await
    }

    async fn get_names(&self, ino: i64) -> Result<Vec<(Option<i64>, String)>, MetaError> {
        if ino == ROOT_INODE {
            return Ok(vec![(None, "/".to_string())]);
        }

        let operation = "get_names";
        self.read_txn(operation, |store, txn| {
            Box::pin(async move {
                store
                    .txn_get_node(txn, ino, false, operation)
                    .await
                    .map(|node| {
                        node.map(|node| vec![(Some(node.parent), node.name)])
                            .unwrap_or_default()
                    })
            })
        })
        .await
    }

    async fn get_paths(&self, ino: i64) -> Result<Vec<String>, MetaError> {
        if ino == ROOT_INODE {
            return Ok(vec!["/".to_string()]);
        }

        let operation = "get_paths";
        self.read_txn(operation, |store, txn| {
            Box::pin(async move {
                let Some(node) = store.txn_get_node(txn, ino, false, operation).await? else {
                    return Ok(Vec::new());
                };

                let mut parts = vec![node.name];
                let mut current_parent = node.parent;
                while current_parent != ROOT_INODE {
                    let Some(parent) = store
                        .txn_get_node(txn, current_parent, false, operation)
                        .await?
                    else {
                        return Ok(Vec::new());
                    };
                    parts.push(parent.name);
                    current_parent = parent.parent;
                }
                parts.reverse();
                Ok(vec![format!("/{}", parts.join("/"))])
            })
        })
        .await
    }

    fn root_ino(&self) -> i64 {
        ROOT_INODE
    }

    async fn initialize(&self) -> Result<(), MetaError> {
        let operation = "initialize";
        let (root_exists, counter_exists) = self
            .read_txn(operation, |store, txn| {
                Box::pin(async move {
                    let root_exists = store
                        .txn_get_node(txn, ROOT_INODE, false, operation)
                        .await?
                        .is_some();
                    let counter_exists =
                        Self::txn_get_raw(txn, store.counter_key(INODE_ID_KEY), false, operation)
                            .await?
                            .is_some();
                    Ok((root_exists, counter_exists))
                })
            })
            .await?;

        if root_exists && counter_exists {
            return Ok(());
        }

        self.write_txn(operation, |store, txn| {
            Box::pin(async move {
                if store
                    .txn_get_node(txn, ROOT_INODE, true, operation)
                    .await?
                    .is_none()
                {
                    store
                        .txn_put_node(txn, &Self::root_node(), operation)
                        .await?;
                }

                let counter_key = store.counter_key(INODE_ID_KEY);
                if Self::txn_get_raw(txn, counter_key.clone(), true, operation)
                    .await?
                    .is_none()
                {
                    Self::txn_put_raw(
                        txn,
                        counter_key,
                        Self::encode(&FIRST_ALLOCATED_INODE)?,
                        operation,
                    )
                    .await?;
                }
                Ok(())
            })
        })
        .await
    }

    async fn get_deleted_files(&self) -> Result<Vec<i64>, MetaError> {
        Ok(Vec::new())
    }

    async fn remove_file_metadata(&self, ino: i64) -> Result<(), MetaError> {
        let operation = "remove_file_metadata";
        self.write_txn(operation, |store, txn| {
            Box::pin(
                async move { Self::txn_delete_raw(txn, store.inode_key(ino), operation).await },
            )
        })
        .await
    }

    async fn get_slices(&self, chunk_id: u64) -> Result<Vec<SliceDesc>, MetaError> {
        let operation = "get_slices";
        self.read_txn(operation, |store, txn| {
            Box::pin(async move {
                Self::txn_get_raw(txn, store.chunk_key(chunk_id), false, operation)
                    .await?
                    .as_deref()
                    .map(Self::decode_slices)
                    .transpose()
                    .map(|slices| slices.unwrap_or_default())
            })
        })
        .await
    }

    async fn list_chunk_ids(&self, limit: usize) -> Result<Vec<u64>, MetaError> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let operation = "list_chunk_ids";
        self.read_txn(operation, |store, txn| {
            Box::pin(async move {
                let prefix = store.chunk_prefix();
                let pairs =
                    Self::txn_scan_prefix(txn, prefix.clone(), Some(limit), operation).await?;
                let mut out = Vec::with_capacity(pairs.len());
                for pair in pairs {
                    let key: Vec<u8> = pair.into_key().into();
                    let suffix = std::str::from_utf8(&key[prefix.len()..]).map_err(|e| {
                        MetaError::Serialization(format!("TiKV chunk key is not UTF-8: {e}"))
                    })?;
                    if let Ok(chunk_id) = suffix.parse::<u64>() {
                        out.push(chunk_id);
                    }
                }
                Ok(out)
            })
        })
        .await
    }

    async fn append_slice(&self, chunk_id: u64, slice: SliceDesc) -> Result<(), MetaError> {
        let operation = "append_slice";
        self.write_txn(operation, |store, txn| {
            Box::pin(async move {
                let chunk_key = store.chunk_key(chunk_id);
                let mut slices = Self::txn_get_raw(txn, chunk_key.clone(), true, operation)
                    .await?
                    .as_deref()
                    .map(Self::decode_slices)
                    .transpose()?
                    .unwrap_or_default();
                slices.push(slice);
                Self::txn_put_raw(txn, chunk_key, Self::encode(&slices)?, operation).await
            })
        })
        .await
    }

    async fn write(
        &self,
        ino: i64,
        chunk_id: u64,
        slice: SliceDesc,
        new_size: u64,
    ) -> Result<(), MetaError> {
        let operation = "write";
        self.write_txn(operation, |store, txn| {
            Box::pin(async move {
                let mut node = store
                    .txn_get_node(txn, ino, true, operation)
                    .await?
                    .ok_or(MetaError::NotFound(ino))?;
                if node.kind != StoredNodeKind::File {
                    return Err(MetaError::NotSupported(
                        "TiKV write currently supports only regular files".to_string(),
                    ));
                }

                let chunk_key = store.chunk_key(chunk_id);
                let mut slices = Self::txn_get_raw(txn, chunk_key.clone(), true, operation)
                    .await?
                    .as_deref()
                    .map(Self::decode_slices)
                    .transpose()?
                    .unwrap_or_default();
                slices.push(slice);
                Self::txn_put_raw(txn, chunk_key, Self::encode(&slices)?, operation).await?;

                if new_size > node.size {
                    let now = Self::now();
                    node.size = new_size;
                    node.blocks = new_size.div_ceil(512);
                    node.mtime = now;
                    node.ctime = now;
                    store.txn_put_node(txn, &node, operation).await?;
                }
                Ok(())
            })
        })
        .await
    }

    async fn next_id(&self, key: &str) -> Result<i64, MetaError> {
        let operation = "next_id";
        let key = key.to_string();
        self.write_txn(operation, |store, txn| {
            let key = key.clone();
            Box::pin(async move { store.txn_next_counter(txn, &key, 1, operation).await })
        })
        .await
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn normalize_namespace(namespace: &str) -> String {
    let namespace = namespace.trim_matches('/');
    if namespace.is_empty() {
        default_tikv_namespace()
    } else {
        namespace.to_string()
    }
}

fn prefix_range_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    for idx in (0..end.len()).rev() {
        if end[idx] != 0xff {
            end[idx] += 1;
            end.truncate(idx + 1);
            return Some(end);
        }
    }
    None
}

#[cfg(test)]
mod tests;
