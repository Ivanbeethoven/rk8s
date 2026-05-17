//! Storage backends: asynchronous block-level IO traits and in-memory implementations.

use crate::chunk::page_cache::{ReadPageCache, PageKey};
use crate::chunk::singleflight::SingleFlight;
use crate::utils::NumCastExt;
use crate::utils::zero::make_zero_bytes;
use crate::{
    cadapter::client::{ObjectBackend, ObjectClient},
    chunk::cache::{ChunksCache, ChunksCacheConfig},
};
use anyhow::{self, Context};
use async_trait::async_trait;
use bytes::Bytes;
use futures::executor::block_on;
use hex::encode;
use moka::{Entry, ops::compute::Op};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, fs, io::SeekFrom, path::PathBuf, sync::LazyLock};
use tokio::{
    io::{self, AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::RwLock,
};

/// Abstract block store interface (cadapter/S3/etc. can implement this).
#[async_trait]
// ensure offset_in_block + data.len() <= block_size
pub trait BlockStore {
    /// Write a new block without reading any existing data.
    ///
    /// All writes use copy-on-write semantics: every write targets a fresh
    /// object/key.  There is no read-modify-write path — callers must ensure
    /// the target key is fresh; using this on an existing object would drop
    /// any previous content outside the written range.
    #[tracing::instrument(level = "trace", skip(self, chunks), fields(key = ?key, offset, chunk_count = chunks.len()))]
    async fn write_fresh_vectored(
        &self,
        key: BlockKey,
        offset: u64,
        chunks: Vec<Bytes>,
    ) -> anyhow::Result<u64> {
        let data = chunks
            .into_iter()
            .flat_map(|e| e.to_vec())
            .collect::<Vec<_>>();
        self.write_fresh_range(key, offset, &data).await
    }

    /// Write a new block without reading any existing data.
    /// Required — every store must implement COW writes directly.
    async fn write_fresh_range(
        &self,
        key: BlockKey,
        offset: u64,
        data: &[u8],
    ) -> anyhow::Result<u64>;

    async fn read_range(&self, key: BlockKey, offset: u64, buf: &mut [u8]) -> anyhow::Result<()>;

    /// Delete `block_count` blocks starting from `key.1` (block_index) for slice `key.0`.
    async fn delete_range(&self, key: BlockKey, block_count: u64) -> anyhow::Result<()>;

    /// Proactively insert a block into the read cache after upload.
    /// Default is a no-op; ObjectBlockStore overrides to populate ChunksCache.
    async fn cache_block(&self, _key: BlockKey, _data: &[u8]) -> anyhow::Result<()> {
        Ok(())
    }
}

pub type BlockKey = (u64 /*slice_id*/, u32 /*block_index*/);

/// Simple in-memory implementation for local development/testing.
#[derive(Default)]
#[allow(dead_code)]
pub struct InMemoryBlockStore {
    map: RwLock<HashMap<BlockKey, Vec<u8>>>,
}

#[allow(dead_code)]
impl InMemoryBlockStore {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            map: RwLock::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl BlockStore for InMemoryBlockStore {
    async fn write_fresh_range(
        &self,
        key: BlockKey,
        offset: u64,
        data: &[u8],
    ) -> anyhow::Result<u64> {
        let mut guard = self.map.write().await;
        let entry = guard.entry(key).or_insert_with(Vec::new);
        let start = offset.as_usize();
        let end = start + data.len();
        if entry.len() < end {
            entry.resize(end, 0);
        }
        entry[start..end].copy_from_slice(data);
        Ok(data.len() as u64)
    }

    // Caller is responsible for zero-filling buf; this method only overwrites existing bytes.
    async fn read_range(&self, key: BlockKey, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let guard = self.map.read().await;
        if let Some(src) = guard.get(&key) {
            let start = offset.as_usize();
            let end = start + buf.len();
            let copy_end = end.min(src.len());
            if copy_end > start {
                let len = copy_end - start;
                buf[..len].copy_from_slice(&src[start..copy_end]);
            }
        }
        Ok(())
    }

    async fn delete_range(&self, key: BlockKey, block_count: u64) -> anyhow::Result<()> {
        let (chunk_id, block_index) = key;
        let mut guard = self.map.write().await;
        let start = block_index;
        let end = start + block_count.as_u32();
        for i in start..end {
            guard.remove(&(chunk_id, i));
        }
        Ok(())
    }
}

/// BlockStore backed by cadapter::client (key space `chunks/{chunk_id}/{block_index}`).
pub struct ObjectBlockStore<B: ObjectBackend> {
    client: ObjectClient<B>,
    block_cache: ChunksCache,
    /// Page-granularity (64KB) read cache for small range reads that would
    /// otherwise be discarded.  Intercepts repeated small random reads so they
    /// hit memory instead of making a network round-trip every time.
    page_cache: ReadPageCache,
    /// SingleFlight controller for coalescing concurrent reads to the same block
    /// Thread-safe and shared across the store lifetime so concurrent requests can coalesce.
    read_flight: SingleFlight<BlockKey, Bytes>,
    /// Configuration for read strategy
    config: BlockStoreConfig,
}

/// Configuration for ObjectBlockStore read strategy
#[derive(Debug, Clone)]
pub struct BlockStoreConfig {
    /// Block size in bytes (default: 4MB)
    pub block_size: usize,
    /// For ranges smaller than this threshold, use direct range read instead of full block read
    /// Default is 25% of block size (1MB for 4MB blocks)
    pub range_read_threshold: f32,
    /// Page size for the page-granularity read cache (default: 64KB).
    /// Small range reads are aligned to page boundaries, fetched, and cached at this granularity.
    pub page_size: usize,
    /// Maximum number of pages in the read cache (default: 4096 → 256MB with 64KB pages).
    pub page_cache_capacity: usize,
}

impl Default for BlockStoreConfig {
    fn default() -> Self {
        Self {
            block_size: 4 * 1024 * 1024, // 4MB
            range_read_threshold: 0.25,  // 25% = 1MB for 4MB blocks
            page_size: 64 * 1024,        // 64KB
            page_cache_capacity: 4096,   // 4096 pages × 64KB = 256MB
        }
    }
}

impl BlockStoreConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.block_size == 0 {
            anyhow::bail!("block_size must be greater than 0");
        }
        if !(0.0..=1.0).contains(&self.range_read_threshold) {
            anyhow::bail!("range_read_threshold must be between 0.0 and 1.0");
        }
        if self.page_size == 0 {
            anyhow::bail!("page_size must be greater than 0");
        }
        if self.page_cache_capacity == 0 {
            anyhow::bail!("page_cache_capacity must be greater than 0");
        }
        Ok(())
    }

    fn range_size_threshold(&self) -> usize {
        (self.block_size as f32 * self.range_read_threshold) as usize
    }
}

impl<B: ObjectBackend> ObjectBlockStore<B> {
    pub fn new(client: ObjectClient<B>) -> Self {
        let cache_dir = dirs::cache_dir().unwrap().join("slayerfs");

        let _ = fs::create_dir_all(cache_dir.clone());

        let block_cache = block_on(ChunksCache::new_with_config(ChunksCacheConfig::default()))
            .map_err(|e| anyhow::anyhow!("Failed to create cache: {}", e))
            .unwrap();
        let config = BlockStoreConfig::default();
        config.validate().expect("default config must be valid");
        let page_cache = ReadPageCache::new(config.page_cache_capacity, config.page_size);
        Self {
            client,
            block_cache,
            page_cache,
            read_flight: SingleFlight::new(),
            config,
        }
    }
    /// Creates a new ObjectBlockStore with custom cache configuration
    #[allow(unused)]
    pub fn new_with_config(
        client: ObjectClient<B>,
        cache_config: ChunksCacheConfig,
    ) -> anyhow::Result<Self> {
        Self::new_with_configs(client, cache_config, BlockStoreConfig::default())
    }

    /// Creates a new ObjectBlockStore with custom cache and block store configurations
    #[allow(unused)]
    pub fn new_with_configs(
        client: ObjectClient<B>,
        cache_config: ChunksCacheConfig,
        store_config: BlockStoreConfig,
    ) -> anyhow::Result<Self> {
        store_config.validate()?;
        let cache_dir = dirs::cache_dir().unwrap().join("slayerfs");
        let _ = fs::create_dir_all(cache_dir.clone());

        let block_cache = block_on(ChunksCache::new_with_config(cache_config))
            .map_err(|e| anyhow::anyhow!("Failed to create cache: {}", e))?;
        let page_cache = ReadPageCache::new(store_config.page_cache_capacity, store_config.page_size);
        Ok(Self {
            client,
            block_cache,
            page_cache,
            read_flight: SingleFlight::new(),
            config: store_config,
        })
    }

    fn key_for(key: BlockKey) -> String {
        let (chunk_id, block_index) = key;
        format!("chunks/{chunk_id}/{block_index}")
    }
}

#[async_trait]
impl<B: ObjectBackend + Send + Sync> BlockStore for ObjectBlockStore<B> {
    #[tracing::instrument(name = "ObjectBlockStore.write_fresh_vectored", level = "trace", skip(self, chunks), fields(key = ?key, offset, chunk_count = chunks.len()))]
    async fn write_fresh_vectored(
        &self,
        key: BlockKey,
        offset: u64,
        chunks: Vec<Bytes>,
    ) -> anyhow::Result<u64> {
        let key_str = Self::key_for(key);
        let total_len = chunks.iter().map(|c| c.len()).sum::<usize>();
        if total_len == 0 {
            return Ok(0);
        }

        let offset_usize = offset.as_usize();
        let mut parts: Vec<Bytes> = Vec::new();
        if offset_usize > 0 {
            parts.extend(make_zero_bytes(offset_usize));
        }
        parts.extend(chunks);

        self.client
            .put_object_vectored(&key_str, parts)
            .await
            .map_err(|e| anyhow::anyhow!("object store put failed: {key_str}, {e:?}"))?;

        Ok(total_len as u64)
    }

    async fn write_fresh_range(
        &self,
        key: BlockKey,
        offset: u64,
        data: &[u8],
    ) -> anyhow::Result<u64> {
        let key_str = Self::key_for(key);
        if data.is_empty() {
            return Ok(0);
        }

        let offset_usize = offset.as_usize();
        let mut parts = Vec::new();
        if offset_usize > 0 {
            parts.extend(make_zero_bytes(offset_usize));
        }
        parts.push(Bytes::copy_from_slice(data));

        self.client
            .put_object_vectored(&key_str, parts)
            .await
            .map_err(|e| anyhow::anyhow!("object store put failed: {key_str}, {e:?}"))?;

        Ok(data.len() as u64)
    }

    #[tracing::instrument(
        name = "ObjectBlockStore.read_range",
        level = "trace",
        skip(self, buf),
        fields(key = ?key, offset, len = buf.len(), read_len = tracing::field::Empty, strategy = tracing::field::Empty)
    )]
    // Caller is responsible for zero-filling buf; this method only overwrites existing bytes.
    async fn read_range(&self, key: BlockKey, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        let len = buf.len();
        let key_str = Self::key_for(key);

        // Try cache first — blocks are immutable once committed, so a cache
        // hit is always valid regardless of read size or offset.
        if let Some(cached) = self.block_cache.get(&key_str).await {
            tracing::Span::current().record("strategy", "cache_hit");
            let offset_usize = offset as usize;
            let end = (offset_usize + len).min(cached.len());
            if offset_usize < cached.len() {
                let copy_len = end - offset_usize;
                buf[..copy_len].copy_from_slice(&cached[offset_usize..end]);
                tracing::Span::current().record("read_len", copy_len);
            }
            return Ok(());
        }

        let range_size_threshold = self.config.range_size_threshold();

        if len <= range_size_threshold {
            // Small range read — serve via page-granularity cache so that
            // repeated small reads within the same 64KB page avoid a network
            // round-trip.
            let page_size = self.page_cache.page_size();
            let start_page = offset as usize / page_size;
            // end_page is inclusive
            let end_page = (offset as usize + len - 1) / page_size;

            let client = &self.client;
            let page_cache = &self.page_cache;
            let mut pos: usize = 0;
            let mut total_read: usize = 0;

            for page_idx in start_page..=end_page {
                let page_start = page_idx * page_size;
                let page_end = (page_start + page_size).min(self.config.block_size);

                let cache_key: PageKey = (key.0, key.1, page_idx as u32);

                let page_data = if let Some(cached) = page_cache.get(&cache_key).await {
                    tracing::Span::current().record("strategy", "page_cache_hit");
                    cached
                } else {
                    tracing::Span::current().record("strategy", "page_cache_miss");
                    let range_offset = page_start as u64;
                    let range_len = page_end - page_start;
                    let mut page_buf = vec![0u8; range_len];
                    let read_len = client
                        .get_object_range(&key_str, range_offset, &mut page_buf)
                        .await
                        .map_err(|e| {
                            anyhow::anyhow!("object store range read failed: {key_str}, {e:?}")
                        })?;
                    page_buf.truncate(read_len);
                    let page_bytes = Bytes::from(page_buf);
                    page_cache.insert(cache_key, page_bytes.clone()).await;
                    page_bytes
                };

                // Determine the byte range within this page that the caller needs
                let copy_start = if page_idx == start_page {
                    offset as usize - page_start
                } else {
                    0
                };
                let copy_end = if page_idx == end_page {
                    (offset as usize + len).saturating_sub(page_start)
                } else {
                    page_data.len()
                };
                let copy_end = copy_end.min(page_data.len());
                if copy_end > copy_start {
                    let copy_len = copy_end - copy_start;
                    buf[pos..pos + copy_len]
                        .copy_from_slice(&page_data[copy_start..copy_end]);
                    pos += copy_len;
                    total_read += copy_len;
                }
            }

            tracing::Span::current().record("read_len", total_read);
            return Ok(());
        }

        // Large read — fetch full block via SingleFlight, then cache it.
        tracing::Span::current().record("strategy", "coalesced_full");
        let client = &self.client;

        let block_data =
            self.read_flight
                .execute(key, || async move {
                    let key_str = Self::key_for(key);
                    let data = client.get_object(&key_str).await.map_err(|e| {
                        anyhow::anyhow!("object store get failed: {key_str}, {e:?}")
                    })?;
                    Ok::<_, anyhow::Error>(Bytes::from(data.unwrap_or_default()))
                })
                .await
                .map_err(|e| anyhow::anyhow!("SingleFlight read failed: {e}"))?;

        // Populate cache with the full block for future reads.
        let _ = self
            .block_cache
            .insert(&key_str, &block_data.to_vec())
            .await;

        let offset_usize = offset as usize;
        let end = offset_usize + len;
        let mut copy_len = 0;
        if offset_usize < block_data.len() {
            let copy_end = end.min(block_data.len());
            copy_len = copy_end - offset_usize;
            buf[..copy_len].copy_from_slice(&block_data.as_ref()[offset_usize..copy_end]);
        }
        tracing::Span::current().record("read_len", copy_len);

        Ok(())
    }

    async fn delete_range(&self, key: BlockKey, block_count: u64) -> anyhow::Result<()> {
        let (chunk_id, block_index) = key;
        let start = block_index;
        let end = start + block_count.as_u32();
        for i in start..end {
            let key_str = Self::key_for((chunk_id, i));
            self.client
                .delete_object(&key_str)
                .await
                .map_err(|e| anyhow::anyhow!("object store delete failed: {key_str}, {e:?}"))?;
        }
        Ok(())
    }

    async fn cache_block(&self, key: BlockKey, data: &[u8]) -> anyhow::Result<()> {
        let key_str = Self::key_for(key);
        let _ = self.block_cache.insert(&key_str, &data.to_vec()).await;
        Ok(())
    }
}

/// Convenience alias: BlockStore backed by the real S3 backend.
#[allow(dead_code)]
pub type S3BlockStore = ObjectBlockStore<crate::cadapter::s3::S3Backend>;
/// Convenience alias: BlockStore backed by the LocalFs mock backend.
#[allow(dead_code)]
pub type LocalFsBlockStore = ObjectBlockStore<crate::cadapter::localfs::LocalFsBackend>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cadapter::client::ObjectClient;
    use crate::cadapter::localfs::LocalFsBackend;
    use crate::chunk::layout::ChunkLayout;

    #[tokio::test]
    async fn test_localfs_block_store_put_get() {
        let tmp = tempfile::tempdir().unwrap();
        let client = ObjectClient::new(LocalFsBackend::new(tmp.path()));
        let store = ObjectBlockStore::new(client);
        let layout = ChunkLayout::default();

        let data = vec![7u8; layout.block_size as usize / 2];
        store
            .write_fresh_range((42, 3), (layout.block_size / 4) as u64, &data)
            .await
            .unwrap();

        let mut out = vec![0u8; data.len()];
        store
            .read_range((42, 3), (layout.block_size / 4) as u64, &mut out)
            .await
            .unwrap();
        assert_eq!(out, data);
    }

    #[tokio::test]
    async fn test_cache_effectiveness() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        let client = ObjectClient::new(LocalFsBackend::new(tmp.path()));
        let store = ObjectBlockStore::new(client);
        let layout = ChunkLayout::default();
        let data = vec![7u8; layout.block_size as usize / 2];
        store
            .write_fresh_range((42, 3), (layout.block_size / 4) as u64, &data)
            .await
            .unwrap();
        // First read should miss the cache.
        let mut data1 = vec![0u8; data.len()];
        store
            .read_range((42, 3), (layout.block_size / 4) as u64, &mut data1)
            .await
            .unwrap();

        // Second read of the same data should hit the cache.
        let mut data2 = vec![0u8; data.len()];
        store
            .read_range((42, 3), (layout.block_size / 4) as u64, &mut data2)
            .await
            .unwrap();
        assert_eq!(data1, data2);

        Ok(())
    }

    #[tokio::test]
    async fn test_intelligent_read_strategy() -> Result<(), Box<dyn std::error::Error>> {
        use crate::cadapter::client::{ObjectBackend, ObjectClient};
        use async_trait::async_trait;
        use futures::future;
        use std::{
            collections::HashMap,
            sync::{Arc, Mutex},
        };
        use tokio::time::{Duration, sleep};

        #[derive(Debug, Clone)]
        struct MockStats {
            get_object_calls: usize,
            get_object_range_calls: usize,
        }

        #[derive(Clone)]
        struct MockBackend {
            data: Arc<Mutex<HashMap<String, Vec<u8>>>>,
            stats: Arc<Mutex<MockStats>>,
        }

        impl MockBackend {
            fn new() -> Self {
                let mut data = HashMap::new();
                // Create a 4MB block with known pattern
                let block_data: Vec<u8> = (0..4_194_304).map(|i| (i % 256) as u8).collect();
                data.insert("chunks/42/3".to_string(), block_data);

                Self {
                    data: Arc::new(Mutex::new(data)),
                    stats: Arc::new(Mutex::new(MockStats {
                        get_object_calls: 0,
                        get_object_range_calls: 0,
                    })),
                }
            }

            fn get_stats(&self) -> MockStats {
                self.stats.lock().unwrap().clone()
            }

            fn reset_stats(&self) {
                let mut stats = self.stats.lock().unwrap();
                stats.get_object_calls = 0;
                stats.get_object_range_calls = 0;
            }
        }

        #[async_trait]
        impl ObjectBackend for MockBackend {
            async fn put_object(&self, key: &str, data: &[u8]) -> anyhow::Result<()> {
                self.data
                    .lock()
                    .unwrap()
                    .insert(key.to_string(), data.to_vec());
                Ok(())
            }

            async fn get_object(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
                // Simulate some latency so SingleFlight can coalesce concurrent requests
                sleep(Duration::from_millis(10)).await;
                self.stats.lock().unwrap().get_object_calls += 1;
                Ok(self.data.lock().unwrap().get(key).cloned())
            }

            async fn get_object_range(
                &self,
                key: &str,
                offset: u64,
                buf: &mut [u8],
            ) -> anyhow::Result<usize> {
                self.stats.lock().unwrap().get_object_range_calls += 1;
                if let Some(data) = self.data.lock().unwrap().get(key) {
                    let offset = offset as usize;
                    let end = (offset + buf.len()).min(data.len());
                    if offset < data.len() {
                        let copy_len = end - offset;
                        buf[..copy_len].copy_from_slice(&data[offset..end]);
                        Ok(copy_len)
                    } else {
                        Ok(0)
                    }
                } else {
                    Ok(0)
                }
            }

            async fn get_etag(&self, _key: &str) -> anyhow::Result<String> {
                Ok("test_etag".to_string())
            }

            async fn delete_object(&self, key: &str) -> anyhow::Result<()> {
                self.data.lock().unwrap().remove(key);
                Ok(())
            }
        }

        // Test small range uses direct read
        let backend = MockBackend::new();
        let client = ObjectClient::new(backend.clone());
        let config = BlockStoreConfig {
            block_size: 4 * 1024 * 1024,
            range_read_threshold: 0.25, // 1MB threshold
            ..Default::default()
        };
        let store = Arc::new(ObjectBlockStore::new_with_configs(
            client,
            ChunksCacheConfig::default(),
            config,
        )?);

        backend.reset_stats();

        // Small read (512KB < 1MB threshold) — uses page cache.
        // 512KB = 8 × 64KB pages, each page triggers one range GET on first access.
        let mut small_buf = vec![0u8; 512 * 1024];
        store.read_range((42, 3), 0, &mut small_buf).await?;

        let stats = backend.get_stats();
        assert_eq!(
            stats.get_object_range_calls, 8,
            "512KB read should fetch 8 pages (8 × 64KB range reads)"
        );
        assert_eq!(
            stats.get_object_calls, 0,
            "Small read should not use full block read"
        );

        // Same read again — all pages should hit the page cache, zero new backend calls.
        backend.reset_stats();
        let mut small_buf2 = vec![0u8; 512 * 1024];
        store.read_range((42, 3), 0, &mut small_buf2).await?;
        assert_eq!(small_buf, small_buf2);

        let stats = backend.get_stats();
        assert_eq!(
            stats.get_object_range_calls, 0,
            "Re-read of same range should hit page cache (no new range reads)"
        );
        assert_eq!(
            stats.get_object_calls, 0,
            "Re-read should not fall back to full block read"
        );

        backend.reset_stats();

        // Large read (2MB > 1MB threshold) — should use full block read.
        let mut large_buf = vec![0u8; 2 * 1024 * 1024];
        store.read_range((42, 3), 0, &mut large_buf).await?;

        let stats = backend.get_stats();
        assert_eq!(stats.get_object_calls, 1, "Large read should use full read");
        assert_eq!(
            stats.get_object_range_calls, 0,
            "Large read should not use range read"
        );

        // Concurrent large reads for a DIFFERENT (uncached) block should
        // coalesce to a single backend call via SingleFlight.
        backend.reset_stats();
        let handles: Vec<_> = (0..5)
            .map(|_| {
                let store = store.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 2 * 1024 * 1024];
                    store.read_range((99, 1), 0, &mut buf).await
                })
            })
            .collect();

        future::try_join_all(handles).await?;

        let stats = backend.get_stats();
        assert_eq!(
            stats.get_object_calls, 1,
            "Concurrent reads should coalesce to 1 call"
        );
        assert_eq!(
            stats.get_object_range_calls, 0,
            "Coalesced path should not fall back to range reads",
        );

        Ok(())
    }
}
