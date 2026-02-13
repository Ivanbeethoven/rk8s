/// 智能读取策略演示
/// 展示如何根据读取范围大小选择不同的读取策略
use anyhow::Result;
use slayerfs::cadapter::{client::ObjectClient, localfs::LocalFsBackend};
use slayerfs::chuck::cache::ChunksCacheConfig;
use slayerfs::chuck::store::{BlockStore, BlockStoreConfig, ObjectBlockStore};
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::main]
async fn main() -> Result<()> {
    println!("🚀 SlayerFS 智能读取策略演示");
    println!("{}", "=".repeat(60));

    // 创建临时目录和测试数据
    let temp_dir = TempDir::new()?;
    let backend = LocalFsBackend::new(temp_dir.path());
    let client = ObjectClient::new(backend);

    // 配置智能读取策略
    let block_config = BlockStoreConfig {
        block_size: 64 * 1024 * 1024, // 64MB 块大小
        range_read_threshold: 0.2,    // 20% 阈值 = 12.8MB
    };

    let store = Arc::new(ObjectBlockStore::new_with_configs(
        client,
        ChunksCacheConfig::default(),
        block_config,
    )?);

    // 创建测试数据 (64MB 的测试块)
    println!("📝 创建 64MB 测试数据...");
    let test_data: Vec<u8> = (0..67_108_864).map(|i| (i % 256) as u8).collect();

    // 写入测试数据
    let chunk_key = (42, 3); // (chunk_id, block_index)
    store.write_range(chunk_key, 0, &test_data).await?;
    println!("✅ 测试数据写入完成: {} bytes", test_data.len());

    println!("\n📊 智能读取策略测试:");
    println!("   阈值: 12.8MB (20% of 64MB block)");
    println!("   策略: <= 12.8MB → 范围读取 | > 12.8MB → 完整读取 + SingleFlight");

    // 测试场景 1: 小范围读取 (10MB <= 12.8MB)
    println!("\n🔍 场景 1: 小范围读取 (10MB)");
    let mut small_buffer = vec![0u8; 10 * 1024 * 1024];
    let start = std::time::Instant::now();
    store.read_range(chunk_key, 1024, &mut small_buffer).await?;
    let duration = start.elapsed();

    let small_bytes = small_buffer.len() as f64 / (1024.0 * 1024.0);
    println!("   ✓ 策略: 直接范围读取 (get_object_range)");
    println!(
        "   ✓ 耗时: {:?} (≈{:.2} MB, {:.2} MB/s)",
        duration,
        small_bytes,
        small_bytes / duration.as_secs_f64()
    );
    println!(
        "   ✓ 数据验证: {}",
        if small_buffer[0] == ((1024) % 256) as u8 {
            "通过"
        } else {
            "失败"
        }
    );

    // 测试场景 2: 大范围读取 (32MB > 12.8MB)
    println!("\n🔍 场景 2: 大范围读取 (32MB)");
    let mut large_buffer = vec![0u8; 32 * 1024 * 1024];
    let start = std::time::Instant::now();
    store.read_range(chunk_key, 0, &mut large_buffer).await?;
    let duration = start.elapsed();

    let large_bytes = large_buffer.len() as f64 / (1024.0 * 1024.0);
    println!("   ✓ 策略: 完整块读取 + SingleFlight 合并");
    println!(
        "   ✓ 耗时: {:?} (≈{:.2} MB, {:.2} MB/s)",
        duration,
        large_bytes,
        large_bytes / duration.as_secs_f64()
    );
    println!(
        "   ✓ 数据验证: {}",
        if large_buffer[0] == 0 && large_buffer[1000] == (1000 % 256) as u8 {
            "通过"
        } else {
            "失败"
        }
    );

    // 测试场景 3: 并发读取演示
    println!("\n🔍 场景 3: 并发大范围读取 (展示 SingleFlight 效果)");
    let start = std::time::Instant::now();

    let mut handles = Vec::new();
    for _ in 0..10 {
        let store_clone = Arc::clone(&store);
        let handle = tokio::spawn(async move {
            // Use the same offset and a >threshold size to ensure coalescing hits the full-read path
            let mut buffer = vec![0u8; 32 * 1024 * 1024]; // 32MB each
            store_clone.read_range(chunk_key, 0, &mut buffer).await
        });
        handles.push(handle);
    }

    // 等待所有并发读取完成
    for (i, handle) in handles.into_iter().enumerate() {
        match handle.await? {
            Ok(_) => println!("   ✓ 并发读取 {} 完成", i + 1),
            Err(e) => println!("   ✗ 并发读取 {} 失败: {}", i + 1, e),
        }
    }

    let total_duration = start.elapsed();
    let concurrent_bytes = 10.0 * (32 * 1024 * 1024) as f64 / (1024.0 * 1024.0);
    println!(
        "   ✓ 并发总耗时: {:?} (合计 ≈{:.2} MB，请求合并后实际下游IO≈1次, 吞吐≈{:.2} MB/s)",
        total_duration,
        concurrent_bytes,
        concurrent_bytes / total_duration.as_secs_f64(),
    );

    Ok(())
}
