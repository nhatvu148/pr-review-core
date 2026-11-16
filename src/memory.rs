//! Memory observability for a review.
//!
//! A bot that runs agentic reviews spawns an agent CLI and clones the repo for
//! each one, and the failure that sizing guesses produce is a silent one: the
//! kernel OOM-kills the agent child, the reviews that survive report a
//! *timeout*, and the logs name the symptom while hiding the cause. Both bots
//! built on this crate hit exactly that before measuring anything, and chose
//! their concurrency and VM size without a single reading of what a review costs.
//!
//! **Read the cgroup, not just this process.** `/proc/self/statm` reports only the
//! server's own resident set — single-digit MB, and never the thing that gets it
//! killed. What the kernel enforces is the container's cgroup limit, which
//! charges the server *plus* the agent child *plus* anything written to a
//! memory-backed filesystem. So [`container_bytes`] is the number that matters and
//! [`process_rss_bytes`] is the control: the gap between them is the child and the
//! clone.
//!
//! Logging the container reading either side of the clone also answers, by
//! observation, whether the checkout is charged to memory on a platform — an
//! assumption worth testing rather than inheriting. One deployment found its
//! container ~5× oversized this way and raised its concurrency on evidence.
//! **Those numbers do not transfer** — a different repo means a different clone —
//! but the method does.
//!
//! Everything here is best-effort and returns `None` off Linux, so local macOS
//! development is unaffected rather than broken.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How often [`HighWater`] samples. A review runs for minutes and the spike we care
/// about (the agent's context assembly) lasts seconds, so this is fast enough to
/// catch the peak and far too slow to cost anything.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

/// Linux reports `statm` in pages. Every platform this ships on uses 4 KiB, and
/// getting it from `sysconf` would mean a libc dependency for a constant that has
/// not changed on x86-64/arm64 Linux — if that ever stops being true, the parent
/// figure is wrong by a fixed factor and the cgroup number, which is in bytes, is
/// unaffected.
const PAGE_BYTES: u64 = 4096;

/// Total memory charged to this container, in bytes — the figure the kernel
/// enforces and the one an OOM kill is measured against.
///
/// Tries cgroup v2 (what Cloud Run gen2 and every current distro use) and falls
/// back to v1 (what a Fly.io Firecracker microVM exposes). Returns `None` anywhere
/// neither exists, which includes macOS.
///
/// # ⚠️ This is the WHOLE container, not one review
///
/// Every concurrently-running review reads the same number, so with
/// more than one review in flight each one records the **combined** peak against its own
/// `ReviewEvent`. Two reviews running together on 2026-08-14 logged 1623 MB and
/// 1619 MB — not two reviews costing ~1620 MB each, one container costing 1623 MB
/// for both.
///
/// Two consequences for anyone sizing from these numbers:
///
/// - A per-review cost cannot be read off a single row. It is a **difference**
///   between samples at different concurrency levels, and the shared baseline has
///   to be subtracted once, not once per review.
/// - A single-review sample is not enough to project upward. Deriving "~385 MB per
///   review" from one review at 934 MB understated the real marginal cost by ~40%:
///   two concurrent turned out to be 1623 MB, not the projected 1319 MB. Two
///   concurrent is the smallest sample that reveals a marginal cost at all.
///
/// Also note the reading includes reclaimable **page cache**, which the kernel
/// would drop under pressure rather than OOM on. So it is a conservative ceiling,
/// not the live working set; `memory.stat` would separate anon from cache.
pub fn container_bytes() -> Option<u64> {
    // v2 first: a unified hierarchy exposes `memory.current` at the cgroup root as
    // seen from inside the container.
    const PATHS: [&str; 2] = [
        "/sys/fs/cgroup/memory.current",
        "/sys/fs/cgroup/memory/memory.usage_in_bytes",
    ];
    PATHS.iter().find_map(|p| read_u64(p))
}

/// Above this, a "limit" is a sentinel rather than a real cap.
///
/// cgroup v1 spells *unlimited* as `i64::MAX` rounded down to a page boundary
/// (`0x7FFF_FFFF_FFFF_F000` — about 8 EiB). That value is *below* `u64::MAX / 2`,
/// so the obvious half-of-u64 guard lets it through and we would report a
/// multi-exabyte limit with every real reading rounding to 0%. No container is
/// provisioned anywhere near a tebibyte, so this rejects the sentinel by being
/// far under it rather than by matching its exact bit pattern, which differs by
/// page size.
const SANE_LIMIT_CEILING: u64 = 1 << 40; // 1 TiB

/// Is this a real memory cap, or a kernel sentinel meaning "unlimited"?
///
/// Split out from [`container_limit_bytes`] so the boundary is testable without a
/// cgroup filesystem — the bug this replaced was purely arithmetic and invisible
/// on the dev machine, where neither path exists.
fn plausible_limit(v: u64) -> bool {
    v > 0 && v < SANE_LIMIT_CEILING
}

/// The cgroup's memory *limit*, so a reading can be reported as a fraction of what
/// is actually available rather than as a bare number nobody can size against.
pub fn container_limit_bytes() -> Option<u64> {
    const PATHS: [&str; 2] = [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ];
    // cgroup v2 writes the literal string "max" when unlimited, which fails to
    // parse and is filtered here; v1 writes the sentinel SANE_LIMIT_CEILING guards
    // against. Neither is a useful denominator.
    PATHS
        .iter()
        .find_map(|p| read_u64(p))
        .filter(|&v| plausible_limit(v))
        // Fall back to the machine's total RAM. This is what makes the figure
        // meaningful on Fly, which was measured on 2026-08-14: a Machine is a
        // Firecracker microVM, so the cgroup v2 paths do not exist at all and
        // `memory.limit_in_bytes` reads 9223372036854771712 — the v1 "unlimited"
        // sentinel, correctly rejected above. Without this the log would report a
        // peak with nothing to compare it against.
        //
        // Ordered after the cgroup, never before it. Inside a *container* MemTotal
        // is the HOST's RAM, which would be a wildly wrong denominator — so this
        // only applies where no cgroup limit exists, which is exactly the case
        // where the VM's RAM genuinely is the limit.
        .or_else(mem_total_bytes)
}

/// Total RAM as the kernel sees it, from `/proc/meminfo`.
///
/// Only a valid stand-in for a memory limit when the kernel is the machine's own —
/// see the caller. Reported in kB by the kernel.
fn mem_total_bytes() -> Option<u64> {
    let raw = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = raw
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kb * 1024)
}

/// Resident set of *this process only*, in bytes. The control reading: subtract it
/// from [`container_bytes`] to attribute the rest to the agent child and the clone.
pub fn process_rss_bytes() -> Option<u64> {
    // Field 2 of /proc/self/statm is the resident set in pages.
    let raw = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = raw.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * PAGE_BYTES)
}

/// Bytes to whole MB, for logging and the status ring. Saturating rather than
/// panicking: an observability path must never be able to take a review down.
///
/// ```
/// assert_eq!(pr_review_core::memory::as_mb(3 * 1024 * 1024), 3);
/// ```
pub fn as_mb(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

/// A running maximum of [`container_bytes`], sampled on a background task.
///
/// A single reading taken after a review finishes is worthless — by then the child
/// has exited and the peak is gone. This samples across the whole run and keeps the
/// maximum, which is the only figure that can be compared against the memory limit.
///
/// **Stops itself when dropped.** Dropping a `JoinHandle` does not cancel the task
/// behind it, so relying on [`stop`](Self::stop) alone leaks a sampler whenever the
/// holder never reaches it — and the review path has a real way to not reach it: an
/// axum handler future is cancelled if the client disconnects, which for a Cloud
/// Tasks dispatch happens on the 30-minute deadline. Each leak is an immortal
/// 500 ms tick on a long-lived instance.
///
/// This is the same trap as `kill_on_drop` on an agent child process: in Tokio,
/// dropping the handle does not stop the work.
pub struct HighWater {
    peak: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

impl HighWater {
    /// Begin sampling. Cheap enough to start unconditionally: off Linux every
    /// reading is `None`, the peak stays 0, and [`stop`](Self::stop) reports `None`.
    /// Needs a Tokio runtime, since the sampler is a spawned task.
    ///
    /// ```
    /// # tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(async {
    /// let mem = pr_review_core::memory::HighWater::start();
    /// // ... run the review ...
    /// let peak: Option<u64> = mem.stop(); // None off Linux
    /// # let _ = peak;
    /// # });
    /// ```
    pub fn start() -> Self {
        let peak = Arc::new(AtomicU64::new(0));
        let handle = Arc::clone(&peak);
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
            loop {
                ticker.tick().await;
                if let Some(now) = container_bytes() {
                    handle.fetch_max(now, Ordering::Relaxed);
                }
            }
        });
        Self { peak, task }
    }

    /// Stop sampling and return the peak in bytes, or `None` if nothing was ever
    /// read (not Linux, or the review was shorter than the first tick).
    ///
    /// Aborting here is belt-and-braces — [`Drop`] does it too, so a caller that
    /// never reaches this line still stops the sampler. `abort` is idempotent.
    pub fn stop(self) -> Option<u64> {
        self.task.abort();
        match self.peak.load(Ordering::Relaxed) {
            0 => None,
            v => Some(v),
        }
    }
}

impl Drop for HighWater {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Total on-disk size of a directory tree, following no symlinks — used to log how
/// much the clone actually costs, which is otherwise assumed rather than known.
///
/// Errors are swallowed per entry: a partially-readable clone should still yield a
/// usable figure rather than nothing.
pub fn dir_size_bytes(root: &std::path::Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

/// Read a file whose entire contents are one integer.
fn read_u64(path: &str) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readings_are_optional_and_never_panic() {
        // On Linux these are Some; on macOS None. Either way the call must not
        // panic — this runs on both in CI and on the dev machine.
        let _ = container_bytes();
        let _ = container_limit_bytes();
        let _ = process_rss_bytes();
    }

    /// The exact value cgroup v1 writes for "unlimited": `i64::MAX` rounded down to
    /// a 4 KiB page.
    const V1_UNLIMITED: u64 = 0x7FFF_FFFF_FFFF_F000;

    /// The trap, proven at compile time: the sentinel is 4095 *below* `u64::MAX / 2`,
    /// so the obvious half-of-u64 guard passes it and reports an 8 EiB limit against
    /// which every real reading rounds to 0%.
    const _: () = assert!(V1_UNLIMITED < u64::MAX / 2);

    #[test]
    fn rejects_the_cgroup_v1_unlimited_sentinel() {
        assert!(!plausible_limit(V1_UNLIMITED));
        // Larger page sizes shift the sentinel; the ceiling must reject those too.
        assert!(!plausible_limit(i64::MAX as u64));
        assert!(!plausible_limit(0x7FFF_FFFF_FFFF_0000));
        assert!(!plausible_limit(u64::MAX));
    }

    #[test]
    fn the_fly_v1_sentinel_is_rejected_and_falls_back() {
        // Measured on the deployed Fly Machine, 2026-08-14: cgroup v2 paths do not
        // exist (Firecracker microVM) and memory.limit_in_bytes reads exactly this.
        // It is the value the sibling bot's reviewer caught being let through by a
        // `v < u64::MAX / 2` guard — not a hypothetical, this platform emits it.
        assert!(!plausible_limit(9_223_372_036_854_771_712));
        // With the cgroup limit unusable, the reported limit must come from
        // somewhere real or the peak has no denominator. On Linux that is MemTotal.
        #[cfg(target_os = "linux")]
        assert!(mem_total_bytes().is_some_and(plausible_limit));
        // Off Linux there is no /proc; the call must degrade rather than panic.
        let _ = mem_total_bytes();
    }

    #[test]
    fn accepts_limits_we_actually_deploy() {
        for gib in [1, 2, 4, 8, 32] {
            let bytes = gib * 1024 * 1024 * 1024;
            assert!(plausible_limit(bytes), "{gib} GiB should be a real limit");
        }
        // Zero is not a limit; it would also be a division hazard downstream.
        assert!(!plausible_limit(0));
    }

    #[test]
    fn converts_bytes_to_mb() {
        assert_eq!(as_mb(0), 0);
        assert_eq!(as_mb(1024 * 1024), 1);
        assert_eq!(as_mb(1536 * 1024), 1); // truncates, never rounds up
        assert_eq!(as_mb(4 * 1024 * 1024 * 1024), 4096);
    }

    #[test]
    fn sums_a_directory_tree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a"), vec![0u8; 100]).unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub/b"), vec![0u8; 250]).unwrap();
        assert_eq!(dir_size_bytes(tmp.path()), 350);
    }

    #[test]
    fn missing_directory_sums_to_zero() {
        assert_eq!(
            dir_size_bytes(std::path::Path::new("/nonexistent-path-for-test")),
            0
        );
    }

    #[tokio::test]
    async fn dropping_a_sampler_stops_it() {
        // The regression test for the missing Drop impl. A cancelled /review handler
        // never reaches stop(), and dropping a JoinHandle does not cancel the task —
        // so without Drop each cancelled review left an immortal 500ms tick behind.
        // Asserting `is_finished()` rather than the mere existence of a Drop impl,
        // since only the former would have failed before the fix.
        let hw = HighWater::start();
        let handle = hw.task.abort_handle();
        assert!(!handle.is_finished(), "sampler should be running");
        drop(hw);
        for _ in 0..40 {
            if handle.is_finished() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("sampler survived being dropped — the Drop impl is not aborting it");
    }

    #[tokio::test]
    async fn high_water_stops_cleanly() {
        let hw = HighWater::start();
        tokio::time::sleep(Duration::from_millis(20)).await;
        // Whatever it read (Some on Linux, None on macOS), stopping must not hang
        // or panic — the review path calls this on every run.
        let _ = hw.stop();
    }
}
