//! Metadata store factory
//!
//! Creates appropriate MetaStore implementation based on configuration

use std::path::Path;
use std::sync::Arc;

use crate::meta::client::{MetaClient, MetaClientOptions};
use crate::meta::config::{CacheTtl, Config, DatabaseConfig, DatabaseType};
use crate::meta::layer::MetaLayer;
use crate::meta::store::{MetaError, MetaStore};
use crate::meta::stores::DatabaseMetaStore;
use crate::meta::stores::EtcdMetaStore;

/// Combined handles for raw stores and cached meta layers.
#[derive(Clone)]
pub struct MetaHandle {
    store: Arc<dyn MetaStore>,
    layer: Arc<dyn MetaLayer>,
}

impl MetaHandle {
    pub fn store(&self) -> Arc<dyn MetaStore> {
        Arc::clone(&self.store)
    }

    pub fn layer(&self) -> Arc<dyn MetaLayer> {
        Arc::clone(&self.layer)
    }
}

/// Factory for creating metadata handles (raw store + cached layer)
pub struct MetaStoreFactory;

impl MetaStoreFactory {
    /// Create MetaStore from path (with MetaClient caching)
    #[allow(dead_code)]
    pub async fn create_from_path(backend_path: &Path) -> Result<MetaHandle, MetaError> {
        let config =
            Config::from_path(backend_path).map_err(|e| MetaError::Config(e.to_string()))?;
        Self::create_from_config(config).await
    }

    /// Create MetaStore from config (with MetaClient caching)
    ///
    /// - SQLite: 10s TTL (configurable)
    /// - PostgreSQL: 500ms TTL (configurable)
    /// - Etcd: 100ms TTL (configurable)
    pub async fn create_from_config(config: Config) -> Result<MetaHandle, MetaError> {
        // Validate cache configuration
        config
            .cache
            .validate()
            .map_err(|e| MetaError::Config(format!("Invalid cache config: {}", e)))?;

        let backend_type = match &config.database.db_config {
            DatabaseType::Sqlite { .. } => "sqlite",
            DatabaseType::Postgres { .. } => "postgres",
            DatabaseType::Etcd { .. } => "etcd",
        };

        // Use TTL from config, or backend-specific defaults if not specified
        let ttl = if config.cache.ttl.is_zero() {
            CacheTtl::for_backend(backend_type)
        } else {
            config.cache.ttl.clone()
        };

        // Create MetaClient with configured capacity and TTL
        let default_options = MetaClientOptions::default();
        let client_options = MetaClientOptions {
            read_only: config.client.read_only,
            no_background_jobs: config.client.no_background_jobs,
            case_insensitive: config.client.case_insensitive,
            session_heartbeat: config
                .client
                .session_heartbeat
                .unwrap_or(default_options.session_heartbeat),
            ..default_options
        };

        let capacity = config.cache.capacity.clone();

        let (store, layer): (Arc<dyn MetaStore>, Arc<dyn MetaLayer>) =
            match &config.database.db_config {
                DatabaseType::Sqlite { .. } | DatabaseType::Postgres { .. } => {
                    let store = Arc::new(DatabaseMetaStore::from_config(config.clone()).await?);
                    let client = MetaClient::with_options(
                        Arc::clone(&store),
                        capacity.clone(),
                        ttl.clone(),
                        client_options.clone(),
                    );
                    let store_dyn: Arc<dyn MetaStore> = store;
                    let layer_dyn: Arc<dyn MetaLayer> = client;
                    (store_dyn, layer_dyn)
                }
                DatabaseType::Etcd { .. } => {
                    let store = Arc::new(EtcdMetaStore::from_config(config.clone()).await?);
                    let client =
                        MetaClient::with_options(Arc::clone(&store), capacity, ttl, client_options);
                    let store_dyn: Arc<dyn MetaStore> = store;
                    let layer_dyn: Arc<dyn MetaLayer> = client;
                    (store_dyn, layer_dyn)
                }
            };

        layer.initialize().await?;

        Ok(MetaHandle { store, layer })
    }

    /// Create raw MetaStore without caching
    pub async fn create_raw_from_config(config: Config) -> Result<Arc<dyn MetaStore>, MetaError> {
        match &config.database.db_config {
            DatabaseType::Sqlite { .. } | DatabaseType::Postgres { .. } => {
                let store = DatabaseMetaStore::from_config(config).await?;
                Ok(Arc::new(store))
            }
            DatabaseType::Etcd { .. } => {
                let store = EtcdMetaStore::from_config(config).await?;
                Ok(Arc::new(store))
            }
        }
    }

    /// Create MetaStore from URL (simplified interface, with caching)
    pub async fn create_from_url(url: &str) -> Result<MetaHandle, MetaError> {
        let config = Self::config_from_url(url)?;
        Self::create_from_config(config).await
    }

    /// Create raw MetaStore from URL (without caching)
    #[allow(dead_code)]
    pub async fn create_raw_from_url(url: &str) -> Result<Arc<dyn MetaStore>, MetaError> {
        let config = Self::config_from_url(url)?;
        Self::create_raw_from_config(config).await
    }

    /// Parse URL to config
    fn config_from_url(url: &str) -> Result<Config, MetaError> {
        let db_config = if url.starts_with("sqlite:") {
            DatabaseType::Sqlite {
                url: url.to_string(),
            }
        } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            DatabaseType::Postgres {
                url: url.to_string(),
            }
        } else if url.starts_with("etcd://")
            || url.starts_with("http://")
            || url.starts_with("https://")
        {
            // For etcd, support comma-separated URLs
            let urls: Vec<String> = if url.contains(',') {
                url.split(',').map(|s| s.trim().to_string()).collect()
            } else {
                vec![url.to_string()]
            };
            DatabaseType::Etcd { urls }
        } else {
            return Err(MetaError::Config(format!(
                "Unsupported URL scheme: {}",
                url
            )));
        };

        Ok(Config {
            database: DatabaseConfig { db_config },
            cache: Default::default(), // Use default cache configuration
            client: Default::default(),
        })
    }
}

/// Convenience function to create MetaStore from path
#[allow(dead_code)]
pub async fn create_meta_store(backend_path: &Path) -> Result<MetaHandle, MetaError> {
    MetaStoreFactory::create_from_path(backend_path).await
}

/// Convenience function to create MetaStore from URL
pub async fn create_meta_store_from_url(url: &str) -> Result<MetaHandle, MetaError> {
    MetaStoreFactory::create_from_url(url).await
}
