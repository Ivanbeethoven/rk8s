#!/usr/bin/env python3
"""
SlayerFS Performance Analysis Tool

Generates a detailed performance profile report from fio JSON results,
slayerfs logs, and optional perf profiling data.

Usage:
    # Analyze a single run
    python3 analyze_perf.py /path/to/perf-run-XXXXX/

    # Compare two runs
    python3 analyze_perf.py --compare /path/to/baseline/ /path/to/current/

    # Analyze with bottleneck identification
    python3 analyze_perf.py --bottleneck /path/to/perf-run-XXXXX/
"""

import argparse
import json
import os
import pathlib
import sys
from dataclasses import dataclass, field
from typing import Optional


@dataclass
class FioResult:
    """Parsed fio job result for one workload."""

    name: str
    rw: str
    bs: str
    numjobs: int
    # Read metrics
    read_bw_bytes: float = 0
    read_iops: float = 0
    read_lat_mean_ns: float = 0
    read_lat_percentiles: dict = field(default_factory=dict)
    read_total_ios: int = 0
    # Write metrics
    write_bw_bytes: float = 0
    write_iops: float = 0
    write_lat_mean_ns: float = 0
    write_lat_percentiles: dict = field(default_factory=dict)
    write_total_ios: int = 0
    # Submission latency (time from submission to actual dispatch)
    read_slat_mean_ns: float = 0
    write_slat_mean_ns: float = 0
    # Runtime
    runtime_ms: float = 0


def parse_fio_json(path: pathlib.Path) -> Optional[FioResult]:
    """Parse a single fio JSON output file."""
    try:
        data = json.loads(path.read_text())
    except (json.JSONDecodeError, OSError):
        return None

    jobs = data.get("jobs", [])
    if not jobs:
        return None

    # Aggregate across all jobs (group_reporting combines them)
    job = jobs[0]
    options = job.get("job options", {})

    result = FioResult(
        name=path.stem,
        rw=options.get("rw", "unknown"),
        bs=options.get("bs", "unknown"),
        numjobs=int(options.get("numjobs", 1)),
    )

    read_op = job.get("read", {})
    if read_op.get("bw_bytes", 0) > 0:
        result.read_bw_bytes = read_op["bw_bytes"]
        result.read_iops = read_op["iops"]
        result.read_lat_mean_ns = read_op.get("lat_ns", {}).get("mean", 0)
        result.read_lat_percentiles = read_op.get("clat_ns", {}).get("percentile", {})
        result.read_slat_mean_ns = read_op.get("slat_ns", {}).get("mean", 0)
        result.read_total_ios = int(read_op.get("total_ios", 0))
        result.runtime_ms = read_op.get("runtime", 0)

    write_op = job.get("write", {})
    if write_op.get("bw_bytes", 0) > 0:
        result.write_bw_bytes = write_op["bw_bytes"]
        result.write_iops = write_op["iops"]
        result.write_lat_mean_ns = write_op.get("lat_ns", {}).get("mean", 0)
        result.write_lat_percentiles = write_op.get("clat_ns", {}).get("percentile", {})
        result.write_slat_mean_ns = write_op.get("slat_ns", {}).get("mean", 0)
        result.write_total_ios = int(write_op.get("total_ios", 0))
        if result.runtime_ms == 0:
            result.runtime_ms = write_op.get("runtime", 0)

    return result


def fmt_bw(bw_bytes: float) -> str:
    """Format bandwidth in human-readable form."""
    if bw_bytes == 0:
        return "-"
    mib = bw_bytes / (1024 * 1024)
    if mib >= 1024:
        return f"{mib / 1024:.2f} GiB/s"
    return f"{mib:.1f} MiB/s"


def fmt_lat(ns: float) -> str:
    """Format latency from nanoseconds."""
    if ns == 0:
        return "-"
    ms = ns / 1_000_000
    if ms < 0.1:
        return f"{ns / 1000:.1f} µs"
    if ms < 1000:
        return f"{ms:.2f} ms"
    return f"{ms / 1000:.2f} s"


def fmt_iops(iops: float) -> str:
    if iops == 0:
        return "-"
    if iops >= 1000:
        return f"{iops / 1000:.1f}K"
    return f"{iops:.1f}"


PERCENTILE_KEYS = [
    "1.000000", "5.000000", "10.000000", "25.000000",
    "50.000000", "75.000000", "90.000000", "95.000000",
    "99.000000", "99.900000", "99.990000",
]

PERCENTILE_LABELS = [
    "p1", "p5", "p10", "p25", "p50", "p75", "p90", "p95", "p99", "p99.9", "p99.99"
]


def generate_latency_table(results: list[FioResult]) -> list[str]:
    """Generate detailed latency percentile table."""
    lines = [
        "",
        "## Latency Distribution",
        "",
        "### Read Latency Percentiles",
        "",
        "| Workload | p1 | p5 | p25 | p50 | p75 | p90 | p95 | p99 | p99.9 |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]

    pct_keys = ["1.000000", "5.000000", "25.000000", "50.000000",
                "75.000000", "90.000000", "95.000000", "99.000000", "99.900000"]
    pct_labels = ["p1", "p5", "p25", "p50", "p75", "p90", "p95", "p99", "p99.9"]

    for r in results:
        if not r.read_lat_percentiles:
            continue
        cols = [r.name]
        for k in pct_keys:
            val = r.read_lat_percentiles.get(k, 0)
            cols.append(fmt_lat(val))
        lines.append("| " + " | ".join(cols) + " |")

    lines.extend([
        "",
        "### Write Latency Percentiles",
        "",
        "| Workload | p1 | p5 | p25 | p50 | p75 | p90 | p95 | p99 | p99.9 |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |",
    ])

    for r in results:
        if not r.write_lat_percentiles:
            continue
        cols = [r.name]
        for k in pct_keys:
            val = r.write_lat_percentiles.get(k, 0)
            cols.append(fmt_lat(val))
        lines.append("| " + " | ".join(cols) + " |")

    return lines


def generate_bottleneck_analysis(results: list[FioResult]) -> list[str]:
    """Heuristic bottleneck identification based on latency patterns."""
    lines = [
        "",
        "## Bottleneck Analysis",
        "",
    ]

    for r in results:
        findings = []

        # Analyze read path
        if r.read_lat_percentiles:
            p50 = r.read_lat_percentiles.get("50.000000", 0) / 1e6
            p99 = r.read_lat_percentiles.get("99.000000", 0) / 1e6
            p999 = r.read_lat_percentiles.get("99.900000", 0) / 1e6

            if p99 > p50 * 5:
                findings.append(
                    f"  - **Read tail latency**: p99/p50 = {p99/p50:.1f}x "
                    f"({fmt_lat(p50*1e6)} → {fmt_lat(p99*1e6)}). "
                    "Likely cause: S3 GET retry or cache miss on cold blocks."
                )
            if p999 > p99 * 3:
                findings.append(
                    f"  - **Read outliers**: p99.9/p99 = {p999/p99:.1f}x. "
                    "Possible GC pause, TCP retransmit, or lock contention."
                )
            if p50 > 50:  # >50ms median for reads
                findings.append(
                    f"  - **High read baseline**: p50={p50:.0f}ms. "
                    "Network RTT to S3 dominates. Consider prefetch tuning or local cache."
                )

        # Analyze write path
        if r.write_lat_percentiles:
            p50 = r.write_lat_percentiles.get("50.000000", 0) / 1e6
            p99 = r.write_lat_percentiles.get("99.000000", 0) / 1e6
            p999 = r.write_lat_percentiles.get("99.900000", 0) / 1e6

            if p50 < 10 and p99 > 100:
                findings.append(
                    f"  - **Write stall pattern**: p50={p50:.1f}ms, p99={p99:.0f}ms. "
                    "Most writes are buffered (fast), but auto_flush/freeze triggers "
                    "S3 upload that blocks subsequent writes (write buffer hard limit)."
                )
            if p99 > 500:
                findings.append(
                    f"  - **Write P99 > 500ms** ({p99:.0f}ms): "
                    "Consider increasing write buffer capacity or S3 upload concurrency."
                )

        # Throughput analysis
        if r.read_bw_bytes > 0 and r.numjobs > 1:
            per_job_bw = r.read_bw_bytes / r.numjobs / (1024 * 1024)
            if per_job_bw < 50:
                findings.append(
                    f"  - **Read scaling**: {per_job_bw:.0f} MiB/s/job "
                    f"({r.numjobs} jobs). May be limited by S3 connection pool or prefetch contention."
                )

        if r.write_bw_bytes > 0:
            # Check if write BW is suspiciously low relative to p50
            effective_bw = r.write_iops * int(r.bs.replace("m", "")) * 1024 * 1024
            if r.write_bw_bytes < effective_bw * 0.5 and r.numjobs > 1:
                findings.append(
                    f"  - **Write contention**: actual BW lower than expected from IOPS×BS. "
                    "Likely mutex contention or write buffer serialization."
                )

        if findings:
            lines.append(f"### {r.name} ({r.rw}, bs={r.bs}, jobs={r.numjobs})")
            lines.append("")
            lines.extend(findings)
            lines.append("")

    if len(lines) == 3:
        lines.append("No significant bottlenecks detected in latency distribution.")
        lines.append("")

    return lines


def generate_comparison(baseline: list[FioResult], current: list[FioResult]) -> list[str]:
    """Compare two runs and show deltas."""
    lines = [
        "",
        "## Comparison (Baseline → Current)",
        "",
        "| Workload | Metric | Baseline | Current | Delta |",
        "| --- | --- | ---: | ---: | ---: |",
    ]

    baseline_map = {r.name: r for r in baseline}

    for curr in current:
        base = baseline_map.get(curr.name)
        if not base:
            continue

        # Read BW
        if curr.read_bw_bytes > 0 and base.read_bw_bytes > 0:
            delta_pct = (curr.read_bw_bytes - base.read_bw_bytes) / base.read_bw_bytes * 100
            sign = "+" if delta_pct > 0 else ""
            lines.append(
                f"| {curr.name} | Read BW | {fmt_bw(base.read_bw_bytes)} | "
                f"{fmt_bw(curr.read_bw_bytes)} | {sign}{delta_pct:.1f}% |"
            )

        # Write BW
        if curr.write_bw_bytes > 0 and base.write_bw_bytes > 0:
            delta_pct = (curr.write_bw_bytes - base.write_bw_bytes) / base.write_bw_bytes * 100
            sign = "+" if delta_pct > 0 else ""
            lines.append(
                f"| {curr.name} | Write BW | {fmt_bw(base.write_bw_bytes)} | "
                f"{fmt_bw(curr.write_bw_bytes)} | {sign}{delta_pct:.1f}% |"
            )

        # Read P99
        if curr.read_lat_percentiles and base.read_lat_percentiles:
            c99 = curr.read_lat_percentiles.get("99.000000", 0)
            b99 = base.read_lat_percentiles.get("99.000000", 0)
            if b99 > 0:
                delta_pct = (c99 - b99) / b99 * 100
                sign = "+" if delta_pct > 0 else ""
                lines.append(
                    f"| {curr.name} | Read P99 | {fmt_lat(b99)} | "
                    f"{fmt_lat(c99)} | {sign}{delta_pct:.1f}% |"
                )

        # Write P99
        if curr.write_lat_percentiles and base.write_lat_percentiles:
            c99 = curr.write_lat_percentiles.get("99.000000", 0)
            b99 = base.write_lat_percentiles.get("99.000000", 0)
            if b99 > 0:
                delta_pct = (c99 - b99) / b99 * 100
                sign = "+" if delta_pct > 0 else ""
                lines.append(
                    f"| {curr.name} | Write P99 | {fmt_lat(b99)} | "
                    f"{fmt_lat(c99)} | {sign}{delta_pct:.1f}% |"
                )

    return lines


def generate_optimization_roadmap(results: list[FioResult]) -> list[str]:
    """Generate prioritized optimization suggestions based on results."""
    lines = [
        "",
        "## Optimization Roadmap",
        "",
    ]

    suggestions = []

    for r in results:
        # High write tail latency → buffer management
        if r.write_lat_percentiles:
            p99 = r.write_lat_percentiles.get("99.000000", 0) / 1e6
            if p99 > 300 and "write" in r.rw:
                suggestions.append((
                    "Write Buffer Management",
                    f"Write P99={p99:.0f}ms in {r.name}. Consider: "
                    "increase write_buffer_hard_limit, use adaptive auto_flush "
                    "based on upload throughput feedback, or implement S3 upload "
                    "pipelining to avoid blocking writes during uploads.",
                    1,
                ))

        # High read latency → prefetch/cache
        if r.read_lat_percentiles:
            p50 = r.read_lat_percentiles.get("50.000000", 0) / 1e6
            if p50 > 100 and "rand" in r.rw:
                suggestions.append((
                    "Random Read Prefetch",
                    f"Random read p50={p50:.0f}ms in {r.name}. Each 4MB block "
                    "requires a full S3 GET. Consider: smaller block size for "
                    "random workloads, read-ahead pattern detection, or tiered "
                    "block cache with SSD backing.",
                    2,
                ))
            elif p50 > 10 and "seq" in r.rw:
                suggestions.append((
                    "Sequential Read Pipeline",
                    f"Seq read p50={p50:.0f}ms in {r.name}. Consider: "
                    "aggressive prefetch (read-ahead window), coalescing "
                    "adjacent block fetches into single range GET, or "
                    "io_uring for concurrent S3 requests.",
                    3,
                ))

        # Multi-job scaling
        if r.numjobs > 1 and r.read_bw_bytes > 0:
            per_job = r.read_bw_bytes / r.numjobs / (1024 * 1024)
            if per_job < 40:
                suggestions.append((
                    "Parallel Read Scaling",
                    f"Only {per_job:.0f} MiB/s/job in {r.name} ({r.numjobs} jobs). "
                    "May be limited by: connection pool size, prefetch contention, "
                    "or per-inode lock granularity. Consider per-chunk parallelism.",
                    4,
                ))

    # Deduplicate
    seen = set()
    priority = 1
    for title, desc, _ in sorted(suggestions, key=lambda x: x[2]):
        if title in seen:
            continue
        seen.add(title)
        lines.append(f"### {priority}. {title}")
        lines.append("")
        lines.append(desc)
        lines.append("")
        priority += 1

    if not suggestions:
        lines.append("All workloads performing within expected parameters.")
        lines.append("")

    return lines


def generate_meta_perf_analysis(artifact_dir: pathlib.Path) -> list[str]:
    """Parse metaperf log for metadata operation analysis."""
    metaperf_log = artifact_dir / "tools" / "metaperf.log"
    if not metaperf_log.exists():
        return []

    lines = [
        "",
        "## Metadata Performance",
        "",
        "| Operation | Ops/sec | Latency (µs/op) |",
        "| --- | ---: | ---: |",
    ]

    for line in metaperf_log.read_text().splitlines():
        # Format: "create: 25 times, 200 file(s) ... ops/sec=176.21, usec/op=5675.17"
        if "ops/sec=" in line and "usec/op" in line:
            op = line.split(":")[0].strip()
            ops_sec = line.split("ops/sec=")[1].split(",")[0]
            usec_op = line.split("usec/op")[1].strip().lstrip("=").strip()
            lines.append(f"| {op} | {float(ops_sec):.1f} | {float(usec_op):.0f} |")

    return lines


def generate_report(artifact_dir: pathlib.Path, bottleneck: bool = False) -> str:
    """Generate comprehensive performance report."""
    results_dir = artifact_dir / "results"
    results: list[FioResult] = []

    if results_dir.exists():
        for json_file in sorted(results_dir.glob("fio*.json")):
            r = parse_fio_json(json_file)
            if r:
                results.append(r)

    lines = [
        "# SlayerFS Detailed Performance Profile",
        "",
        f"Artifact: `{artifact_dir.name}`",
        "",
    ]

    # Summary table
    lines.extend([
        "## Throughput Summary",
        "",
        "| Workload | Mode | BS | Jobs | Read BW | Write BW | Read IOPS | Write IOPS |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |",
    ])
    for r in results:
        lines.append(
            f"| {r.name} | {r.rw} | {r.bs} | {r.numjobs} | "
            f"{fmt_bw(r.read_bw_bytes)} | {fmt_bw(r.write_bw_bytes)} | "
            f"{fmt_iops(r.read_iops)} | {fmt_iops(r.write_iops)} |"
        )

    # Latency summary
    lines.extend([
        "",
        "## Latency Summary",
        "",
        "| Workload | Read Mean | Read P50 | Read P99 | Write Mean | Write P50 | Write P99 |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: |",
    ])
    for r in results:
        rp50 = fmt_lat(r.read_lat_percentiles.get("50.000000", 0))
        rp99 = fmt_lat(r.read_lat_percentiles.get("99.000000", 0))
        wp50 = fmt_lat(r.write_lat_percentiles.get("50.000000", 0))
        wp99 = fmt_lat(r.write_lat_percentiles.get("99.000000", 0))
        lines.append(
            f"| {r.name} | {fmt_lat(r.read_lat_mean_ns)} | {rp50} | {rp99} | "
            f"{fmt_lat(r.write_lat_mean_ns)} | {wp50} | {wp99} |"
        )

    # Detailed percentiles
    lines.extend(generate_latency_table(results))

    # Metadata perf
    lines.extend(generate_meta_perf_analysis(artifact_dir))

    # Bottleneck analysis
    if bottleneck:
        lines.extend(generate_bottleneck_analysis(results))
        lines.extend(generate_optimization_roadmap(results))

    return "\n".join(lines) + "\n"


def generate_comparison_report(
    baseline_dir: pathlib.Path, current_dir: pathlib.Path
) -> str:
    """Generate comparison report between two runs."""
    baseline_results = []
    current_results = []

    for json_file in sorted((baseline_dir / "results").glob("fio*.json")):
        r = parse_fio_json(json_file)
        if r:
            baseline_results.append(r)

    for json_file in sorted((current_dir / "results").glob("fio*.json")):
        r = parse_fio_json(json_file)
        if r:
            current_results.append(r)

    lines = [
        "# SlayerFS Performance Comparison",
        "",
        f"Baseline: `{baseline_dir.name}`",
        f"Current:  `{current_dir.name}`",
        "",
    ]

    lines.extend(generate_comparison(baseline_results, current_results))

    # Also show current absolute numbers
    lines.extend([
        "",
        "## Current Run Details",
        "",
    ])
    for r in current_results:
        parts = [f"**{r.name}** ({r.rw}, bs={r.bs}, {r.numjobs}j):"]
        if r.read_bw_bytes > 0:
            parts.append(f"Read {fmt_bw(r.read_bw_bytes)}")
        if r.write_bw_bytes > 0:
            parts.append(f"Write {fmt_bw(r.write_bw_bytes)}")
        lines.append("- " + ", ".join(parts))

    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(
        description="SlayerFS performance analysis tool"
    )
    parser.add_argument(
        "artifact_dir",
        nargs="?",
        help="Path to perf-run artifact directory",
    )
    parser.add_argument(
        "--compare",
        nargs=2,
        metavar=("BASELINE", "CURRENT"),
        help="Compare two runs",
    )
    parser.add_argument(
        "--bottleneck",
        action="store_true",
        help="Include bottleneck identification and optimization roadmap",
    )
    parser.add_argument(
        "--output", "-o",
        help="Output file (default: stdout)",
    )
    args = parser.parse_args()

    if args.compare:
        report = generate_comparison_report(
            pathlib.Path(args.compare[0]),
            pathlib.Path(args.compare[1]),
        )
    elif args.artifact_dir:
        report = generate_report(
            pathlib.Path(args.artifact_dir),
            bottleneck=args.bottleneck,
        )
    else:
        parser.print_help()
        sys.exit(1)

    if args.output:
        pathlib.Path(args.output).write_text(report)
        print(f"Report written to {args.output}", file=sys.stderr)
    else:
        print(report)


if __name__ == "__main__":
    main()
