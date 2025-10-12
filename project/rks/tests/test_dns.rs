use rks::api::xlinestore::XlineStore;
use rks::dns::authority::run_dns_server;
use rks::protocol::config::load_config;
use std::{fs, sync::Arc};

use env_logger;
use log::{LevelFilter, info};
use once_cell::sync::OnceCell;

fn init_logger() {
    static LOGGER: OnceCell<()> = OnceCell::new();
    LOGGER.get_or_init(|| {
        env_logger::builder()
            .is_test(false)
            .filter_level(LevelFilter::Debug)
            .try_init()
            .ok();
    });
}

async fn load_store() -> Arc<XlineStore> {
    let config_path = std::env::var("TEST_CONFIG_PATH").unwrap_or_else(|_| {
        format!(
            "{}/tests/config.yaml",
            std::env::var("CARGO_MANIFEST_DIR").unwrap()
        )
    });
    let config = load_config(&config_path).expect("Failed to load config");
    let endpoints: Vec<&str> = config
        .xline_config
        .endpoints
        .iter()
        .map(|s| s.as_str())
        .collect();
    Arc::new(
        XlineStore::new(&endpoints)
            .await
            .expect("connect xline failed"),
    )
}

#[tokio::test]
async fn test_run_dns_server_startup() {
    init_logger();

    let store = load_store().await;

    let file_path = "/home/tcy/project/rk8s/project/rks/tests/test-pod.yaml";
    let pod_yaml = fs::read_to_string(file_path).expect("open error");
    store
        .insert_pod_yaml("test-pod1", &pod_yaml)
        .await
        .expect("insert error");
    let pods = match store.list_pods().await {
        Ok(pods) => pods,
        Err(e) => panic!("Failed to list pods: {:?}", e),
    };

    info!("test get pods: {pods:?}");
    let handle = tokio::spawn(async move {
        let _ = run_dns_server(store, 5300).await;
    });

    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen for ctrl_c");

    handle.abort();
}
