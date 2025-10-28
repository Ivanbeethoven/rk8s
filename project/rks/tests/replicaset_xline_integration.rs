use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

use common::{
    ContainerSpec, LabelSelector, ObjectMeta, PodSpec, PodTemplateSpec, ReplicaSet, ReplicaSetSpec,
};
use rks::api::xlinestore::XlineStore;
use rks::controllers::{ControllerManager, ReplicaSetController};
use serial_test::serial;

#[derive(Deserialize)]
struct TestCfg {
    xline_config: XlineCfg,
}

#[derive(Deserialize)]
struct XlineCfg {
    endpoints: Vec<String>,
}

fn load_test_config() -> Result<TestCfg> {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let path = std::path::Path::new(manifest).join("tests/config.yaml");
    let s = std::fs::read_to_string(path)?;
    let cfg: TestCfg = serde_yaml::from_str(&s)?;
    Ok(cfg)
}

async fn setup_store_and_manager() -> Result<(
    Arc<XlineStore>,
    Arc<ControllerManager>,
    Arc<ReplicaSetController>,
)> {
    let cfg = load_test_config()?;
    let endpoints: Vec<String> = cfg.xline_config.endpoints;
    let endpoint_refs: Vec<&str> = endpoints.iter().map(|s| s.as_str()).collect();
    let store: Arc<XlineStore> = Arc::new(XlineStore::new(&endpoint_refs).await?);
    let mgr = Arc::new(ControllerManager::new());
    let rs_ctrl = Arc::new(ReplicaSetController::new());
    mgr.clone()
        .register(rs_ctrl.clone(), 2, store.clone())
        .await?;
    mgr.clone().start_watch(store.clone()).await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok((store, mgr, rs_ctrl))
}

fn make_test_replicaset(name: &str, replicas: i32) -> ReplicaSet {
    let mut labels = HashMap::new();
    labels.insert("app".to_string(), "test".to_string());
    ReplicaSet {
        api_version: "v1".to_string(),
        kind: "ReplicaSet".to_string(),
        metadata: ObjectMeta {
            name: name.to_string(),
            namespace: "default".to_string(),
            labels: HashMap::new(),
            annotations: HashMap::new(),
            uid: None,
        },
        spec: ReplicaSetSpec {
            replicas,
            selector: LabelSelector {
                match_labels: labels.clone(),
                match_expressions: Vec::new(),
            },
            template: PodTemplateSpec {
                metadata: ObjectMeta {
                    name: format!("{}-pod-template", name),
                    namespace: "default".to_string(),
                    labels: labels.clone(),
                    annotations: HashMap::new(),
                    uid: None,
                },
                spec: PodSpec {
                    node_name: None,
                    containers: vec![ContainerSpec {
                        name: "c".to_string(),
                        image: "busybox:latest".to_string(),
                        ports: Vec::new(),
                        args: Vec::new(),
                        resources: None,
                    }],
                    init_containers: Vec::new(),
                    tolerations: Vec::new(),
                },
            },
        },
        status: Default::default(),
    }
}

/// Helper to clean up all pods whose name contains the given prefix.
async fn cleanup_pods_by_prefix(store: &Arc<XlineStore>, prefix: &str) -> Result<()> {
    let pods = store.list_pods().await?;
    for p in pods {
        if p.metadata.name.contains(prefix) {
            let _ = store.delete_pod(&p.metadata.name).await;
        }
    }
    Ok(())
}

/// Ensures that creating a ReplicaSet automatically creates the desired number of Pods.
#[serial]
#[tokio::test]
async fn test_replicaset_creates_pods() -> Result<()> {
    let (store, _mgr, _rs_ctrl) = setup_store_and_manager().await?;
    let rs = make_test_replicaset("test-rs-create", 3);
    let yaml = serde_yaml::to_string(&rs)?;
    store
        .insert_replicaset_yaml(&rs.metadata.name, &yaml)
        .await?;
    sleep(Duration::from_secs(3)).await;

    let pods = store.list_pods().await?;
    let found = pods.iter().any(|p| {
        p.metadata
            .labels
            .get("app")
            .map(|v| v == "test")
            .unwrap_or(false)
    });

    // cleanup
    let _ = store.delete_replicaset(&rs.metadata.name).await;
    let _ = cleanup_pods_by_prefix(&store, "test-rs-create").await;

    assert!(
        found,
        "replicaset controller should create pods for the replicaset"
    );
    Ok(())
}

/// Ensures that when a Pod managed by a ReplicaSet is deleted, the controller recreates it.
#[serial]
#[tokio::test]
async fn test_replicaset_recreates_after_delete() -> Result<()> {
    let (store, _mgr, _rs_ctrl) = setup_store_and_manager().await?;
    let rs = make_test_replicaset("test-rs-recreate", 3);
    let yaml = serde_yaml::to_string(&rs)?;
    store
        .insert_replicaset_yaml(&rs.metadata.name, &yaml)
        .await?;
    sleep(Duration::from_secs(3)).await;

    let pods = store.list_pods().await?;
    let matching: Vec<_> = pods
        .into_iter()
        .filter(|p| {
            p.metadata
                .labels
                .get("app")
                .map(|v| v == "test")
                .unwrap_or(false)
        })
        .collect();
    assert!(!matching.is_empty(), "setup should have at least one pod");

    for p in matching.iter() {
        let name = p.metadata.name.clone();
        store.delete_pod(&name).await?;
    }

    sleep(Duration::from_secs(4)).await;

    let pods_after = store.list_pods().await?;
    let found_after = pods_after.iter().any(|p| {
        p.metadata
            .labels
            .get("app")
            .map(|v| v == "test")
            .unwrap_or(false)
    });

    // cleanup
    let _ = store.delete_replicaset(&rs.metadata.name).await;
    let _ = cleanup_pods_by_prefix(&store, "test-rs-recreate").await;

    assert!(
        found_after,
        "replicaset controller should have recreated a pod after deletion"
    );
    Ok(())
}

/// Ensures that scaling down the ReplicaSet (reducing replicas) correctly deletes extra Pods.
#[serial]
#[tokio::test]
async fn test_replicaset_scales_down() -> Result<()> {
    let (store, _mgr, _rs_ctrl) = setup_store_and_manager().await?;
    let rs = make_test_replicaset("test-rs-scale-down", 3);
    let yaml = serde_yaml::to_string(&rs)?;
    store
        .insert_replicaset_yaml(&rs.metadata.name, &yaml)
        .await?;
    sleep(Duration::from_secs(3)).await;

    let mut updated_rs = rs.clone();
    updated_rs.spec.replicas = 1;
    let yaml = serde_yaml::to_string(&updated_rs)?;
    store
        .insert_replicaset_yaml(&updated_rs.metadata.name, &yaml)
        .await?;

    sleep(Duration::from_secs(3)).await;

    let pods = store.list_pods().await?;
    let matching: Vec<_> = pods
        .into_iter()
        .filter(|p| p.metadata.name.contains("test-rs-scale-down"))
        .collect();

    // cleanup
    let _ = store.delete_replicaset(&rs.metadata.name).await;
    let _ = cleanup_pods_by_prefix(&store, "test-rs-scale-down").await;

    assert_eq!(
        matching.len(),
        1,
        "replicaset controller should scale down pods"
    );
    Ok(())
}

/// Ensures that scaling up the ReplicaSet creates additional Pods to meet the new desired replicas.
#[serial]
#[tokio::test]
async fn test_replicaset_scales_up() -> Result<()> {
    let (store, _mgr, _rs_ctrl) = setup_store_and_manager().await?;
    let rs = make_test_replicaset("test-rs-scale-up", 1);
    let yaml = serde_yaml::to_string(&rs)?;
    store
        .insert_replicaset_yaml(&rs.metadata.name, &yaml)
        .await?;
    sleep(Duration::from_secs(3)).await;

    let mut updated_rs = rs.clone();
    updated_rs.spec.replicas = 3;
    let yaml = serde_yaml::to_string(&updated_rs)?;
    store
        .insert_replicaset_yaml(&updated_rs.metadata.name, &yaml)
        .await?;

    sleep(Duration::from_secs(4)).await;

    let pods = store.list_pods().await?;
    let matching: Vec<_> = pods
        .into_iter()
        .filter(|p| p.metadata.name.contains("test-rs-scale-up"))
        .collect();

    // cleanup
    let _ = store.delete_replicaset(&rs.metadata.name).await;
    let _ = cleanup_pods_by_prefix(&store, "test-rs-scale-up").await;

    assert_eq!(matching.len(), 3, "replicaset should scale up to 3 pods");
    Ok(())
}

/// Ensures that two ReplicaSets with different label selectors manage only their own Pods.
#[serial]
#[tokio::test]
async fn test_two_replicasets_independent() -> Result<()> {
    let (store, _mgr, _rs_ctrl) = setup_store_and_manager().await?;

    let mut rs_a = make_test_replicaset("rs-a", 2);
    rs_a.spec
        .selector
        .match_labels
        .insert("tier".to_string(), "frontend".to_string());
    rs_a.spec
        .template
        .metadata
        .labels
        .insert("tier".to_string(), "frontend".to_string());
    let yaml_a = serde_yaml::to_string(&rs_a)?;
    store
        .insert_replicaset_yaml(&rs_a.metadata.name, &yaml_a)
        .await?;

    let mut rs_b = make_test_replicaset("rs-b", 1);
    rs_b.spec
        .selector
        .match_labels
        .insert("tier".to_string(), "backend".to_string());
    rs_b.spec
        .template
        .metadata
        .labels
        .insert("tier".to_string(), "backend".to_string());
    let yaml_b = serde_yaml::to_string(&rs_b)?;
    store
        .insert_replicaset_yaml(&rs_b.metadata.name, &yaml_b)
        .await?;

    sleep(Duration::from_secs(5)).await;

    let pods = store.list_pods().await?;
    let pods_a: Vec<_> = pods
        .iter()
        .filter(|p| p.metadata.name.contains("rs-a"))
        .collect();
    let pods_b: Vec<_> = pods
        .iter()
        .filter(|p| p.metadata.name.contains("rs-b"))
        .collect();

    // cleanup
    let _ = store.delete_replicaset(&rs_a.metadata.name).await;
    let _ = store.delete_replicaset(&rs_b.metadata.name).await;
    let _ = cleanup_pods_by_prefix(&store, "rs-a").await;
    let _ = cleanup_pods_by_prefix(&store, "rs-b").await;

    assert_eq!(pods_a.len(), 2, "RS A should manage 2 pods");
    assert_eq!(pods_b.len(), 1, "RS B should manage 1 pod");
    Ok(())
}
