use super::*;
use crate::chunk::SliceDesc;
use crate::meta::config::{
    CacheConfig, ClientOptions, CompactConfig, DatabaseConfig, DatabaseType,
};
use crate::meta::factory::MetaStoreFactory;
use crate::meta::store::{FileType, MetaStore};

fn test_config(namespace: &str) -> Config {
    Config {
        database: DatabaseConfig {
            db_config: DatabaseType::TiKv {
                pd_endpoints: vec!["127.0.0.1:2379".to_string()],
                namespace: namespace.to_string(),
            },
        },
        cache: CacheConfig::default(),
        client: ClientOptions::default(),
        compact: CompactConfig::default(),
    }
}

fn integration_config(test_name: &str) -> Config {
    let pd_endpoints = std::env::var("SLAYERFS_TIKV_PD_ENDPOINTS")
        .or_else(|_| std::env::var("SLAYERFS_META_TIKV_PD_ENDPOINTS"))
        .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
        .split(',')
        .map(str::trim)
        .filter(|endpoint| !endpoint.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    let namespace = format!(
        "slayerfs-test-{test_name}-{}",
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );

    Config {
        database: DatabaseConfig {
            db_config: DatabaseType::TiKv {
                pd_endpoints,
                namespace,
            },
        },
        cache: CacheConfig::default(),
        client: ClientOptions::default(),
        compact: CompactConfig::default(),
    }
}

#[test]
fn parses_tikv_database_config_from_yaml() {
    let config: Config = serde_yaml::from_str(
        r#"
database:
  type: tikv
  pd_endpoints:
    - 127.0.0.1:2379
  namespace: tenant-a
"#,
    )
    .expect("tikv YAML config should parse");

    match config.database.db_config {
        DatabaseType::TiKv {
            pd_endpoints,
            namespace,
        } => {
            assert_eq!(pd_endpoints, vec!["127.0.0.1:2379"]);
            assert_eq!(namespace, "tenant-a");
        }
        other => panic!("expected TiKv config, got {other:?}"),
    }
}

#[test]
fn key_schema_uses_namespace_prefixes() {
    assert_eq!(normalize_namespace("/tenant-a/"), "tenant-a");
    assert_eq!(normalize_namespace(""), default_tikv_namespace());
    assert_eq!(
        TiKvMetaStore::scoped_key("tenant-a", "inode/1"),
        b"tenant-a/inode/1".to_vec()
    );
    assert_eq!(
        TiKvMetaStore::scoped_key("tenant-a", "dentry/1/name"),
        b"tenant-a/dentry/1/name".to_vec()
    );
    assert_eq!(
        TiKvMetaStore::scoped_key("tenant-a", "chunk/42"),
        b"tenant-a/chunk/42".to_vec()
    );
    assert_eq!(
        TiKvMetaStore::scoped_key("tenant-a", "counter/slayerfs:next_inode_id"),
        b"tenant-a/counter/slayerfs:next_inode_id".to_vec()
    );
}

#[test]
fn prefix_range_end_bounds_namespace_scans() {
    assert_eq!(
        prefix_range_end(b"tenant-a/chunk/"),
        Some(b"tenant-a/chunk0".to_vec())
    );
    assert_eq!(prefix_range_end(&[0xff, 0xff]), None);
}

#[test]
fn stored_values_round_trip_through_json() {
    let node = TiKvMetaStore::root_node();
    let encoded = TiKvMetaStore::encode(&node).unwrap();
    let decoded = TiKvMetaStore::decode_node(&encoded).unwrap();

    assert_eq!(decoded.ino, ROOT_INODE);
    assert_eq!(decoded.kind, StoredNodeKind::Dir);
    assert_eq!(decoded.nlink, 2);
}

#[tokio::test]
async fn rejects_empty_pd_endpoints() {
    let mut config = test_config("tenant-a");
    config.database.db_config = DatabaseType::TiKv {
        pd_endpoints: Vec::new(),
        namespace: "tenant-a".to_string(),
    };

    let err = TiKvMetaStore::from_config(config)
        .await
        .expect_err("empty PD endpoint list should be rejected before connecting to TiKV");
    assert!(matches!(err, MetaError::Config(message) if message.contains("PD endpoint")));
}

#[tokio::test]
#[ignore = "requires a running TiKV/PD cluster; set SLAYERFS_TIKV_PD_ENDPOINTS"]
async fn tikv_factory_initializes_root() {
    let handle = MetaStoreFactory::<TiKvMetaStore>::create_from_config(integration_config("root"))
        .await
        .expect("factory should create and initialize tikv store");
    let store = handle.store();

    assert_eq!(store.name(), "tikv");
    assert_eq!(store.root_ino(), 1);
    let root = store.stat(1).await.unwrap().unwrap();
    assert_eq!(root.kind, FileType::Dir);
    assert_eq!(
        store.lookup_path("/").await.unwrap(),
        Some((1, FileType::Dir))
    );
    assert!(store.readdir(1).await.unwrap().is_empty());
}

#[tokio::test]
#[ignore = "requires a running TiKV/PD cluster; set SLAYERFS_TIKV_PD_ENDPOINTS"]
async fn tikv_transactional_namespace_schema() {
    let store = TiKvMetaStore::from_config(integration_config("namespace"))
        .await
        .expect("tikv store should connect");
    store.initialize().await.unwrap();
    let root = store.root_ino();

    let dir = store.mkdir(root, "dir".to_string()).await.unwrap();
    let file = store.create_file(dir, "file".to_string()).await.unwrap();

    assert_eq!(store.lookup(root, "dir").await.unwrap(), Some(dir));
    assert_eq!(store.lookup(dir, "file").await.unwrap(), Some(file));
    assert_eq!(
        store.lookup_path("/dir/file").await.unwrap(),
        Some((file, FileType::File))
    );

    let file_attr = store.stat(file).await.unwrap().unwrap();
    assert_eq!(file_attr.kind, FileType::File);
    assert_eq!(file_attr.size, 0);

    let entries = store.readdir(dir).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "file");
    assert_eq!(
        store.get_names(file).await.unwrap(),
        vec![(Some(dir), "file".to_string())]
    );
    assert_eq!(
        store.get_paths(file).await.unwrap(),
        vec!["/dir/file".to_string()]
    );

    store
        .rename(dir, "file", root, "moved".to_string())
        .await
        .unwrap();
    assert_eq!(store.lookup(dir, "file").await.unwrap(), None);
    assert_eq!(store.lookup(root, "moved").await.unwrap(), Some(file));
    assert_eq!(
        store.get_paths(file).await.unwrap(),
        vec!["/moved".to_string()]
    );

    store.unlink(root, "moved").await.unwrap();
    assert!(store.stat(file).await.unwrap().is_none());
}

#[tokio::test]
#[ignore = "requires a running TiKV/PD cluster; set SLAYERFS_TIKV_PD_ENDPOINTS"]
async fn tikv_rejects_non_empty_rmdir() {
    let store = TiKvMetaStore::from_config(integration_config("rmdir"))
        .await
        .expect("tikv store should connect");
    store.initialize().await.unwrap();
    let root = store.root_ino();

    let dir = store.mkdir(root, "dir".to_string()).await.unwrap();
    store.create_file(dir, "file".to_string()).await.unwrap();

    let err = store
        .rmdir(root, "dir")
        .await
        .expect_err("non-empty directory should not be removed");
    assert!(matches!(err, MetaError::DirectoryNotEmpty(ino) if ino == dir));

    store.unlink(dir, "file").await.unwrap();
    store.rmdir(root, "dir").await.unwrap();
    assert_eq!(store.lookup(root, "dir").await.unwrap(), None);
}

#[tokio::test]
#[ignore = "requires a running TiKV/PD cluster; set SLAYERFS_TIKV_PD_ENDPOINTS"]
async fn tikv_transactional_file_data_schema() {
    let store = TiKvMetaStore::from_config(integration_config("file-data"))
        .await
        .expect("tikv store should connect");
    store.initialize().await.unwrap();
    let root = store.root_ino();
    let file = store.create_file(root, "file".to_string()).await.unwrap();
    let first = SliceDesc {
        slice_id: 7,
        chunk_id: 42,
        offset: 0,
        length: 128,
    };
    let second = SliceDesc {
        slice_id: 8,
        chunk_id: 42,
        offset: 128,
        length: 64,
    };

    store.write(file, 42, first, 128).await.unwrap();
    store.append_slice(42, second).await.unwrap();

    assert_eq!(store.get_slices(42).await.unwrap(), vec![first, second]);
    assert_eq!(store.list_chunk_ids(10).await.unwrap(), vec![42]);
    assert_eq!(store.stat(file).await.unwrap().unwrap().size, 128);

    store
        .write(
            file,
            43,
            SliceDesc {
                slice_id: 9,
                chunk_id: 43,
                offset: 0,
                length: 32,
            },
            32,
        )
        .await
        .unwrap();
    assert_eq!(
        store.stat(file).await.unwrap().unwrap().size,
        128,
        "write should not shrink file size when commits arrive out of order"
    );

    store.extend_file_size(file, 256).await.unwrap();
    assert_eq!(store.stat(file).await.unwrap().unwrap().size, 256);
    store.extend_file_size(file, 64).await.unwrap();
    assert_eq!(
        store.stat(file).await.unwrap().unwrap().size,
        256,
        "extend_file_size should be monotonic"
    );

    store.set_file_size(file, 64).await.unwrap();
    assert_eq!(store.stat(file).await.unwrap().unwrap().size, 64);
}

#[tokio::test]
#[ignore = "requires a running TiKV/PD cluster; set SLAYERFS_TIKV_PD_ENDPOINTS"]
async fn tikv_allocates_generic_counters_transactionally() {
    let store = TiKvMetaStore::from_config(integration_config("counters"))
        .await
        .expect("tikv store should connect");
    store.initialize().await.unwrap();

    assert_eq!(store.next_id("custom").await.unwrap(), 1);
    assert_eq!(store.next_id("custom").await.unwrap(), 2);
}
