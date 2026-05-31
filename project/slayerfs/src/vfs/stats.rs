//! Real-time performance statistics collector for SlayerFS.
//!
//! Provides atomic counters for FUSE operations, metadata ops, S3 object
//! traffic, and buffer usage. Metrics are exposed via a `.stats` virtual
//! file at the mount root (similar to JuiceFS) in a Prometheus-compatible
//! text format.
//!
//! The `stats` CLI tool reads this file periodically to display real-time
//! throughput and latency in the terminal.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Relaxed ordering is sufficient for stats counters — we only need eventual
/// visibility, not happens-before relationships.
const ORD: Ordering = Ordering::Relaxed;

/// Global filesystem statistics, designed for lock-free concurrent updates.
#[derive(Debug)]
pub struct FsStats {
    pub start_time: Instant,

    // ─── FUSE layer ───────────────────────────────────────────────
    /// Total FUSE read operations
    pub fuse_read_ops: AtomicU64,
    /// Total bytes read via FUSE
    pub fuse_read_bytes: AtomicU64,
    /// Total FUSE read latency in microseconds
    pub fuse_read_lat_us: AtomicU64,
    /// Total FUSE write operations
    pub fuse_write_ops: AtomicU64,
    /// Total bytes written via FUSE
    pub fuse_write_bytes: AtomicU64,
    /// Total FUSE write latency in microseconds
    pub fuse_write_lat_us: AtomicU64,
    /// Total FUSE lookup operations
    pub fuse_lookup_ops: AtomicU64,
    /// Total FUSE lookup latency in microseconds
    pub fuse_lookup_lat_us: AtomicU64,
    /// Total FUSE getattr operations
    pub fuse_getattr_ops: AtomicU64,
    /// Total FUSE getattr latency in microseconds
    pub fuse_getattr_lat_us: AtomicU64,
    /// Total FUSE open operations
    pub fuse_open_ops: AtomicU64,
    /// Total FUSE create operations
    pub fuse_create_ops: AtomicU64,
    /// Total FUSE unlink/rmdir operations
    pub fuse_unlink_ops: AtomicU64,
    /// Total FUSE readdir operations
    pub fuse_readdir_ops: AtomicU64,
    /// Total FUSE flush/fsync operations
    pub fuse_flush_ops: AtomicU64,
    /// Total FUSE flush/fsync latency in microseconds
    pub fuse_flush_lat_us: AtomicU64,

    // ─── Meta layer ──────────────────────────────────────────────
    /// Total metadata operations (get_node, lookup, etc.)
    pub meta_ops: AtomicU64,
    /// Total metadata operation latency in microseconds
    pub meta_lat_us: AtomicU64,
    /// Total metadata transaction (write/commit) operations
    pub meta_txn_ops: AtomicU64,
    /// Total metadata transaction latency in microseconds
    pub meta_txn_lat_us: AtomicU64,

    // ─── VFS diagnostic timing ───────────────────────────────────
    /// Total VFS create_file_at operations timed by the optional diagnostic path
    pub vfs_create_total_ops: AtomicU64,
    /// Total VFS create_file_at latency in microseconds
    pub vfs_create_total_lat_us: AtomicU64,
    /// Total metadata create calls inside create_file_at
    pub vfs_create_meta_ops: AtomicU64,
    /// Total metadata create latency inside create_file_at in microseconds
    pub vfs_create_meta_lat_us: AtomicU64,
    /// Total VFS unlink_at operations timed by the optional diagnostic path
    pub vfs_unlink_total_ops: AtomicU64,
    /// Total VFS unlink_at latency in microseconds
    pub vfs_unlink_total_lat_us: AtomicU64,
    /// Total lookup calls inside unlink_at
    pub vfs_unlink_lookup_ops: AtomicU64,
    /// Total lookup latency inside unlink_at in microseconds
    pub vfs_unlink_lookup_lat_us: AtomicU64,
    /// Total stat calls inside unlink_at
    pub vfs_unlink_stat_ops: AtomicU64,
    /// Total stat latency inside unlink_at in microseconds
    pub vfs_unlink_stat_lat_us: AtomicU64,
    /// Total metadata unlink calls inside unlink_at
    pub vfs_unlink_meta_ops: AtomicU64,
    /// Total metadata unlink latency inside unlink_at in microseconds
    pub vfs_unlink_meta_lat_us: AtomicU64,
    /// Total recently-unlinked map updates inside unlink_at
    pub vfs_unlink_recent_ops: AtomicU64,
    /// Total recently-unlinked map update latency inside unlink_at in microseconds
    pub vfs_unlink_recent_lat_us: AtomicU64,
    /// Total remove-first deleted-inode setattr map probes
    pub vfs_setattr_recent_remove_ops: AtomicU64,
    /// Total remove-first deleted-inode setattr map latency in microseconds
    pub vfs_setattr_recent_remove_lat_us: AtomicU64,
    /// Total get_mut deleted-inode setattr map probes
    pub vfs_setattr_recent_get_mut_ops: AtomicU64,
    /// Total get_mut deleted-inode setattr map latency in microseconds
    pub vfs_setattr_recent_get_mut_lat_us: AtomicU64,

    // ─── Object storage (S3) layer ───────────────────────────────
    /// Total S3 GET requests
    pub s3_get_ops: AtomicU64,
    /// Total bytes fetched from S3
    pub s3_get_bytes: AtomicU64,
    /// Total S3 GET latency in microseconds
    pub s3_get_lat_us: AtomicU64,
    /// Total S3 PUT requests
    pub s3_put_ops: AtomicU64,
    /// Total bytes uploaded to S3
    pub s3_put_bytes: AtomicU64,
    /// Total S3 PUT latency in microseconds
    pub s3_put_lat_us: AtomicU64,
    /// Total S3 DELETE requests
    pub s3_del_ops: AtomicU64,

    // ─── Buffer/cache usage ──────────────────────────────────────
    /// Current dirty write buffer bytes
    pub buf_dirty_bytes: AtomicU64,
    /// Current reader cache bytes
    pub buf_read_bytes: AtomicU64,
    /// Block cache hit count
    pub cache_hits: AtomicU64,
    /// Block cache miss count
    pub cache_misses: AtomicU64,
}

impl FsStats {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
            fuse_read_ops: AtomicU64::new(0),
            fuse_read_bytes: AtomicU64::new(0),
            fuse_read_lat_us: AtomicU64::new(0),
            fuse_write_ops: AtomicU64::new(0),
            fuse_write_bytes: AtomicU64::new(0),
            fuse_write_lat_us: AtomicU64::new(0),
            fuse_lookup_ops: AtomicU64::new(0),
            fuse_lookup_lat_us: AtomicU64::new(0),
            fuse_getattr_ops: AtomicU64::new(0),
            fuse_getattr_lat_us: AtomicU64::new(0),
            fuse_open_ops: AtomicU64::new(0),
            fuse_create_ops: AtomicU64::new(0),
            fuse_unlink_ops: AtomicU64::new(0),
            fuse_readdir_ops: AtomicU64::new(0),
            fuse_flush_ops: AtomicU64::new(0),
            fuse_flush_lat_us: AtomicU64::new(0),
            meta_ops: AtomicU64::new(0),
            meta_lat_us: AtomicU64::new(0),
            meta_txn_ops: AtomicU64::new(0),
            meta_txn_lat_us: AtomicU64::new(0),
            vfs_create_total_ops: AtomicU64::new(0),
            vfs_create_total_lat_us: AtomicU64::new(0),
            vfs_create_meta_ops: AtomicU64::new(0),
            vfs_create_meta_lat_us: AtomicU64::new(0),
            vfs_unlink_total_ops: AtomicU64::new(0),
            vfs_unlink_total_lat_us: AtomicU64::new(0),
            vfs_unlink_lookup_ops: AtomicU64::new(0),
            vfs_unlink_lookup_lat_us: AtomicU64::new(0),
            vfs_unlink_stat_ops: AtomicU64::new(0),
            vfs_unlink_stat_lat_us: AtomicU64::new(0),
            vfs_unlink_meta_ops: AtomicU64::new(0),
            vfs_unlink_meta_lat_us: AtomicU64::new(0),
            vfs_unlink_recent_ops: AtomicU64::new(0),
            vfs_unlink_recent_lat_us: AtomicU64::new(0),
            vfs_setattr_recent_remove_ops: AtomicU64::new(0),
            vfs_setattr_recent_remove_lat_us: AtomicU64::new(0),
            vfs_setattr_recent_get_mut_ops: AtomicU64::new(0),
            vfs_setattr_recent_get_mut_lat_us: AtomicU64::new(0),
            s3_get_ops: AtomicU64::new(0),
            s3_get_bytes: AtomicU64::new(0),
            s3_get_lat_us: AtomicU64::new(0),
            s3_put_ops: AtomicU64::new(0),
            s3_put_bytes: AtomicU64::new(0),
            s3_put_lat_us: AtomicU64::new(0),
            s3_del_ops: AtomicU64::new(0),
            buf_dirty_bytes: AtomicU64::new(0),
            buf_read_bytes: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
        }
    }

    /// Render all counters in Prometheus text format (one metric per line).
    /// Format: `metric_name value\n`
    pub fn render(&self) -> String {
        let uptime_secs = self.start_time.elapsed().as_secs();
        let mut out = String::with_capacity(2048);

        // System
        out.push_str(&format!("slayerfs_uptime_seconds {}\n", uptime_secs));

        // FUSE
        out.push_str(&format!(
            "slayerfs_fuse_read_ops_total {}\n",
            self.fuse_read_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_fuse_read_bytes_total {}\n",
            self.fuse_read_bytes.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_fuse_read_lat_us_total {}\n",
            self.fuse_read_lat_us.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_fuse_write_ops_total {}\n",
            self.fuse_write_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_fuse_write_bytes_total {}\n",
            self.fuse_write_bytes.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_fuse_write_lat_us_total {}\n",
            self.fuse_write_lat_us.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_fuse_lookup_ops_total {}\n",
            self.fuse_lookup_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_fuse_lookup_lat_us_total {}\n",
            self.fuse_lookup_lat_us.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_fuse_getattr_ops_total {}\n",
            self.fuse_getattr_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_fuse_getattr_lat_us_total {}\n",
            self.fuse_getattr_lat_us.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_fuse_open_ops_total {}\n",
            self.fuse_open_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_fuse_create_ops_total {}\n",
            self.fuse_create_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_fuse_unlink_ops_total {}\n",
            self.fuse_unlink_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_fuse_readdir_ops_total {}\n",
            self.fuse_readdir_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_fuse_flush_ops_total {}\n",
            self.fuse_flush_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_fuse_flush_lat_us_total {}\n",
            self.fuse_flush_lat_us.load(ORD)
        ));

        // Meta
        out.push_str(&format!(
            "slayerfs_meta_ops_total {}\n",
            self.meta_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_meta_lat_us_total {}\n",
            self.meta_lat_us.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_meta_txn_ops_total {}\n",
            self.meta_txn_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_meta_txn_lat_us_total {}\n",
            self.meta_txn_lat_us.load(ORD)
        ));

        // VFS diagnostic timing
        out.push_str(&format!(
            "slayerfs_vfs_create_total_ops_total {}\n",
            self.vfs_create_total_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_vfs_create_total_lat_us_total {}\n",
            self.vfs_create_total_lat_us.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_vfs_create_meta_ops_total {}\n",
            self.vfs_create_meta_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_vfs_create_meta_lat_us_total {}\n",
            self.vfs_create_meta_lat_us.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_vfs_unlink_total_ops_total {}\n",
            self.vfs_unlink_total_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_vfs_unlink_total_lat_us_total {}\n",
            self.vfs_unlink_total_lat_us.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_vfs_unlink_lookup_ops_total {}\n",
            self.vfs_unlink_lookup_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_vfs_unlink_lookup_lat_us_total {}\n",
            self.vfs_unlink_lookup_lat_us.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_vfs_unlink_stat_ops_total {}\n",
            self.vfs_unlink_stat_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_vfs_unlink_stat_lat_us_total {}\n",
            self.vfs_unlink_stat_lat_us.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_vfs_unlink_meta_ops_total {}\n",
            self.vfs_unlink_meta_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_vfs_unlink_meta_lat_us_total {}\n",
            self.vfs_unlink_meta_lat_us.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_vfs_unlink_recent_ops_total {}\n",
            self.vfs_unlink_recent_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_vfs_unlink_recent_lat_us_total {}\n",
            self.vfs_unlink_recent_lat_us.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_vfs_setattr_recent_remove_ops_total {}\n",
            self.vfs_setattr_recent_remove_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_vfs_setattr_recent_remove_lat_us_total {}\n",
            self.vfs_setattr_recent_remove_lat_us.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_vfs_setattr_recent_get_mut_ops_total {}\n",
            self.vfs_setattr_recent_get_mut_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_vfs_setattr_recent_get_mut_lat_us_total {}\n",
            self.vfs_setattr_recent_get_mut_lat_us.load(ORD)
        ));

        // Object storage
        out.push_str(&format!(
            "slayerfs_s3_get_ops_total {}\n",
            self.s3_get_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_s3_get_bytes_total {}\n",
            self.s3_get_bytes.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_s3_get_lat_us_total {}\n",
            self.s3_get_lat_us.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_s3_put_ops_total {}\n",
            self.s3_put_ops.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_s3_put_bytes_total {}\n",
            self.s3_put_bytes.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_s3_put_lat_us_total {}\n",
            self.s3_put_lat_us.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_s3_del_ops_total {}\n",
            self.s3_del_ops.load(ORD)
        ));

        // Buffer/cache
        out.push_str(&format!(
            "slayerfs_buffer_dirty_bytes {}\n",
            self.buf_dirty_bytes.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_buffer_read_bytes {}\n",
            self.buf_read_bytes.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_cache_hits_total {}\n",
            self.cache_hits.load(ORD)
        ));
        out.push_str(&format!(
            "slayerfs_cache_misses_total {}\n",
            self.cache_misses.load(ORD)
        ));

        out
    }

    pub fn record_duration(ops_counter: &AtomicU64, lat_counter: &AtomicU64, duration: Duration) {
        let elapsed_us = duration.as_micros() as u64;
        ops_counter.fetch_add(1, ORD);
        lat_counter.fetch_add(elapsed_us, ORD);
    }
}

impl Default for FsStats {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard for timing an operation and recording latency + count.
/// Records stats on drop, so it can be used as `let _timer = OpTimer::new(...)`.
pub struct OpTimer<'a> {
    start: Instant,
    ops_counter: &'a AtomicU64,
    lat_counter: &'a AtomicU64,
}

impl<'a> OpTimer<'a> {
    pub fn new(ops_counter: &'a AtomicU64, lat_counter: &'a AtomicU64) -> Self {
        Self {
            start: Instant::now(),
            ops_counter,
            lat_counter,
        }
    }

    /// Finish timing and record the operation (consumes self).
    pub fn finish(self) {
        // Drop impl handles the actual recording.
    }
}

impl<'a> Drop for OpTimer<'a> {
    fn drop(&mut self) {
        FsStats::record_duration(self.ops_counter, self.lat_counter, self.start.elapsed());
    }
}

/// Optional timer for diagnostic hot-path stats. Disabled timers avoid
/// `Instant::now()` so production hot paths only pay a cheap branch.
pub struct MaybeOpTimer<'a> {
    start: Option<Instant>,
    ops_counter: &'a AtomicU64,
    lat_counter: &'a AtomicU64,
}

impl<'a> MaybeOpTimer<'a> {
    pub fn new(enabled: bool, ops_counter: &'a AtomicU64, lat_counter: &'a AtomicU64) -> Self {
        Self {
            start: enabled.then(Instant::now),
            ops_counter,
            lat_counter,
        }
    }
}

impl<'a> Drop for MaybeOpTimer<'a> {
    fn drop(&mut self) {
        if let Some(start) = self.start {
            FsStats::record_duration(self.ops_counter, self.lat_counter, start.elapsed());
        }
    }
}

/// Convenience macro for timing an async operation and recording stats.
///
/// Usage:
/// ```ignore
/// let result = timed_op!(stats.fuse_read_ops, stats.fuse_read_lat_us, {
///     handle.read(offset, len).await
/// });
/// ```
#[macro_export]
macro_rules! timed_op {
    ($ops:expr, $lat:expr, $body:expr) => {{
        let __start = std::time::Instant::now();
        let __result = $body;
        let __elapsed_us = __start.elapsed().as_micros() as u64;
        $ops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        $lat.fetch_add(__elapsed_us, std::sync::atomic::Ordering::Relaxed);
        __result
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_contains_all_metrics() {
        let stats = FsStats::new();
        stats.fuse_read_ops.store(42, ORD);
        stats.fuse_read_bytes.store(1024 * 1024, ORD);
        stats.s3_put_ops.store(10, ORD);

        let output = stats.render();
        assert!(output.contains("slayerfs_fuse_read_ops_total 42"));
        assert!(output.contains("slayerfs_fuse_read_bytes_total 1048576"));
        assert!(output.contains("slayerfs_s3_put_ops_total 10"));
        assert!(output.contains("slayerfs_uptime_seconds"));
        assert!(output.contains("slayerfs_cache_hits_total 0"));
        assert!(output.contains("slayerfs_vfs_create_total_ops_total 0"));
        assert!(output.contains("slayerfs_vfs_unlink_lookup_lat_us_total 0"));
        assert!(output.contains("slayerfs_vfs_unlink_recent_ops_total 0"));
        assert!(output.contains("slayerfs_vfs_setattr_recent_remove_lat_us_total 0"));
    }

    #[test]
    fn op_timer_records_latency() {
        let ops = AtomicU64::new(0);
        let lat = AtomicU64::new(0);

        let timer = OpTimer::new(&ops, &lat);
        std::thread::sleep(std::time::Duration::from_millis(1));
        timer.finish();

        assert_eq!(ops.load(ORD), 1);
        assert!(lat.load(ORD) >= 1000); // at least 1ms = 1000us
    }
}
