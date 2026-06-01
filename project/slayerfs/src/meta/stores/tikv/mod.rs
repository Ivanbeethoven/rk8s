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
    DirEntry, FileAttr, FileType, MetaError, MetaStore, MetaStoreCapabilities, OpenFlags,
    RetryReason, SetAttrFlags, SetAttrRequest,
};
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::fmt;
use std::ops::Bound;
use std::path::Path;
use tikv_client::{BoundRange, Key, KvPair, Transaction, TransactionClient};

const ROOT_INODE: i64 = 1;
const ROOT_SIZE: u64 = 4096;
const FIRST_ALLOCATED_INODE: i64 = 2;
const SCAN_BATCH_LIMIT: u32 = 1024;

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

    fn inode_prefix(&self) -> Vec<u8> {
        self.key_bytes("inode/")
    }

    fn link_parent_key(&self, ino: i64) -> Vec<u8> {
        self.key_bytes(&format!("link_parent/{ino}"))
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
            symlink_target: None,
            deleted: false,
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

    fn decode_link_parents(bytes: &[u8]) -> Result<Vec<StoredLinkParent>, MetaError> {
        serde_json::from_slice(bytes)
            .map_err(|e| MetaError::Serialization(format!("TiKV link parent decode failed: {e}")))
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

    async fn txn_get_link_parents(
        &self,
        txn: &mut Transaction,
        ino: i64,
        lock: bool,
        operation: &str,
    ) -> Result<Vec<StoredLinkParent>, MetaError> {
        Self::txn_get_raw(txn, self.link_parent_key(ino), lock, operation)
            .await?
            .as_deref()
            .map(Self::decode_link_parents)
            .transpose()
            .map(|parents| parents.unwrap_or_default())
    }

    async fn txn_put_link_parents(
        &self,
        txn: &mut Transaction,
        ino: i64,
        parents: &[StoredLinkParent],
        operation: &str,
    ) -> Result<(), MetaError> {
        Self::txn_put_raw(
            txn,
            self.link_parent_key(ino),
            Self::encode(&parents)?,
            operation,
        )
        .await
    }

    async fn txn_delete_link_parents(
        &self,
        txn: &mut Transaction,
        ino: i64,
        operation: &str,
    ) -> Result<(), MetaError> {
        Self::txn_delete_raw(txn, self.link_parent_key(ino), operation).await
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
        self.txn_create_node_with_target(txn, parent, name, kind, None, operation)
            .await
    }

    async fn txn_create_node_with_target(
        &self,
        txn: &mut Transaction,
        parent: i64,
        name: String,
        kind: StoredNodeKind,
        symlink_target: Option<String>,
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
        let target_len = symlink_target.as_ref().map(|target| target.len() as u64);
        let (size, blocks, mode, nlink) = match kind {
            StoredNodeKind::File => (0, 0, 0o100644, 1),
            StoredNodeKind::Dir => (ROOT_SIZE, ROOT_SIZE.div_ceil(512), 0o40755, 2),
            StoredNodeKind::Symlink => {
                let size = target_len.unwrap_or(0);
                (size, size.div_ceil(512), 0o120777, 1)
            }
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
            symlink_target,
            deleted: false,
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

    async fn txn_remove_non_dir_dentry(
        &self,
        txn: &mut Transaction,
        parent: i64,
        name: &str,
        dentry: StoredDentry,
        now: i64,
        operation: &str,
    ) -> Result<(), MetaError> {
        if dentry.kind == StoredNodeKind::Dir {
            return Err(MetaError::NotSupported(
                "TiKV unlink for directories is not supported; use rmdir".to_string(),
            ));
        }

        let mut node = self
            .txn_get_node(txn, dentry.ino, true, operation)
            .await?
            .ok_or(MetaError::NotFound(dentry.ino))?;
        if node.kind == StoredNodeKind::Dir {
            return Err(MetaError::NotSupported(
                "TiKV unlink for directories is not supported; use rmdir".to_string(),
            ));
        }
        if node.deleted || node.nlink == 0 {
            return Err(MetaError::NotFound(dentry.ino));
        }

        Self::txn_delete_raw(txn, self.dentry_key(parent, name), operation).await?;

        if node.nlink > 1 {
            let mut link_parents = self
                .txn_get_link_parents(txn, node.ino, true, operation)
                .await?;
            let before = link_parents.len();
            link_parents.retain(|link| !(link.parent == parent && link.name == name));
            if link_parents.len() == before {
                return Err(MetaError::Internal(format!(
                    "expected link parent binding {parent}/{name} for inode {}",
                    node.ino
                )));
            }

            node.nlink -= 1;
            node.deleted = false;
            if node.nlink == 1 {
                let remaining = link_parents.first().cloned().ok_or_else(|| {
                    MetaError::Internal(format!(
                        "missing remaining link parent for inode {}",
                        node.ino
                    ))
                })?;
                node.parent = remaining.parent;
                node.name = remaining.name;
                self.txn_delete_link_parents(txn, node.ino, operation)
                    .await?;
            } else {
                node.parent = 0;
                node.name.clear();
                self.txn_put_link_parents(txn, node.ino, &link_parents, operation)
                    .await?;
            }
        } else {
            node.nlink = 0;
            node.deleted = true;
            node.parent = 0;
            node.name.clear();
            self.txn_delete_link_parents(txn, node.ino, operation)
                .await?;
        }

        node.mtime = now;
        node.ctime = now;
        self.txn_put_node(txn, &node, operation).await
    }

    async fn txn_move_node_binding(
        &self,
        txn: &mut Transaction,
        node: &mut StoredNode,
        old_parent: i64,
        old_name: &str,
        new_parent: i64,
        new_name: &str,
        operation: &str,
    ) -> Result<(), MetaError> {
        if node.kind == StoredNodeKind::Dir || node.nlink <= 1 {
            node.parent = new_parent;
            node.name = new_name.to_string();
            return Ok(());
        }

        let mut link_parents = self
            .txn_get_link_parents(txn, node.ino, true, operation)
            .await?;
        let mut updated = false;
        for link in &mut link_parents {
            if link.parent == old_parent && link.name == old_name {
                link.parent = new_parent;
                link.name = new_name.to_string();
                updated = true;
                break;
            }
        }

        if !updated {
            return Err(MetaError::Internal(format!(
                "expected link parent binding {old_parent}/{old_name} for inode {}",
                node.ino
            )));
        }

        node.parent = 0;
        node.name.clear();
        self.txn_put_link_parents(txn, node.ino, &link_parents, operation)
            .await
    }

    async fn finish_write<T>(
        &self,
        txn: &mut Transaction,
        operation: &str,
        result: Result<T, MetaError>,
    ) -> Result<T, MetaError> {
        match result {
            Ok(value) => {
                if let Err(err) = self.commit_write(txn, operation).await {
                    return Err(err);
                }
                Ok(value)
            }
            Err(err) => {
                Self::rollback_best_effort(txn, operation).await;
                Err(err)
            }
        }
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StoredLinkParent {
    parent: i64,
    name: String,
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
    #[serde(default)]
    symlink_target: Option<String>,
    #[serde(default)]
    deleted: bool,
}

impl StoredNode {
    fn to_attr(&self) -> FileAttr {
        let size = self
            .symlink_target
            .as_ref()
            .map(|target| target.len() as u64)
            .unwrap_or(self.size);
        let blocks = if self.symlink_target.is_some() {
            size.div_ceil(512)
        } else {
            self.blocks
        };
        FileAttr {
            ino: self.ino,
            size,
            blocks,
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
            hardlinks: true,
            symlinks: true,
            rename_exchange: true,
            stat_fs: false,
            ..MetaStoreCapabilities::default()
        }
    }

    async fn from_config(config: Config) -> Result<Self, MetaError> {
        Self::from_config_inner(config).await
    }

    async fn stat(&self, ino: i64) -> Result<Option<FileAttr>, MetaError> {
        let operation = "stat";
        let mut txn = self.begin_read(operation).await?;
        let result = self
            .txn_get_node(&mut txn, ino, false, operation)
            .await
            .map(|node| node.map(|node| node.to_attr()));
        Self::rollback_best_effort(&mut txn, operation).await;
        result
    }

    async fn lookup(&self, parent: i64, name: &str) -> Result<Option<i64>, MetaError> {
        let operation = "lookup";
        let mut txn = self.begin_read(operation).await?;
        let result = self
            .txn_get_dentry(&mut txn, parent, name, false, operation)
            .await
            .map(|dentry| dentry.map(|dentry| dentry.ino));
        Self::rollback_best_effort(&mut txn, operation).await;
        result
    }

    async fn lookup_path(&self, path: &str) -> Result<Option<(i64, FileType)>, MetaError> {
        if path.is_empty() {
            return Ok(None);
        }
        if path == "/" {
            return Ok(Some((ROOT_INODE, FileType::Dir)));
        }

        let operation = "lookup_path";
        let mut txn = self.begin_read(operation).await?;
        let result = async {
            let mut current = ROOT_INODE;
            for segment in path.split('/').filter(|part| !part.is_empty()) {
                let Some(dentry) = self
                    .txn_get_dentry(&mut txn, current, segment, false, operation)
                    .await?
                else {
                    return Ok(None);
                };
                current = dentry.ino;
            }

            Ok(self
                .txn_get_node(&mut txn, current, false, operation)
                .await?
                .map(|node| (node.ino, node.kind.into())))
        }
        .await;
        Self::rollback_best_effort(&mut txn, operation).await;
        result
    }

    async fn readdir(&self, ino: i64) -> Result<Vec<DirEntry>, MetaError> {
        let operation = "readdir";
        let mut txn = self.begin_read(operation).await?;
        let result = async {
            let node = self
                .txn_get_node(&mut txn, ino, false, operation)
                .await?
                .ok_or(MetaError::NotFound(ino))?;
            if node.kind != StoredNodeKind::Dir {
                return Err(MetaError::NotDirectory(ino));
            }

            let prefix = self.dentry_prefix(ino);
            let pairs = Self::txn_scan_prefix(&mut txn, prefix.clone(), None, operation).await?;
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
        }
        .await;
        Self::rollback_best_effort(&mut txn, operation).await;
        result
    }

    async fn mkdir(&self, parent: i64, name: String) -> Result<i64, MetaError> {
        let operation = "mkdir";
        let mut txn = self.begin_write(operation).await?;
        let result = self
            .txn_create_node(&mut txn, parent, name, StoredNodeKind::Dir, operation)
            .await;
        self.finish_write(&mut txn, operation, result).await
    }

    async fn rmdir(&self, parent: i64, name: &str) -> Result<(), MetaError> {
        let operation = "rmdir";
        let mut txn = self.begin_write(operation).await?;
        let result = async {
            let mut parent_node = self
                .txn_require_dir(&mut txn, parent, true, operation)
                .await?;
            let dentry = self
                .txn_get_dentry(&mut txn, parent, name, true, operation)
                .await?
                .ok_or(MetaError::NotFound(parent))?;
            if dentry.kind != StoredNodeKind::Dir {
                return Err(MetaError::NotDirectory(dentry.ino));
            }
            let dir_node = self
                .txn_get_node(&mut txn, dentry.ino, true, operation)
                .await?
                .ok_or(MetaError::NotFound(dentry.ino))?;
            if dir_node.kind != StoredNodeKind::Dir {
                return Err(MetaError::NotDirectory(dentry.ino));
            }

            let child_prefix = self.dentry_prefix(dentry.ino);
            if !Self::txn_scan_prefix(&mut txn, child_prefix, Some(1), operation)
                .await?
                .is_empty()
            {
                return Err(MetaError::DirectoryNotEmpty(dentry.ino));
            }

            let now = Self::now();
            parent_node.nlink = parent_node.nlink.saturating_sub(1);
            parent_node.mtime = now;
            parent_node.ctime = now;
            self.txn_put_node(&mut txn, &parent_node, operation).await?;
            Self::txn_delete_raw(&mut txn, self.dentry_key(parent, name), operation).await?;
            Self::txn_delete_raw(&mut txn, self.inode_key(dentry.ino), operation).await
        }
        .await;
        self.finish_write(&mut txn, operation, result).await
    }

    async fn create_file(&self, parent: i64, name: String) -> Result<i64, MetaError> {
        let operation = "create_file";
        let mut txn = self.begin_write(operation).await?;
        let result = self
            .txn_create_node(&mut txn, parent, name, StoredNodeKind::File, operation)
            .await;
        self.finish_write(&mut txn, operation, result).await
    }

    async fn link(&self, ino: i64, parent: i64, name: &str) -> Result<FileAttr, MetaError> {
        if ino == ROOT_INODE {
            return Err(MetaError::NotSupported(
                "cannot create hard links to the root inode".to_string(),
            ));
        }

        let operation = "link";
        let mut txn = self.begin_write(operation).await?;
        let result = async {
            let mut parent_node = self
                .txn_require_dir(&mut txn, parent, true, operation)
                .await?;
            if self
                .txn_get_dentry(&mut txn, parent, name, true, operation)
                .await?
                .is_some()
            {
                return Err(MetaError::AlreadyExists {
                    parent,
                    name: name.to_string(),
                });
            }

            let mut node = self
                .txn_get_node(&mut txn, ino, true, operation)
                .await?
                .ok_or(MetaError::NotFound(ino))?;
            if node.kind == StoredNodeKind::Dir {
                return Err(MetaError::NotSupported(
                    "cannot create hard links to directories".to_string(),
                ));
            }
            if node.kind == StoredNodeKind::Symlink {
                return Err(MetaError::NotSupported(
                    "cannot create hard links to symbolic links".to_string(),
                ));
            }
            if node.deleted || node.nlink == 0 {
                return Err(MetaError::NotFound(ino));
            }

            let mut link_parents = if node.nlink == 1 {
                vec![StoredLinkParent {
                    parent: node.parent,
                    name: node.name.clone(),
                }]
            } else {
                self.txn_get_link_parents(&mut txn, ino, true, operation)
                    .await?
            };
            link_parents.push(StoredLinkParent {
                parent,
                name: name.to_string(),
            });

            let now = Self::now();
            node.nlink = node.nlink.saturating_add(1);
            node.parent = 0;
            node.name.clear();
            node.deleted = false;
            node.mtime = now;
            node.ctime = now;
            parent_node.mtime = now;
            parent_node.ctime = now;

            self.txn_put_link_parents(&mut txn, ino, &link_parents, operation)
                .await?;
            self.txn_put_node(&mut txn, &node, operation).await?;
            self.txn_put_node(&mut txn, &parent_node, operation).await?;
            self.txn_put_dentry(
                &mut txn,
                parent,
                name,
                &StoredDentry {
                    ino,
                    kind: StoredNodeKind::File,
                },
                operation,
            )
            .await?;

            Ok(node.to_attr())
        }
        .await;
        self.finish_write(&mut txn, operation, result).await
    }

    async fn symlink(
        &self,
        parent: i64,
        name: &str,
        target: &str,
    ) -> Result<(i64, FileAttr), MetaError> {
        let operation = "symlink";
        let mut txn = self.begin_write(operation).await?;
        let result = async {
            let ino = self
                .txn_create_node_with_target(
                    &mut txn,
                    parent,
                    name.to_string(),
                    StoredNodeKind::Symlink,
                    Some(target.to_string()),
                    operation,
                )
                .await?;
            let attr = self
                .txn_get_node(&mut txn, ino, false, operation)
                .await?
                .ok_or(MetaError::NotFound(ino))?
                .to_attr();
            Ok((ino, attr))
        }
        .await;
        self.finish_write(&mut txn, operation, result).await
    }

    async fn read_symlink(&self, ino: i64) -> Result<String, MetaError> {
        let operation = "read_symlink";
        let mut txn = self.begin_read(operation).await?;
        let result = async {
            let node = self
                .txn_get_node(&mut txn, ino, false, operation)
                .await?
                .ok_or(MetaError::NotFound(ino))?;
            if node.kind != StoredNodeKind::Symlink {
                return Err(MetaError::NotSupported(format!(
                    "inode {ino} is not a symbolic link"
                )));
            }
            node.symlink_target.ok_or_else(|| {
                MetaError::Internal(format!("symlink target missing for inode {ino}"))
            })
        }
        .await;
        Self::rollback_best_effort(&mut txn, operation).await;
        result
    }

    async fn unlink(&self, parent: i64, name: &str) -> Result<(), MetaError> {
        let operation = "unlink";
        let mut txn = self.begin_write(operation).await?;
        let result = async {
            let mut parent_node = self
                .txn_require_dir(&mut txn, parent, true, operation)
                .await?;
            let dentry = self
                .txn_get_dentry(&mut txn, parent, name, true, operation)
                .await?
                .ok_or(MetaError::NotFound(parent))?;

            let now = Self::now();
            self.txn_remove_non_dir_dentry(&mut txn, parent, name, dentry, now, operation)
                .await?;
            parent_node.mtime = now;
            parent_node.ctime = now;
            self.txn_put_node(&mut txn, &parent_node, operation).await
        }
        .await;
        self.finish_write(&mut txn, operation, result).await
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
        let mut txn = self.begin_write(operation).await?;
        let result = async {
            let mut old_parent_node = self
                .txn_require_dir(&mut txn, old_parent, true, operation)
                .await?;
            let mut new_parent_node = if old_parent == new_parent {
                old_parent_node.clone()
            } else {
                self.txn_require_dir(&mut txn, new_parent, true, operation)
                    .await?
            };
            let source_dentry = self
                .txn_get_dentry(&mut txn, old_parent, old_name, true, operation)
                .await?
                .ok_or(MetaError::NotFound(old_parent))?;

            let mut source_node = self
                .txn_get_node(&mut txn, source_dentry.ino, true, operation)
                .await?
                .ok_or(MetaError::NotFound(source_dentry.ino))?;
            if source_node.deleted || source_node.nlink == 0 {
                return Err(MetaError::NotFound(source_dentry.ino));
            }

            let destination = self
                .txn_get_dentry(&mut txn, new_parent, &new_name, true, operation)
                .await?;
            let now = Self::now();
            let mut old_parent_nlink_delta = 0;
            let mut new_parent_nlink_delta = 0;

            if let Some(dest_dentry) = destination {
                if dest_dentry.ino == source_dentry.ino {
                    return Ok(());
                }

                let dest_node = self
                    .txn_get_node(&mut txn, dest_dentry.ino, true, operation)
                    .await?
                    .ok_or(MetaError::NotFound(dest_dentry.ino))?;

                match (source_node.kind, dest_node.kind) {
                    (StoredNodeKind::Dir, StoredNodeKind::Dir) => {
                        let child_prefix = self.dentry_prefix(dest_dentry.ino);
                        if !Self::txn_scan_prefix(&mut txn, child_prefix, Some(1), operation)
                            .await?
                            .is_empty()
                        {
                            return Err(MetaError::DirectoryNotEmpty(dest_dentry.ino));
                        }
                        Self::txn_delete_raw(
                            &mut txn,
                            self.dentry_key(new_parent, &new_name),
                            operation,
                        )
                        .await?;
                        Self::txn_delete_raw(&mut txn, self.inode_key(dest_dentry.ino), operation)
                            .await?;
                        self.txn_delete_link_parents(&mut txn, dest_dentry.ino, operation)
                            .await?;
                        new_parent_nlink_delta -= 1;
                    }
                    (StoredNodeKind::Dir, _) => {
                        return Err(MetaError::Io(std::io::Error::from(
                            std::io::ErrorKind::NotADirectory,
                        )));
                    }
                    (_, StoredNodeKind::Dir) => {
                        return Err(MetaError::Io(std::io::Error::from(
                            std::io::ErrorKind::IsADirectory,
                        )));
                    }
                    _ => {
                        self.txn_remove_non_dir_dentry(
                            &mut txn,
                            new_parent,
                            &new_name,
                            dest_dentry,
                            now,
                            operation,
                        )
                        .await?;
                    }
                }
            }

            self.txn_move_node_binding(
                &mut txn,
                &mut source_node,
                old_parent,
                old_name,
                new_parent,
                &new_name,
                operation,
            )
            .await?;
            source_node.ctime = now;
            source_node.mtime = now;

            old_parent_node.mtime = now;
            old_parent_node.ctime = now;
            new_parent_node.mtime = now;
            new_parent_node.ctime = now;
            if source_node.kind == StoredNodeKind::Dir && old_parent != new_parent {
                old_parent_nlink_delta -= 1;
                new_parent_nlink_delta += 1;
            }

            Self::txn_delete_raw(&mut txn, self.dentry_key(old_parent, old_name), operation)
                .await?;
            self.txn_put_node(&mut txn, &source_node, operation).await?;
            if old_parent == new_parent {
                old_parent_node.nlink = apply_nlink_delta(
                    old_parent_node.nlink,
                    old_parent_nlink_delta + new_parent_nlink_delta,
                );
                self.txn_put_node(&mut txn, &old_parent_node, operation)
                    .await?;
            } else {
                old_parent_node.nlink =
                    apply_nlink_delta(old_parent_node.nlink, old_parent_nlink_delta);
                new_parent_node.nlink =
                    apply_nlink_delta(new_parent_node.nlink, new_parent_nlink_delta);
                self.txn_put_node(&mut txn, &old_parent_node, operation)
                    .await?;
                self.txn_put_node(&mut txn, &new_parent_node, operation)
                    .await?;
            }
            self.txn_put_dentry(&mut txn, new_parent, &new_name, &source_dentry, operation)
                .await
        }
        .await;
        self.finish_write(&mut txn, operation, result).await
    }

    async fn rename_exchange(
        &self,
        old_parent: i64,
        old_name: &str,
        new_parent: i64,
        new_name: &str,
    ) -> Result<(), MetaError> {
        if old_parent == new_parent && old_name == new_name {
            return Ok(());
        }

        let operation = "rename_exchange";
        let mut txn = self.begin_write(operation).await?;
        let result = async {
            let mut old_parent_node = self
                .txn_require_dir(&mut txn, old_parent, true, operation)
                .await?;
            let mut new_parent_node = if old_parent == new_parent {
                old_parent_node.clone()
            } else {
                self.txn_require_dir(&mut txn, new_parent, true, operation)
                    .await?
            };

            let old_dentry = self
                .txn_get_dentry(&mut txn, old_parent, old_name, true, operation)
                .await?
                .ok_or(MetaError::NotFound(old_parent))?;
            let new_dentry = self
                .txn_get_dentry(&mut txn, new_parent, new_name, true, operation)
                .await?
                .ok_or(MetaError::NotFound(new_parent))?;

            if old_dentry.ino == new_dentry.ino {
                return Ok(());
            }

            let mut old_node = self
                .txn_get_node(&mut txn, old_dentry.ino, true, operation)
                .await?
                .ok_or(MetaError::NotFound(old_dentry.ino))?;
            let mut new_node = self
                .txn_get_node(&mut txn, new_dentry.ino, true, operation)
                .await?
                .ok_or(MetaError::NotFound(new_dentry.ino))?;

            if old_node.deleted || old_node.nlink == 0 {
                return Err(MetaError::NotFound(old_dentry.ino));
            }
            if new_node.deleted || new_node.nlink == 0 {
                return Err(MetaError::NotFound(new_dentry.ino));
            }

            let now = Self::now();
            let mut old_parent_nlink_delta = 0;
            let mut new_parent_nlink_delta = 0;

            if old_parent != new_parent {
                if old_node.kind == StoredNodeKind::Dir {
                    old_parent_nlink_delta -= 1;
                    new_parent_nlink_delta += 1;
                }
                if new_node.kind == StoredNodeKind::Dir {
                    new_parent_nlink_delta -= 1;
                    old_parent_nlink_delta += 1;
                }
            }

            self.txn_move_node_binding(
                &mut txn,
                &mut old_node,
                old_parent,
                old_name,
                new_parent,
                new_name,
                operation,
            )
            .await?;
            self.txn_move_node_binding(
                &mut txn,
                &mut new_node,
                new_parent,
                new_name,
                old_parent,
                old_name,
                operation,
            )
            .await?;

            old_node.mtime = now;
            old_node.ctime = now;
            new_node.mtime = now;
            new_node.ctime = now;

            Self::txn_put_raw(
                &mut txn,
                self.dentry_key(old_parent, old_name),
                Self::encode(&new_dentry)?,
                operation,
            )
            .await?;
            Self::txn_put_raw(
                &mut txn,
                self.dentry_key(new_parent, new_name),
                Self::encode(&old_dentry)?,
                operation,
            )
            .await?;
            self.txn_put_node(&mut txn, &old_node, operation).await?;
            self.txn_put_node(&mut txn, &new_node, operation).await?;

            old_parent_node.mtime = now;
            old_parent_node.ctime = now;
            new_parent_node.mtime = now;
            new_parent_node.ctime = now;
            if old_parent == new_parent {
                old_parent_node.nlink = apply_nlink_delta(
                    old_parent_node.nlink,
                    old_parent_nlink_delta + new_parent_nlink_delta,
                );
                self.txn_put_node(&mut txn, &old_parent_node, operation)
                    .await?;
            } else {
                old_parent_node.nlink =
                    apply_nlink_delta(old_parent_node.nlink, old_parent_nlink_delta);
                new_parent_node.nlink =
                    apply_nlink_delta(new_parent_node.nlink, new_parent_nlink_delta);
                self.txn_put_node(&mut txn, &old_parent_node, operation)
                    .await?;
                self.txn_put_node(&mut txn, &new_parent_node, operation)
                    .await?;
            }

            Ok(())
        }
        .await;
        self.finish_write(&mut txn, operation, result).await
    }

    async fn set_file_size(&self, ino: i64, size: u64) -> Result<(), MetaError> {
        let operation = "set_file_size";
        let mut txn = self.begin_write(operation).await?;
        let result = async {
            let mut node = self
                .txn_get_node(&mut txn, ino, true, operation)
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
            self.txn_put_node(&mut txn, &node, operation).await
        }
        .await;
        self.finish_write(&mut txn, operation, result).await
    }

    async fn set_attr(
        &self,
        ino: i64,
        req: &SetAttrRequest,
        flags: SetAttrFlags,
    ) -> Result<FileAttr, MetaError> {
        let operation = "set_attr";
        let mut txn = self.begin_write(operation).await?;
        let result = async {
            let mut node = self
                .txn_get_node(&mut txn, ino, true, operation)
                .await?
                .ok_or(MetaError::NotFound(ino))?;
            let now = Self::now();
            let mut ctime_update = false;

            if let Some(mode) = req.mode {
                let kind_bits = node.mode & 0o170000;
                node.mode = kind_bits | (mode & 0o777);
                ctime_update = true;
            }
            if let Some(uid) = req.uid {
                node.uid = uid;
                ctime_update = true;
            }
            if let Some(gid) = req.gid {
                node.gid = gid;
                ctime_update = true;
            }
            if flags.contains(SetAttrFlags::CLEAR_SUID) {
                node.mode &= !0o4000;
                ctime_update = true;
            }
            if flags.contains(SetAttrFlags::CLEAR_SGID) {
                node.mode &= !0o2000;
                ctime_update = true;
            }

            if let Some(size) = req.size {
                if node.kind != StoredNodeKind::File {
                    return Err(MetaError::NotSupported(
                        "truncate flag only supported for regular files".to_string(),
                    ));
                }
                if node.size != size {
                    node.size = size;
                    node.blocks = size.div_ceil(512);
                    node.mtime = now;
                }
                ctime_update = true;
            }

            if flags.contains(SetAttrFlags::SET_ATIME_NOW) {
                node.atime = now;
                ctime_update = true;
            } else if let Some(atime) = req.atime {
                node.atime = atime;
                ctime_update = true;
            }

            if flags.contains(SetAttrFlags::SET_MTIME_NOW) {
                node.mtime = now;
                ctime_update = true;
            } else if let Some(mtime) = req.mtime {
                node.mtime = mtime;
                ctime_update = true;
            }

            if let Some(ctime) = req.ctime {
                node.ctime = ctime;
            } else if ctime_update {
                node.ctime = now;
            }

            self.txn_put_node(&mut txn, &node, operation).await?;
            Ok(node.to_attr())
        }
        .await;
        self.finish_write(&mut txn, operation, result).await
    }

    async fn open(&self, ino: i64, flags: OpenFlags) -> Result<FileAttr, MetaError> {
        let operation = "open";
        let mut txn = self.begin_write(operation).await?;
        let result = async {
            let mut node = self
                .txn_get_node(&mut txn, ino, true, operation)
                .await?
                .ok_or(MetaError::NotFound(ino))?;
            if node.kind == StoredNodeKind::Symlink {
                return Err(MetaError::NotSupported(
                    "opening symlink targets is not implemented".to_string(),
                ));
            }
            if flags.contains(OpenFlags::TRUNC) && node.kind != StoredNodeKind::File {
                return Err(MetaError::NotSupported(
                    "truncate flag only supported for regular files".to_string(),
                ));
            }

            let now = Self::now();
            node.atime = now;
            if flags.contains(OpenFlags::TRUNC) {
                node.size = 0;
                node.blocks = 0;
                node.mtime = now;
                node.ctime = now;
            }

            self.txn_put_node(&mut txn, &node, operation).await?;
            Ok(node.to_attr())
        }
        .await;
        self.finish_write(&mut txn, operation, result).await
    }

    async fn close(&self, ino: i64) -> Result<(), MetaError> {
        if self.stat(ino).await?.is_some() {
            Ok(())
        } else {
            Err(MetaError::NotFound(ino))
        }
    }

    async fn get_names(&self, ino: i64) -> Result<Vec<(Option<i64>, String)>, MetaError> {
        if ino == ROOT_INODE {
            return Ok(vec![(None, "/".to_string())]);
        }

        let operation = "get_names";
        let mut txn = self.begin_read(operation).await?;
        let result = async {
            let Some(node) = self.txn_get_node(&mut txn, ino, false, operation).await? else {
                return Ok(Vec::new());
            };
            if node.deleted || node.nlink == 0 {
                return Ok(Vec::new());
            }
            if node.kind == StoredNodeKind::Dir || node.nlink <= 1 {
                return Ok(vec![(Some(node.parent), node.name)]);
            }

            let mut out: Vec<_> = self
                .txn_get_link_parents(&mut txn, ino, false, operation)
                .await?
                .into_iter()
                .map(|link| (Some(link.parent), link.name))
                .collect();
            out.sort();
            out.dedup();
            Ok(out)
        }
        .await;
        Self::rollback_best_effort(&mut txn, operation).await;
        result
    }

    async fn get_paths(&self, ino: i64) -> Result<Vec<String>, MetaError> {
        if ino == ROOT_INODE {
            return Ok(vec!["/".to_string()]);
        }

        let operation = "get_paths";
        let mut txn = self.begin_read(operation).await?;
        let result = async {
            let Some(node) = self.txn_get_node(&mut txn, ino, false, operation).await? else {
                return Ok(Vec::new());
            };
            if node.deleted || node.nlink == 0 {
                return Ok(Vec::new());
            }

            let bindings = if node.kind == StoredNodeKind::Dir || node.nlink <= 1 {
                vec![StoredLinkParent {
                    parent: node.parent,
                    name: node.name,
                }]
            } else {
                self.txn_get_link_parents(&mut txn, ino, false, operation)
                    .await?
            };

            let mut out = Vec::with_capacity(bindings.len());
            for binding in bindings {
                let mut parts = vec![binding.name];
                let mut current_parent = binding.parent;
                while current_parent != ROOT_INODE {
                    let Some(parent) = self
                        .txn_get_node(&mut txn, current_parent, false, operation)
                        .await?
                    else {
                        parts.clear();
                        break;
                    };
                    if parent.deleted || parent.nlink == 0 {
                        parts.clear();
                        break;
                    }
                    parts.push(parent.name);
                    current_parent = parent.parent;
                }
                if !parts.is_empty() {
                    parts.reverse();
                    out.push(format!("/{}", parts.join("/")));
                }
            }
            out.sort();
            out.dedup();
            Ok(out)
        }
        .await;
        Self::rollback_best_effort(&mut txn, operation).await;
        result
    }

    fn root_ino(&self) -> i64 {
        ROOT_INODE
    }

    async fn initialize(&self) -> Result<(), MetaError> {
        let operation = "initialize";
        let mut txn = self.begin_read(operation).await?;
        let root_exists = self
            .txn_get_node(&mut txn, ROOT_INODE, false, operation)
            .await?
            .is_some();
        let counter_exists =
            Self::txn_get_raw(&mut txn, self.counter_key(INODE_ID_KEY), false, operation)
                .await?
                .is_some();
        Self::rollback_best_effort(&mut txn, operation).await;

        if root_exists && counter_exists {
            return Ok(());
        }

        let mut txn = self.begin_write(operation).await?;
        let result = async {
            if self
                .txn_get_node(&mut txn, ROOT_INODE, true, operation)
                .await?
                .is_none()
            {
                self.txn_put_node(&mut txn, &Self::root_node(), operation)
                    .await?;
            }

            let counter_key = self.counter_key(INODE_ID_KEY);
            if Self::txn_get_raw(&mut txn, counter_key.clone(), true, operation)
                .await?
                .is_none()
            {
                Self::txn_put_raw(
                    &mut txn,
                    counter_key,
                    Self::encode(&FIRST_ALLOCATED_INODE)?,
                    operation,
                )
                .await?;
            }
            Ok(())
        }
        .await;
        self.finish_write(&mut txn, operation, result).await
    }

    async fn get_deleted_files(&self) -> Result<Vec<i64>, MetaError> {
        let operation = "get_deleted_files";
        let mut txn = self.begin_read(operation).await?;
        let result = async {
            let pairs =
                Self::txn_scan_prefix(&mut txn, self.inode_prefix(), None, operation).await?;
            let mut out = Vec::new();
            for pair in pairs {
                let node = Self::decode_node(pair.value())?;
                if node.deleted && node.kind != StoredNodeKind::Dir {
                    out.push(node.ino);
                }
            }
            Ok(out)
        }
        .await;
        Self::rollback_best_effort(&mut txn, operation).await;
        result
    }

    async fn remove_file_metadata(&self, ino: i64) -> Result<(), MetaError> {
        let operation = "remove_file_metadata";
        let mut txn = self.begin_write(operation).await?;
        let result = async {
            Self::txn_delete_raw(&mut txn, self.inode_key(ino), operation).await?;
            self.txn_delete_link_parents(&mut txn, ino, operation).await
        }
        .await;
        self.finish_write(&mut txn, operation, result).await
    }

    async fn get_slices(&self, chunk_id: u64) -> Result<Vec<SliceDesc>, MetaError> {
        let operation = "get_slices";
        let mut txn = self.begin_read(operation).await?;
        let result = Self::txn_get_raw(&mut txn, self.chunk_key(chunk_id), false, operation)
            .await?
            .as_deref()
            .map(Self::decode_slices)
            .transpose()
            .map(|slices| slices.unwrap_or_default());
        Self::rollback_best_effort(&mut txn, operation).await;
        result
    }

    async fn list_chunk_ids(&self, limit: usize) -> Result<Vec<u64>, MetaError> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let operation = "list_chunk_ids";
        let mut txn = self.begin_read(operation).await?;
        let result = async {
            let prefix = self.chunk_prefix();
            let pairs =
                Self::txn_scan_prefix(&mut txn, prefix.clone(), Some(limit), operation).await?;
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
        }
        .await;
        Self::rollback_best_effort(&mut txn, operation).await;
        result
    }

    async fn append_slice(&self, chunk_id: u64, slice: SliceDesc) -> Result<(), MetaError> {
        let operation = "append_slice";
        let mut txn = self.begin_write(operation).await?;
        let result = async {
            let chunk_key = self.chunk_key(chunk_id);
            let mut slices = Self::txn_get_raw(&mut txn, chunk_key.clone(), true, operation)
                .await?
                .as_deref()
                .map(Self::decode_slices)
                .transpose()?
                .unwrap_or_default();
            slices.push(slice);
            Self::txn_put_raw(&mut txn, chunk_key, Self::encode(&slices)?, operation).await
        }
        .await;
        self.finish_write(&mut txn, operation, result).await
    }

    async fn write(
        &self,
        ino: i64,
        chunk_id: u64,
        slice: SliceDesc,
        new_size: u64,
    ) -> Result<(), MetaError> {
        let operation = "write";
        let mut txn = self.begin_write(operation).await?;
        let result = async {
            let mut node = self
                .txn_get_node(&mut txn, ino, true, operation)
                .await?
                .ok_or(MetaError::NotFound(ino))?;
            if node.kind != StoredNodeKind::File {
                return Err(MetaError::NotSupported(
                    "TiKV write currently supports only regular files".to_string(),
                ));
            }

            let chunk_key = self.chunk_key(chunk_id);
            let mut slices = Self::txn_get_raw(&mut txn, chunk_key.clone(), true, operation)
                .await?
                .as_deref()
                .map(Self::decode_slices)
                .transpose()?
                .unwrap_or_default();
            slices.push(slice);
            Self::txn_put_raw(&mut txn, chunk_key, Self::encode(&slices)?, operation).await?;

            let now = Self::now();
            node.size = new_size;
            node.blocks = new_size.div_ceil(512);
            node.mtime = now;
            node.ctime = now;
            self.txn_put_node(&mut txn, &node, operation).await
        }
        .await;
        self.finish_write(&mut txn, operation, result).await
    }

    async fn next_id(&self, key: &str) -> Result<i64, MetaError> {
        let operation = "next_id";
        let mut txn = self.begin_write(operation).await?;
        let result = self.txn_next_counter(&mut txn, key, 1, operation).await;
        self.finish_write(&mut txn, operation, result).await
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

fn apply_nlink_delta(nlink: u32, delta: i32) -> u32 {
    if delta >= 0 {
        nlink.saturating_add(delta as u32)
    } else {
        nlink.saturating_sub(delta.unsigned_abs())
    }
}

#[cfg(test)]
mod tests;
