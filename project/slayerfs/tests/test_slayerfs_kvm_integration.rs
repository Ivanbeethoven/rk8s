use std::path::{Path, PathBuf};

use std::sync::Once;
use std::time::Duration;

use anyhow::{Context, Result};
use qlean::{Distro, Machine, MachineConfig, create_image, with_machine};
use tracing_subscriber::EnvFilter;

const SLAYERFS_BIN_IN_VM: &str = "/usr/local/bin/slayerfs";
const SLAYERFS_MOUNTPOINT: &str = "/mnt/slayerfs";
const SLAYERFS_DATA_DIR: &str = "/tmp/slayerfs-data";
const SLAYERFS_LOG_PATH: &str = "/var/log/slayerfs.log";
const SLAYERFS_META_DIR: &str = "/tmp/slayerfs-meta";
const XFSTESTS_STAGE_DIR: &str = "/opt/slayerfs-xfstests";
const XFSTESTS_SCRIPT_IN_VM: &str = "/opt/slayerfs-xfstests/xfstests_slayer.sh";
const XFSTESTS_BIN_IN_VM: &str = "/opt/slayerfs-xfstests/persistence_demo";
const XFSTESTS_EXCLUDE_IN_VM: &str = "/opt/slayerfs-xfstests/xfstests_slayer.exclude";
const XFSTESTS_REMOTE_DIR: &str = "/tmp/xfstests-dev";
const XFSTESTS_HOST_ARTIFACT_ROOT: &str = "/tmp/slayerfs-kvm-xfstests";

// ---------------------------------------------------------------------------
// Meta-backend abstraction (mirrors the multinode test pattern)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetaBackend {
    Sqlite,
    Etcd,
    Redis,
}

impl MetaBackend {
    fn as_str(self) -> &'static str {
        match self {
            MetaBackend::Sqlite => "sqlite",
            MetaBackend::Etcd => "etcd",
            MetaBackend::Redis => "redis",
        }
    }

    /// CLI value for `--meta-backend`.
    fn cli_backend(self) -> &'static str {
        match self {
            MetaBackend::Sqlite => "sqlx",
            MetaBackend::Etcd => "etcd",
            MetaBackend::Redis => "redis",
        }
    }

    /// Return the config YAML filename shipped from the repo root.
    fn config_filename(self) -> &'static str {
        match self {
            MetaBackend::Sqlite => "slayerfs-sqlite.yml",
            MetaBackend::Etcd => "slayerfs-etcd.yml",
            MetaBackend::Redis => "slayerfs-redis.yml",
        }
    }

    /// Path to the config file once uploaded into the xfstests staging dir.
    fn xfstests_config_in_vm(self) -> String {
        format!("{}/{}", XFSTESTS_STAGE_DIR, self.config_filename())
    }
}

/// Build meta-URL(s) for the given backend, referencing the host gateway IP.
fn backend_meta_url(backend: MetaBackend, gateway: &str) -> String {
    match backend {
        MetaBackend::Sqlite => {
            // SQLite is local to the VM — no gateway needed.
            "sqlite:///tmp/slayerfs-db/metadata.db".to_string()
        }
        MetaBackend::Etcd => format!("http://{}:2379", gateway),
        MetaBackend::Redis => format!("redis://{}:6379/0", gateway),
    }
}

async fn default_gateway_ip(vm: &mut Machine) -> Result<Option<String>> {
    let out = exec_check(
        vm,
        r#"sh -lc "ip route | awk '/default/ {print \$3; exit}'""#,
    )
    .await?;
    let ip = out.trim();
    if ip.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ip.to_string()))
    }
}

fn guess_gateway_from_ip(ip: &str) -> Option<String> {
    let parts: Vec<&str> = ip.trim().split('.').collect();
    if parts.len() != 4 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    Some(format!("{}.{}.{}.1", parts[0], parts[1], parts[2]))
}

async fn detect_gateway(vm: &mut Machine) -> Result<String> {
    let gw = default_gateway_ip(vm).await?;
    if let Some(ip) = gw {
        return Ok(ip);
    }
    // Fallback: derive from VM's own IP
    let vm_ip = exec_check(vm, "sh -lc \"hostname -I | awk '{print $1}'\"").await?;
    let vm_ip = vm_ip.trim();
    guess_gateway_from_ip(vm_ip)
        .ok_or_else(|| anyhow::anyhow!("cannot detect host gateway IP from VM IP '{}'", vm_ip))
}

/// Clean up etcd metadata before a new test run (requires etcdctl in the VM
/// or we use the bundled approach via curl).
async fn cleanup_etcd_metadata(vm: &mut Machine, gateway: &str) -> Result<()> {
    // Install etcdctl if not available (lightweight static binary).
    let has_etcdctl = exec_check(
        vm,
        "command -v etcdctl >/dev/null 2>&1 && echo OK || echo NO",
    )
    .await
    .map(|s| s.trim() == "OK")
    .unwrap_or(false);

    let endpoint = format!("http://{}:2379", gateway);

    if has_etcdctl {
        let prefixes = [
            "slayerfs:",
            "f:",
            "r:",
            "c:",
            "p:",
            "l:",
            "session:",
            "session_info:",
            "slices/",
        ];
        for prefix in prefixes {
            let cmd = format!(
                "sh -lc 'ETCDCTL_API=3 etcdctl --endpoints={endpoint} del --prefix {prefix}'"
            );
            let _ = exec_check(vm, &cmd).await;
        }
    } else {
        // Use etcd's HTTP API to delete all keys (no etcdctl needed).
        // The v3 gRPC-gateway range_end="\0" deletes everything.
        let cmd = format!(
            r#"curl -s -X POST {endpoint}/v3/kv/deleterange -d '{{"key":"AA==","range_end":"AA=="}}' || true"#,
        );
        let _ = exec_check(vm, &cmd).await;
    }
    Ok(())
}

/// Clean up redis metadata before a new test run.
async fn cleanup_redis_metadata(vm: &mut Machine, gateway: &str) -> Result<()> {
    // Try redis-cli if available, otherwise use raw TCP.
    let has_redis_cli = exec_check(
        vm,
        "command -v redis-cli >/dev/null 2>&1 && echo OK || echo NO",
    )
    .await
    .map(|s| s.trim() == "OK")
    .unwrap_or(false);

    if has_redis_cli {
        let cmd = format!("sh -lc 'redis-cli -h {gateway} -p 6379 FLUSHALL'");
        exec_check(vm, &cmd).await?;
    } else {
        // Minimal FLUSHALL via raw TCP (no redis-cli needed).
        let cmd = format!(
            "sh -lc 'printf \"*1\\r\\n\\$8\\r\\nFLUSHALL\\r\\n\" | nc -q1 {gateway} 6379 || true'"
        );
        let _ = exec_check(vm, &cmd).await;
    }
    Ok(())
}

/// Generate a SlayerFS config YAML for the given backend.
/// For SQLite the URL is local; for Redis/Etcd it points to `gateway`.
fn generate_backend_config(backend: MetaBackend, gateway: &str) -> String {
    let db_section = match backend {
        MetaBackend::Sqlite => {
            "database:\n  type: sqlite\n  url: \"sqlite:///tmp/slayerfs/metadata.db\"".to_string()
        }
        MetaBackend::Redis => format!(
            "database:\n  type: redis\n  url: \"redis://{}:6379/0\"",
            gateway
        ),
        MetaBackend::Etcd => format!(
            "database:\n  type: etcd\n  urls:\n    - \"http://{}:2379\"",
            gateway
        ),
    };
    format!(
        "{}\n\ncache:\n  enabled: true\n  capacity:\n    inode: 10000\n    path: 5000\n  ttl:\n    inode_ttl: 10.0\n    path_ttl: 10.0\n",
        db_section
    )
}

// NOTE: These tests use qlean for QEMU-based VM testing.
// qlean 0.2.1+ supports TCG fallback when KVM is unavailable.
fn tracing_subscriber_init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .init();
    });
}

async fn exec_check(vm: &mut Machine, cmd: &str) -> Result<String> {
    let result = vm.exec(cmd).await?;
    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        let stdout = String::from_utf8_lossy(&result.stdout);
        anyhow::bail!(
            "Command '{}' failed with exit code {:?}\nstdout: {}\nstderr: {}",
            cmd,
            result.status.code(),
            stdout,
            stderr
        );
    }
    Ok(String::from_utf8_lossy(&result.stdout).to_string())
}

async fn exec_check_timed(vm: &mut Machine, cmd: &str, timeout: Duration) -> Result<String> {
    let result = tokio::time::timeout(timeout, vm.exec(cmd)).await;
    let result = match result {
        Ok(res) => res?,
        Err(_) => anyhow::bail!("command timed out after {:?}: {}", timeout, cmd),
    };
    if !result.status.success() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        let stdout = String::from_utf8_lossy(&result.stdout);
        anyhow::bail!(
            "Command '{}' failed with exit code {:?}\nstdout: {}\nstderr: {}",
            cmd,
            result.status.code(),
            stdout,
            stderr
        );
    }
    Ok(String::from_utf8_lossy(&result.stdout).to_string())
}

async fn get_file_tail(vm: &mut Machine, path: &str, max_lines: usize) -> Result<String> {
    let out = vm
        .exec(&format!(
            "tail -n {n} {p} 2>/dev/null | tail -c 12000 || true",
            n = max_lines,
            p = path
        ))
        .await?;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

async fn dump_debug_info(vm: &mut Machine) -> Result<String> {
    let mut s = String::new();

    s.push_str("=== mount ===\n");
    s.push_str(&exec_check(vm, "mount | tail -n 50 || true").await?);

    s.push_str("\n=== mountpoint ===\n");
    s.push_str(
        &exec_check(
            vm,
            &format!(
                "mountpoint -q {mp} && echo 'mounted' || echo 'not mounted'",
                mp = SLAYERFS_MOUNTPOINT
            ),
        )
        .await?,
    );
    s.push('\n');

    s.push_str("\n=== ps (slayerfs) ===\n");
    s.push_str(&exec_check(vm, "ps aux | grep '[s]layerfs' || true").await?);

    s.push_str("\n=== logs (slayerfs) ===\n");
    s.push_str(&get_file_tail(vm, SLAYERFS_LOG_PATH, 300).await?);

    s.push_str("\n=== sqlite dir ===\n");
    s.push_str(&exec_check(vm, "ls -la /tmp/slayerfs-db 2>/dev/null || true").await?);
    s.push_str(&exec_check(vm, "stat /tmp/slayerfs-db/metadata.db 2>/dev/null || true").await?);

    s.push_str("\n=== dmesg (tail) ===\n");
    s.push_str(&exec_check(vm, "dmesg -T | tail -n 80 || true").await?);

    Ok(s)
}

fn get_slayerfs_binary_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_slayerfs") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR not set")?;

    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(&manifest_dir)
                .parent()
                .unwrap()
                .join("target")
        });

    let debug_path = target_dir.join("debug/slayerfs");
    if debug_path.exists() {
        return Ok(debug_path);
    }

    let release_path = target_dir.join("release/slayerfs");
    if release_path.exists() {
        return Ok(release_path);
    }

    anyhow::bail!(
        "slayerfs binary not found at {:?} or {:?}. Build it first, e.g. `cargo build -p slayerfs`.",
        debug_path,
        release_path
    );
}

fn get_persistence_demo_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_persistence_demo") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
    }

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR not set")?;
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(&manifest_dir)
                .parent()
                .unwrap()
                .join("target")
        });

    for rel in [
        "release/examples/persistence_demo",
        "debug/examples/persistence_demo",
        "release/persistence_demo",
        "debug/persistence_demo",
    ] {
        let path = target_dir.join(rel);
        if path.exists() {
            return Ok(path);
        }
    }

    anyhow::bail!(
        "persistence_demo binary not found under {:?}. Build it first, e.g. `cargo build -p slayerfs --example persistence_demo --release`.",
        target_dir
    );
}

fn shell_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\"'\"'"))
}

fn xfstests_host_artifact_dir() -> PathBuf {
    let root = std::env::var("SLAYERFS_XFSTESTS_HOST_ARTIFACT_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(XFSTESTS_HOST_ARTIFACT_ROOT));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    root.join(format!(
        "{}-{}-pid{}",
        now.as_secs(),
        now.subsec_nanos(),
        std::process::id()
    ))
}

async fn install_deps(vm: &mut Machine) -> Result<()> {
    // Check if fuse3 (fusermount3) is already installed.
    let fuse_check = vm
        .exec("command -v fusermount3 >/dev/null 2>&1 && echo FUSE_OK || echo FUSE_MISSING")
        .await
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("FUSE_OK"))
        .unwrap_or(false);

    if !fuse_check {
        // Install fusermount3 from the pre-extracted bundle instead of using
        // apt-get.  apt-get + unattended-upgrades exhaust RAM on a 2GB VM,
        // causing the qlean SSH channel to drop (OOM kills sshd).
        // fusermount3 from Debian trixie only dynamically links libc.so.6
        // which is always present in the base image.
        let manifest_dir = PathBuf::from(
            std::env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR not set")?,
        );
        let fuse3_bundle_dir = manifest_dir.join("tests/scripts/fuse3-bundle");
        let fusermount3 = fuse3_bundle_dir.join("fusermount3");
        if fusermount3.exists() {
            vm.upload(&fusermount3, Path::new("/usr/local/bin"))
                .await
                .context("upload bundled fusermount3")?;
            exec_check(vm, "chmod 755 /usr/local/bin/fusermount3").await?;
            tracing::info!("installed fusermount3 from bundle");
        } else {
            tracing::warn!(
                "fuse3 bundle not found at {:?}; falling back to apt-get",
                fuse3_bundle_dir
            );
            // Fall back: kill UA first to reduce OOM risk, then apt-get.
            let _ = exec_check(
                vm,
                "\
                pkill -9 -f 'unattended' 2>/dev/null || true; \
                pkill -9 -x apt-get 2>/dev/null || true",
            )
            .await;
            exec_check(vm, "MALLOC_ARENA_MAX=1 apt-get update -qq").await?;
            exec_check(
                vm,
                "MALLOC_ARENA_MAX=1 DEBIAN_FRONTEND=noninteractive apt-get install -y -qq fuse3 ca-certificates coreutils util-linux procps",
            )
            .await?;
        }
    }

    exec_check(
        vm,
        &format!(
            "mkdir -p {mp} {data}",
            mp = SLAYERFS_MOUNTPOINT,
            data = SLAYERFS_DATA_DIR
        ),
    )
    .await?;

    exec_check(
        vm,
        "(grep -q '^user_allow_other' /etc/fuse.conf 2>/dev/null) || echo 'user_allow_other' >> /etc/fuse.conf",
    )
    .await?;

    Ok(())
}

async fn wait_for_mount(vm: &mut Machine, timeout: Duration) -> Result<()> {
    let start = std::time::Instant::now();
    loop {
        let out = vm
            .exec(&format!(
                "mountpoint -q {mp} && echo OK || echo NO",
                mp = SLAYERFS_MOUNTPOINT
            ))
            .await?;
        let s = String::from_utf8_lossy(&out.stdout);
        if s.trim() == "OK" {
            return Ok(());
        }

        if start.elapsed() > timeout {
            let ps = vm
                .exec("ps aux | grep '[s]layerfs' || true")
                .await
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_else(|e| e.to_string());
            let log = get_file_tail(vm, SLAYERFS_LOG_PATH, 800)
                .await
                .unwrap_or_else(|e| e.to_string());
            anyhow::bail!(
                "timeout waiting for slayerfs mount at {}\n\n=== ps (slayerfs) ===\n{}\n=== logs (slayerfs) ===\n{}",
                SLAYERFS_MOUNTPOINT,
                ps,
                log
            );
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn unmount_best_effort(vm: &mut Machine) -> Result<()> {
    let _ = vm
        .exec(&format!(
            "fusermount3 -u {mp} || umount {mp} || umount -l {mp} || true",
            mp = SLAYERFS_MOUNTPOINT
        ))
        .await;
    Ok(())
}

async fn assert_not_mounted(vm: &mut Machine) -> Result<()> {
    let out = vm
        .exec(&format!(
            "mountpoint -q {mp} && echo 'mounted' || echo 'not mounted'",
            mp = SLAYERFS_MOUNTPOINT
        ))
        .await?;
    let s = String::from_utf8_lossy(&out.stdout);
    if s.trim() != "not mounted" {
        anyhow::bail!("expected mountpoint to be unmounted, but it is still mounted");
    }
    Ok(())
}

async fn run_slayerfs_mount_root(
    vm: &mut Machine,
    backend: MetaBackend,
    meta_url: &str,
) -> Result<()> {
    exec_check(
        vm,
        &format!("rm -f {log} && touch {log}", log = SLAYERFS_LOG_PATH),
    )
    .await?;

    exec_check(
        vm,
        &format!("rm -rf {d} && mkdir -p {d}", d = SLAYERFS_META_DIR),
    )
    .await?;
    exec_check(vm, &format!("chmod 1777 {d}", d = SLAYERFS_META_DIR)).await?;

    exec_check(
        vm,
        &format!(
            "stat -c '%A %U:%G %n' /tmp {d} || true; sh -lc 'touch {d}/.probe_root && rm {d}/.probe_root'",
            d = SLAYERFS_META_DIR
        ),
    )
    .await?;
    exec_check(
        vm,
        &format!(
            "su - tester -c 'sh -lc \"touch {d}/.probe_tester && rm {d}/.probe_tester\"'",
            d = SLAYERFS_META_DIR
        ),
    )
    .await?;

    // For SQLite, ensure the database file exists and is writable.
    if backend == MetaBackend::Sqlite {
        let db_path = meta_url
            .strip_prefix("sqlite:///")
            .or_else(|| meta_url.strip_prefix("sqlite://"))
            .map(|s| s.split('?').next().unwrap_or(s))
            .filter(|s| !s.starts_with(':'))
            .map(|p| {
                if p.starts_with('/') {
                    p.to_string()
                } else {
                    format!("/{p}")
                }
            });
        if let Some(db_path) = db_path {
            exec_check(
                vm,
                &format!(
                    "sh -lc \"test -e '{p}' || touch '{p}'; chmod 666 '{p}'; stat -c '%A %U:%G %n' '{p}'\"",
                    p = db_path
                ),
            )
            .await?;
        }
    }

    // Build the mount command with backend-specific CLI arguments.
    let meta_args = match backend {
        MetaBackend::Sqlite | MetaBackend::Redis => {
            format!(
                "--meta-backend {} --meta-url '{}'",
                backend.cli_backend(),
                meta_url
            )
        }
        MetaBackend::Etcd => {
            format!("--meta-backend etcd --meta-etcd-urls '{}'", meta_url)
        }
    };

    exec_check(
        vm,
        &format!(
            "nohup {bin} mount {mp} --data-dir {data} {meta_args} > {log} 2>&1 &",
            bin = SLAYERFS_BIN_IN_VM,
            mp = SLAYERFS_MOUNTPOINT,
            data = SLAYERFS_DATA_DIR,
            meta_args = meta_args,
            log = SLAYERFS_LOG_PATH
        ),
    )
    .await?;

    wait_for_mount(vm, Duration::from_secs(30)).await?;
    Ok(())
}

async fn basic_fs_checks(vm: &mut Machine) -> Result<()> {
    exec_check(
        vm,
        &format!("sh -lc 'echo hello > {}/a'", SLAYERFS_MOUNTPOINT),
    )
    .await?;
    exec_check(
        vm,
        &format!("sh -lc 'cat {}/a | grep -q hello'", SLAYERFS_MOUNTPOINT),
    )
    .await?;
    exec_check(
        vm,
        &format!("mv {}/a {}/b", SLAYERFS_MOUNTPOINT, SLAYERFS_MOUNTPOINT),
    )
    .await?;
    exec_check(vm, &format!("rm {}/b", SLAYERFS_MOUNTPOINT)).await?;

    exec_check(vm, &format!("mkdir {}/dir", SLAYERFS_MOUNTPOINT)).await?;
    exec_check(vm, &format!("rmdir {}/dir", SLAYERFS_MOUNTPOINT)).await?;

    Ok(())
}

async fn setup_unprivileged_user(vm: &mut Machine, user: &str) -> Result<()> {
    exec_check(
        vm,
        &format!("id -u {user} >/dev/null 2>&1 || useradd -m -s /bin/bash {user}"),
    )
    .await?;
    Ok(())
}

async fn run_posix_edge_cases(vm: &mut Machine) -> Result<()> {
    exec_check(
        vm,
        &format!(
            "mkdir -p {}/edge/dir1 {}/edge/dir2",
            SLAYERFS_MOUNTPOINT, SLAYERFS_MOUNTPOINT
        ),
    )
    .await?;

    exec_check(
        vm,
        &format!(
            "sh -lc 'echo original > {}/edge/dir1/file'",
            SLAYERFS_MOUNTPOINT
        ),
    )
    .await?;

    exec_check(
        vm,
        &format!(
            "ln {}/edge/dir1/file {}/edge/dir1/hardlink",
            SLAYERFS_MOUNTPOINT, SLAYERFS_MOUNTPOINT
        ),
    )
    .await?;
    exec_check(
        vm,
        &format!(
            "sh -lc 'test $(stat -c %h {}/edge/dir1/file) -eq 2'",
            SLAYERFS_MOUNTPOINT
        ),
    )
    .await?;
    exec_check(
        vm,
        &format!(
            "sh -lc 'echo via-link >> {}/edge/dir1/hardlink'",
            SLAYERFS_MOUNTPOINT
        ),
    )
    .await?;
    exec_check(
        vm,
        &format!(
            "sh -lc 'grep -q via-link {}/edge/dir1/file'",
            SLAYERFS_MOUNTPOINT
        ),
    )
    .await?;

    exec_check(
        vm,
        &format!(
            "ln -s {}/edge/dir1/file {}/edge/dir2/symlink",
            SLAYERFS_MOUNTPOINT, SLAYERFS_MOUNTPOINT
        ),
    )
    .await?;
    exec_check(
        vm,
        &format!(
            "sh -lc 'cat {}/edge/dir2/symlink | grep -q original'",
            SLAYERFS_MOUNTPOINT
        ),
    )
    .await?;

    exec_check(
        vm,
        &format!(
            "mv {}/edge/dir1/file {}/edge/dir2/moved",
            SLAYERFS_MOUNTPOINT, SLAYERFS_MOUNTPOINT
        ),
    )
    .await?;
    exec_check(
        vm,
        &format!("test -f {}/edge/dir2/moved", SLAYERFS_MOUNTPOINT),
    )
    .await?;
    exec_check(
        vm,
        &format!("test ! -f {}/edge/dir1/file", SLAYERFS_MOUNTPOINT),
    )
    .await?;

    Ok(())
}

async fn run_concurrency_smoke(vm: &mut Machine) -> Result<()> {
    exec_check(vm, &format!("mkdir -p {}/concurrency", SLAYERFS_MOUNTPOINT)).await?;

    exec_check(
        vm,
        &format!(
            "sh -lc 'set -e; rm -f {}/concurrency/file-*; \
for i in $(seq 1 4); do \
  for j in $(seq 1 50); do echo \"$i-$j\" >> {}/concurrency/file-$i; done; \
done'",
            SLAYERFS_MOUNTPOINT, SLAYERFS_MOUNTPOINT
        ),
    )
    .await?;

    let total1 = exec_check(
        vm,
        &format!("sh -lc 'wc -l {}/concurrency/file-*'", SLAYERFS_MOUNTPOINT),
    )
    .await?;

    let expected_per_file = 50usize;
    let expected_total = 4usize * expected_per_file;

    let mut seen_paths = std::collections::BTreeSet::new();
    let mut total_lines = 0usize;

    for line in total1.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(count) = parts.next() else {
            continue;
        };
        let Some(path) = parts.next() else {
            continue;
        };
        if path == "total" {
            continue;
        }

        let count: usize = count
            .parse()
            .with_context(|| format!("failed to parse per-file wc output: '{}'", line))?;

        if count != expected_per_file {
            anyhow::bail!(
                "unexpected per-file line count for {}: got {}, expected {}\n\nper-file wc output:\n{}",
                path,
                count,
                expected_per_file,
                total1
            );
        }

        if seen_paths.insert(path.to_string()) {
            total_lines += count;
        }
    }

    if seen_paths.len() != 4 {
        anyhow::bail!(
            "unexpected number of concurrency files: got {}, expected 4\n\nper-file wc output:\n{}",
            seen_paths.len(),
            total1
        );
    }

    if total_lines != expected_total {
        anyhow::bail!(
            "unexpected merged line count (sum of unique files): got {}, expected {}\n\nper-file wc output:\n{}",
            total_lines,
            expected_total,
            total1
        );
    }

    Ok(())
}

async fn kill_slayerfs_best_effort(vm: &mut Machine) -> Result<()> {
    let _ = vm.exec("pkill -TERM slayerfs >/dev/null 2>&1 || true; sleep 1; pkill -KILL slayerfs >/dev/null 2>&1 || true").await;
    Ok(())
}

async fn run_in_vm(vm: &mut Machine, slayerfs_bin: &Path, backend: MetaBackend) -> Result<()> {
    let r = run_full(vm, slayerfs_bin, backend).await;

    if let Err(e) = r {
        let dbg = dump_debug_info(vm).await.unwrap_or_else(|e| e.to_string());
        let _ = unmount_best_effort(vm).await;
        anyhow::bail!("{}\n\n{}", e, dbg);
    }

    Ok(())
}

async fn start_vm_and_run(slayerfs_bin: PathBuf, backend: MetaBackend) -> Result<()> {
    tracing_subscriber_init();

    tracing::info!(
        "using slayerfs binary at {:?}, backend={}",
        slayerfs_bin,
        backend.as_str()
    );

    // Use a flag that is set to true only after run_in_vm returns Ok.
    // This lets us distinguish "test passed but shutdown panicked (qlean bug)"
    // from "test panicked during execution (real failure)".
    let run_ok_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag_clone = std::sync::Arc::clone(&run_ok_flag);

    let thread_result = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime");

        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rt.block_on(async move {
                let image = create_image(Distro::Debian, "debian-13-generic-amd64").await?;
                let config = MachineConfig {
                    core: 2,
                    mem: 2048,
                    disk: Some(10),
                    clear: true,
                };
                let slayerfs_bin = std::sync::Arc::new(slayerfs_bin);
                with_machine(&image, &config, move |vm| {
                    let slayerfs_bin = std::sync::Arc::clone(&slayerfs_bin);
                    let flag = std::sync::Arc::clone(&flag_clone);
                    Box::pin(async move {
                        let result = run_in_vm(vm, slayerfs_bin.as_ref(), backend).await;
                        if result.is_ok() {
                            flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                        result
                    })
                })
                .await
            })
        }))
    })
    .join();

    match thread_result {
        // Normal: no panic from with_machine.
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(e))) => Err(e),
        Ok(Err(panic_val)) => {
            // with_machine panicked. Check if run_in_vm completed successfully:
            // if yes, the panic was qlean's shutdown SSH race (known bug) and
            // the test actually passed.
            if run_ok_flag.load(std::sync::atomic::Ordering::SeqCst) {
                tracing::warn!(
                    "with_machine panicked after successful run_in_vm \
                     (qlean shutdown/SSH race); treating as pass"
                );
                Ok(())
            } else {
                let msg = format!("{:?}", panic_val);
                anyhow::bail!("test panicked before run_in_vm could complete: {}", msg)
            }
        }
        Err(thread_err) => anyhow::bail!("test thread error: {:?}", thread_err),
    }
}

async fn start_vm_and_run_xfstests(persistence_bin: PathBuf, backend: MetaBackend) -> Result<()> {
    tracing_subscriber_init();

    tracing::info!(
        "using persistence_demo binary at {:?}, backend={}",
        persistence_bin,
        backend.as_str()
    );

    let artifact_dir_holder: std::sync::Arc<std::sync::Mutex<Option<PathBuf>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let holder_clone = std::sync::Arc::clone(&artifact_dir_holder);

    // Run with_machine in a dedicated OS thread so we can catch the panic it
    // sometimes throws during machine.shutdown() when the SSH channel is closed
    // without an ExitStatus (a qlean bug: `systemctl poweroff` kills sshd
    // before the command sends its ExitStatus back over SSH).
    let thread_result = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime");

        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rt.block_on(async move {
                let image = create_image(Distro::Debian, "debian-13-generic-amd64").await?;
                let config = MachineConfig {
                    core: 2,
                    mem: 8192,
                    disk: Some(30),
                    clear: true,
                };
                let persistence_bin = std::sync::Arc::new(persistence_bin);
                with_machine(&image, &config, move |vm| {
                    let persistence_bin = std::sync::Arc::clone(&persistence_bin);
                    let holder = std::sync::Arc::clone(&holder_clone);
                    Box::pin(async move {
                        let artifact_dir =
                            run_xfstests_in_vm(vm, persistence_bin.as_ref(), backend).await?;
                        tracing::info!("xfstests artifacts stored at {}", artifact_dir.display());
                        *holder.lock().unwrap() = Some(artifact_dir);
                        Ok(())
                    })
                })
                .await
            })
        }))
    })
    .join();

    match thread_result {
        // Thread exited normally (no panic from with_machine).
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(e))) => Err(e),
        Ok(Err(panic_val)) => {
            // with_machine panicked — likely the qlean shutdown SSH race.
            // If xfstests already ran and left artifacts, check their outcome.
            let artifact_dir = artifact_dir_holder.lock().unwrap().clone();
            tracing::warn!(
                "with_machine panicked (qlean shutdown/SSH race during poweroff); \
                 checking artifacts for test outcome"
            );
            if let Some(dir) = artifact_dir {
                check_xfstests_outcome(&dir)
            } else {
                let msg = format!("{:?}", panic_val);
                anyhow::bail!(
                    "with_machine panicked before xfstests artifacts were collected: {}",
                    msg
                )
            }
        }
        Err(thread_err) => {
            anyhow::bail!("test thread panicked: {:?}", thread_err)
        }
    }
}

/// Inspect the xfstests result artifacts to determine pass/fail.
fn check_xfstests_outcome(artifact_dir: &Path) -> Result<()> {
    let artifact_debug = dump_xfstests_artifact_summary(artifact_dir);

    // Look for the check.out file which contains the summary line.
    let check_out = artifact_dir.join("results/check.out");
    if check_out.exists() {
        let content =
            std::fs::read_to_string(&check_out).with_context(|| format!("read {:?}", check_out))?;
        // check.out ends with "Passed all N tests" or similar on success.
        if content.contains("Passed all") {
            tracing::info!(
                "xfstests outcome: PASS (from check.out):\n{}",
                content.trim()
            );
            return Ok(());
        }
        if content.contains("Failures:") || content.contains("failed") {
            anyhow::bail!(
                "xfstests reported failures:\n{}\n\n{}",
                content,
                artifact_debug
            );
        }
    }
    // Fall back to check if the results dir has any .out.bad files (failures).
    let results_dir = artifact_dir.join("results");
    if results_dir.exists() {
        let mut bad = Vec::new();
        collect_files_with_suffix(&results_dir, ".out.bad", &mut bad);
        if bad.is_empty() {
            tracing::info!("xfstests outcome: no .out.bad files found, treating as PASS");
            return Ok(());
        }
        anyhow::bail!(
            "xfstests failed: found {} .out.bad file(s): {:?}\n\n{}",
            bad.len(),
            bad,
            artifact_debug
        );
    }
    anyhow::bail!(
        "xfstests outcome unknown: no check.out or results dir found at {:?}\n\n{}",
        artifact_dir,
        artifact_debug
    )
}

fn tail_text_lines(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    let mut out = lines[start..].join("\n");
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn append_host_file_tail(output: &mut String, title: &str, path: &Path, max_lines: usize) {
    output.push_str("\n=== ");
    output.push_str(title);
    output.push_str(" ===\n");

    match std::fs::read_to_string(path) {
        Ok(content) => output.push_str(&tail_text_lines(&content, max_lines)),
        Err(err) if path.exists() => {
            output.push_str(&format!("failed to read {}: {}\n", path.display(), err));
        }
        Err(_) => {
            output.push_str(&format!("missing: {}\n", path.display()));
        }
    }
}

fn collect_files_with_suffix(root: &Path, suffix: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_with_suffix(&path, suffix, out);
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.ends_with(suffix))
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
}

fn dump_xfstests_artifact_summary(host_dir: &Path) -> String {
    let mut output = String::new();
    output.push_str("=== host xfstests artifacts ===\n");
    output.push_str(&format!("artifact_dir: {}\n", host_dir.display()));

    append_host_file_tail(
        &mut output,
        "host slayerfs log (tail)",
        &host_dir.join("slayerfs.log"),
        300,
    );
    append_host_file_tail(
        &mut output,
        "host xfstests script log (tail)",
        &host_dir.join("xfstests-script.log"),
        300,
    );
    append_host_file_tail(
        &mut output,
        "host xfstests local.config",
        &host_dir.join("local.config"),
        200,
    );
    append_host_file_tail(
        &mut output,
        "host xfstests check.log (tail)",
        &host_dir.join("results/check.log"),
        300,
    );
    append_host_file_tail(
        &mut output,
        "host xfstests check.out (tail)",
        &host_dir.join("results/check.out"),
        300,
    );

    let mut out_bad_files = Vec::new();
    collect_files_with_suffix(&host_dir.join("results"), ".out.bad", &mut out_bad_files);
    out_bad_files.sort();

    output.push_str("\n=== host xfstests .out.bad files ===\n");
    if out_bad_files.is_empty() {
        output.push_str("none\n");
    } else {
        for path in out_bad_files {
            output.push_str(&format!("{}\n", path.display()));
            append_host_file_tail(&mut output, &format!("tail {}", path.display()), &path, 200);
        }
    }

    output
}

async fn run_full(vm: &mut Machine, slayerfs_bin: &Path, backend: MetaBackend) -> Result<()> {
    install_deps(vm).await?;
    setup_unprivileged_user(vm, "tester").await?;
    upload_slayerfs(vm, slayerfs_bin).await?;

    // Determine the meta URL based on the backend.
    let meta_url = match backend {
        MetaBackend::Sqlite => {
            let db_dir = "/tmp/slayerfs-db";
            let db_path = "/tmp/slayerfs-db/metadata.db";
            exec_check(vm, &format!("rm -rf {db_dir} && mkdir -p {db_dir}")).await?;
            exec_check(
                vm,
                &format!(
                    "stat -c '%A %U:%G %n' /tmp {db_dir} || true; ls -ld {db_dir} && sh -lc 'touch {db_dir}/.w && rm {db_dir}/.w'",
                ),
            )
            .await?;
            exec_check(
                vm,
                &format!("rm -f {db_path} && touch {db_path} && stat -c '%A %U:%G %n' {db_path}",),
            )
            .await?;
            format!("sqlite://{db_path}")
        }
        MetaBackend::Etcd => {
            let gateway = detect_gateway(vm).await?;
            tracing::info!(backend = backend.as_str(), %gateway, "detected host gateway for etcd");
            cleanup_etcd_metadata(vm, &gateway).await?;
            backend_meta_url(backend, &gateway)
        }
        MetaBackend::Redis => {
            let gateway = detect_gateway(vm).await?;
            tracing::info!(backend = backend.as_str(), %gateway, "detected host gateway for redis");
            cleanup_redis_metadata(vm, &gateway).await?;
            backend_meta_url(backend, &gateway)
        }
    };

    tracing::info!(backend = backend.as_str(), %meta_url, "mounting slayerfs");
    run_slayerfs_mount_root(vm, backend, &meta_url).await?;
    basic_fs_checks(vm).await?;

    exec_check(
        vm,
        &format!(
            "sh -lc 'echo persist > {}/persist.txt'",
            SLAYERFS_MOUNTPOINT
        ),
    )
    .await?;
    exec_check(vm, &format!("mkdir -p {}/pdir/sub", SLAYERFS_MOUNTPOINT)).await?;

    // Ensure data is flushed to storage before killing the process
    exec_check(vm, "sync").await?;
    tokio::time::sleep(Duration::from_secs(2)).await;

    kill_slayerfs_best_effort(vm).await?;
    unmount_best_effort(vm).await?;
    assert_not_mounted(vm).await?;

    run_slayerfs_mount_root(vm, backend, &meta_url).await?;
    exec_check(
        vm,
        &format!(
            "sh -lc 'cat {}/persist.txt | grep -q persist'",
            SLAYERFS_MOUNTPOINT
        ),
    )
    .await?;
    exec_check(vm, &format!("test -d {}/pdir/sub", SLAYERFS_MOUNTPOINT)).await?;

    run_posix_edge_cases(vm).await?;
    run_concurrency_smoke(vm).await?;

    kill_slayerfs_best_effort(vm).await?;
    unmount_best_effort(vm).await?;
    assert_not_mounted(vm).await?;

    Ok(())
}

async fn upload_slayerfs(vm: &mut Machine, slayerfs_bin: &Path) -> Result<()> {
    vm.upload(slayerfs_bin, Path::new("/usr/local/bin")).await?;
    exec_check(vm, &format!("chmod +x {}", SLAYERFS_BIN_IN_VM)).await?;
    Ok(())
}

async fn upload_xfstests_assets(
    vm: &mut Machine,
    persistence_bin: &Path,
    backend: MetaBackend,
    gateway: &str,
) -> Result<()> {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR not set")?);
    let script = manifest_dir.join("tests/scripts/xfstests_slayer.sh");
    let exclude = manifest_dir.join("tests/scripts/xfstests_slayer.exclude");
    // Prebuilt xfstests tarballs checked into the repo via git lfs.
    // The shell script (xfstests_slayer.sh) expects them at:
    //   <script_dir>/xfstests-prebuilt/*.tar.gz
    // and will extract + use them directly, skipping clone+build entirely.
    let prebuilt_dir = manifest_dir.join("tests/scripts/xfstests-prebuilt");

    exec_check(
        vm,
        &format!("mkdir -p {XFSTESTS_STAGE_DIR}/xfstests-prebuilt"),
    )
    .await?;

    // Strip debug symbols from the binary to reduce upload size (573MB → ~36MB).
    // We write to a named temp file so the original is not modified.
    let stripped_persistence_bin = {
        let tmp_dir = tempfile::TempDir::new().context("create temp dir for stripped binary")?;
        let tmp_path = tmp_dir.path().join("persistence_demo");
        let original_size_mb = std::fs::metadata(persistence_bin)
            .map(|m| m.len() / 1_000_000)
            .unwrap_or(0);
        // Keep the TempDir alive until upload is done.
        let strip_result = std::process::Command::new("strip")
            .arg("--strip-debug")
            .arg(persistence_bin)
            .arg("-o")
            .arg(&tmp_path)
            .status();
        let use_unstripped = || (persistence_bin.to_path_buf(), None::<tempfile::TempDir>);
        match strip_result {
            Ok(status) if status.success() => {
                let stripped_size_mb = std::fs::metadata(&tmp_path)
                    .map(|m| m.len() / 1_000_000)
                    .unwrap_or(0);
                tracing::info!(
                    "stripped persistence_demo: {}MB → {}MB",
                    original_size_mb,
                    stripped_size_mb
                );
                (tmp_path, Some(tmp_dir))
            }
            Ok(status) => {
                tracing::warn!(
                    "strip exited with status {:?}; uploading unstripped binary ({}MB)",
                    status.code(),
                    original_size_mb
                );
                use_unstripped()
            }
            Err(error) => {
                tracing::warn!(
                    "strip is unavailable ({}); uploading unstripped binary ({}MB)",
                    error,
                    original_size_mb
                );
                use_unstripped()
            }
        }
    };

    vm.upload(&stripped_persistence_bin.0, Path::new(XFSTESTS_STAGE_DIR))
        .await
        .context("upload persistence_demo for xfstests")?;
    vm.upload(&script, Path::new(XFSTESTS_STAGE_DIR))
        .await
        .context("upload xfstests runner script")?;
    vm.upload(&exclude, Path::new(XFSTESTS_STAGE_DIR))
        .await
        .context("upload xfstests exclude list")?;

    // Generate and write the backend-specific config file directly in the VM.
    let config_in_vm = backend.xfstests_config_in_vm();
    let config_yaml = generate_backend_config(backend, gateway);
    exec_check(
        vm,
        &format!(
            "cat > {} <<'SLAYERFS_CFG_EOF'\n{}\nSLAYERFS_CFG_EOF",
            config_in_vm, config_yaml
        ),
    )
    .await
    .context("write backend config to VM")?;
    tracing::info!(backend = backend.as_str(), config = %config_in_vm, "wrote backend config in VM");

    // Upload all prebuilt tarballs found in the repo directory.
    if prebuilt_dir.is_dir() {
        let mut rd = tokio::fs::read_dir(&prebuilt_dir)
            .await
            .with_context(|| format!("read prebuilt dir {:?}", prebuilt_dir))?;
        while let Some(entry) = rd.next_entry().await? {
            let p = entry.path();
            if p.extension().map(|e| e == "gz").unwrap_or(false) {
                vm.upload(
                    &p,
                    Path::new(&format!("{XFSTESTS_STAGE_DIR}/xfstests-prebuilt")),
                )
                .await
                .with_context(|| format!("upload prebuilt tarball {:?}", p))?;
                tracing::info!("uploaded prebuilt xfstests tarball: {:?}", p);
            }
        }
    } else {
        tracing::warn!(
            "prebuilt xfstests directory not found at {:?}; script will fall back to clone+build",
            prebuilt_dir
        );
    }

    exec_check(
        vm,
        &format!(
            "chmod +x {bin} {script} && test -f {exclude} && test -f {cfg}",
            bin = XFSTESTS_BIN_IN_VM,
            script = XFSTESTS_SCRIPT_IN_VM,
            exclude = XFSTESTS_EXCLUDE_IN_VM,
            cfg = config_in_vm
        ),
    )
    .await?;

    Ok(())
}

async fn dump_xfstests_debug_info(vm: &mut Machine) -> Result<String> {
    let mut s = dump_debug_info(vm).await?;

    s.push_str("\n=== xfstests script log (tail) ===\n");
    s.push_str(&get_file_tail(vm, "/tmp/xfstests-script.log", 300).await?);

    s.push_str("\n=== xfstests local.config ===\n");
    s.push_str(&exec_check(vm, "cat /tmp/xfstests-dev/local.config 2>/dev/null || true").await?);

    s.push_str("\n=== xfstests results dir ===\n");
    s.push_str(
        &exec_check(
            vm,
            "find /tmp/xfstests-dev/results -maxdepth 2 -type f 2>/dev/null | sort || true",
        )
        .await?,
    );

    s.push_str("\n=== xfstests check.log (tail) ===\n");
    s.push_str(&get_file_tail(vm, "/tmp/xfstests-dev/results/check.log", 200).await?);

    s.push_str("\n=== xfstests full output (tail) ===\n");
    s.push_str(&get_file_tail(vm, "/tmp/xfstests-dev/results/check.out", 200).await?);

    Ok(s)
}

async fn collect_xfstests_artifacts(vm: &mut Machine, host_dir: &Path) -> Result<()> {
    tokio::fs::create_dir_all(host_dir).await?;

    let artifacts = [
        (SLAYERFS_LOG_PATH, host_dir.join("slayerfs.log")),
        (
            "/tmp/xfstests-dev/local.config",
            host_dir.join("local.config"),
        ),
        ("/tmp/xfstests-dev/results", host_dir.join("results")),
        (
            "/tmp/xfstests-script.log",
            host_dir.join("xfstests-script.log"),
        ),
    ];

    for (remote, local) in artifacts {
        if let Err(err) = vm.download(Path::new(remote), &local).await {
            tracing::warn!(remote, local = %local.display(), error = %err, "failed to download xfstests artifact");
        }
    }

    Ok(())
}

async fn run_xfstests_in_vm(
    vm: &mut Machine,
    persistence_bin: &Path,
    backend: MetaBackend,
) -> Result<PathBuf> {
    // Detect the host gateway for non-SQLite backends.
    let gateway = detect_gateway(vm)
        .await
        .unwrap_or_else(|_| "127.0.0.1".to_string());

    // Clean up metadata from any previous run.
    match backend {
        MetaBackend::Etcd => {
            cleanup_etcd_metadata(vm, &gateway).await?;
        }
        MetaBackend::Redis => {
            cleanup_redis_metadata(vm, &gateway).await?;
        }
        MetaBackend::Sqlite => {}
    }

    // Note: unattended-upgrades cleanup and swap creation are handled entirely
    // by the shell script (xfstests_slayer.sh / install_xfstests_deps).
    upload_xfstests_assets(vm, persistence_bin, backend, &gateway).await?;

    let host_artifact_dir = xfstests_host_artifact_dir();
    // When SLAYERFS_XFSTESTS_CASES is not set, the shell script runs the full
    // xfstests/generic suite with exclusions from xfstests_slayer.exclude.
    // Set the env var to restrict to specific cases, e.g. "generic/001 generic/002".
    let xfstests_cases = std::env::var("SLAYERFS_XFSTESTS_CASES").ok();
    let xfstests_repo = std::env::var("SLAYERFS_XFSTESTS_REPO")
        .unwrap_or_else(|_| "https://gitee.com/anolis/xfstests-dev.git".to_string());
    let xfstests_branch =
        std::env::var("SLAYERFS_XFSTESTS_BRANCH").unwrap_or_else(|_| "v2023.12.10".to_string());
    let timeout_secs = std::env::var("SLAYERFS_XFSTESTS_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(90 * 60);
    let force_reclone =
        std::env::var("SLAYERFS_XFSTESTS_FORCE_RECLONE").unwrap_or_else(|_| "0".to_string());

    // Only pass XFSTESTS_CASES when explicitly requested; otherwise the shell
    // script runs the full suite with -E xfstests_slayer.exclude.
    let cases_env = match &xfstests_cases {
        Some(c) => format!("XFSTESTS_CASES={}", shell_quote(c)),
        None => String::new(),
    };

    let config_in_vm = backend.xfstests_config_in_vm();
    let cmd = format!(
        "env SLAYERFS_BIN_PATH={bin} SLAYERFS_CONFIG_PATH={cfg} SLAYERFS_LOG_FILE={log} \
XFSTESTS_REPO={repo} XFSTESTS_BRANCH={branch} XFSTESTS_DIR={dir} {cases_env} \
XFSTESTS_FORCE_RECLONE={force_reclone} XFSTESTS_INSTALL_DEPS=0 {script}",
        bin = shell_quote(XFSTESTS_BIN_IN_VM),
        cfg = shell_quote(&config_in_vm),
        log = shell_quote(SLAYERFS_LOG_PATH),
        repo = shell_quote(&xfstests_repo),
        branch = shell_quote(&xfstests_branch),
        dir = shell_quote(XFSTESTS_REMOTE_DIR),
        cases_env = cases_env,
        force_reclone = shell_quote(&force_reclone),
        script = shell_quote(XFSTESTS_SCRIPT_IN_VM),
    );

    let run_result = exec_check_timed(vm, &cmd, Duration::from_secs(timeout_secs)).await;
    let _ = collect_xfstests_artifacts(vm, &host_artifact_dir).await;
    let artifact_debug = dump_xfstests_artifact_summary(&host_artifact_dir);

    if let Err(err) = run_result {
        let dbg = dump_xfstests_debug_info(vm)
            .await
            .unwrap_or_else(|e| e.to_string());
        anyhow::bail!(
            "{}\n\nartifacts: {}\n\n{}\n\n{}",
            err,
            host_artifact_dir.display(),
            artifact_debug,
            dbg
        );
    }

    Ok(host_artifact_dir)
}

// ---------------------------------------------------------------------------
// Basic KVM tests — one per metadata backend
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn test_slayerfs_kvm_sqlite() -> Result<()> {
    let slayerfs_bin = get_slayerfs_binary_path()?;
    start_vm_and_run(slayerfs_bin, MetaBackend::Sqlite).await
}

#[tokio::test]
#[ignore]
async fn test_slayerfs_kvm_redis() -> Result<()> {
    let slayerfs_bin = get_slayerfs_binary_path()?;
    start_vm_and_run(slayerfs_bin, MetaBackend::Redis).await
}

#[tokio::test]
#[ignore]
async fn test_slayerfs_kvm_etcd() -> Result<()> {
    let slayerfs_bin = get_slayerfs_binary_path()?;
    start_vm_and_run(slayerfs_bin, MetaBackend::Etcd).await
}

// ---------------------------------------------------------------------------
// xfstests KVM tests — one per metadata backend
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn test_slayerfs_kvm_xfstests_sqlite() -> Result<()> {
    let persistence_bin = get_persistence_demo_path()?;
    start_vm_and_run_xfstests(persistence_bin, MetaBackend::Sqlite).await
}

#[tokio::test]
#[ignore]
async fn test_slayerfs_kvm_xfstests_redis() -> Result<()> {
    let persistence_bin = get_persistence_demo_path()?;
    start_vm_and_run_xfstests(persistence_bin, MetaBackend::Redis).await
}

#[tokio::test]
#[ignore]
async fn test_slayerfs_kvm_xfstests_etcd() -> Result<()> {
    let persistence_bin = get_persistence_demo_path()?;
    start_vm_and_run_xfstests(persistence_bin, MetaBackend::Etcd).await
}
