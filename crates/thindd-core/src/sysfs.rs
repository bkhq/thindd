//! Temporary block-device tuning.
//!
//! Linux only. The knobs live in `sysfs`, which no other platform has; on
//! everything else [`BdevTuning::apply`] is a no-op that reports success,
//! because there is genuinely nothing to do rather than something we failed at.
//!
//! Two sysfs knobs make a large sequential write to a USB stick or SD card
//! behave much better, and upstream `bmaptool` sets both:
//!
//! * `queue/scheduler` → `none`. Sequential bulk writes gain nothing from
//!   reordering, and the fair-queueing schedulers actively slow them down.
//! * `bdi/max_ratio` → `1`. Without this the kernel happily fills most of RAM
//!   with dirty pages destined for a 10 MB/s device, and the whole machine
//!   becomes unresponsive.
//!
//! Both are best-effort: an unprivileged user cannot write them, and that is
//! fine — the copy just runs with the defaults. The original values are put
//! back on drop.

#[cfg(target_os = "linux")]
use std::path::Path;
use std::{fmt, path::PathBuf};

/// Restores the block-device knobs it changed when dropped.
pub struct BdevTuning {
    restore: Vec<(PathBuf, String)>,
    /// Knobs we wanted but could not set, for one consolidated warning. Always
    /// empty where the platform has no such knobs at all.
    pub failed: Vec<String>,
}

impl fmt::Debug for BdevTuning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BdevTuning")
            .field("restore", &self.restore.iter().map(|(p, _)| p).collect::<Vec<_>>())
            .field("failed", &self.failed)
            .finish()
    }
}

/// Where the kernel exposes per-device knobs, keyed by device number.
#[cfg(target_os = "linux")]
const SYSFS_DEV_BLOCK: &str = "/sys/dev/block";

impl BdevTuning {
    /// Tune the block device identified by `rdev`, remembering what to undo.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub fn apply(rdev: u64) -> Self {
        Self::apply_under(Path::new(SYSFS_DEV_BLOCK), rdev)
    }

    /// No block-device knobs exist off Linux, so there is nothing to tune and
    /// nothing to restore. Reported as complete, not as a failure.
    #[cfg(not(target_os = "linux"))]
    #[must_use]
    pub const fn apply(_rdev: u64) -> Self {
        Self { restore: Vec::new(), failed: Vec::new() }
    }

    /// [`BdevTuning::apply`] against an arbitrary `/sys/dev/block` root, so the
    /// directory-shape logic can be tested without a real block device.
    #[cfg(target_os = "linux")]
    fn apply_under(root: &Path, rdev: u64) -> Self {
        let mut tuning = Self { restore: Vec::new(), failed: Vec::new() };
        let Some(base) = sysfs_base(root, rdev) else {
            tuning.failed.push("sysfs entry not found".to_owned());
            return tuning;
        };
        // Worth a line of its own: for a partition this is the *parent disk*,
        // and seeing which device the knobs actually landed on is the first
        // thing anyone debugging a slow flash wants to know.
        tracing::debug!(
            dev = %format!("{}:{}", rustix::fs::major(rdev), rustix::fs::minor(rdev)),
            base = %base.display(),
            "resolved block-device sysfs base"
        );

        tuning.set(&base.join("queue/scheduler"), "none", parse_active_scheduler);
        tuning.set(&base.join("bdi/max_ratio"), "1", |s| Some(s.trim().to_owned()));
        tuning
    }

    /// `true` when every knob was applied.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.failed.is_empty()
    }

    #[cfg(target_os = "linux")]
    fn set(&mut self, path: &Path, value: &str, extract: fn(&str) -> Option<String>) {
        let previous = std::fs::read_to_string(path).ok().and_then(|s| extract(&s));
        match std::fs::write(path, value) {
            Ok(()) => {
                if let Some(previous) = previous {
                    self.restore.push((path.to_path_buf(), previous));
                }
                tracing::debug!(knob = %path.display(), value, "tuned block device");
            }
            Err(e) => self.failed.push(format!("{}: {e}", path.display())),
        }
    }
}

impl Drop for BdevTuning {
    fn drop(&mut self) {
        for (path, value) in self.restore.drain(..) {
            if let Err(e) = std::fs::write(&path, &value) {
                tracing::warn!(knob = %path.display(), error = %e, "could not restore sysfs knob");
            }
        }
    }
}

#[cfg(target_os = "linux")]
/// Locate `<root>/<major>:<minor>/`, walking up to the parent disk when the
/// device is a partition.
///
/// Partitions have no `queue/` directory of their own — the knobs live on the
/// whole disk. `/sys/dev/block/<maj>:<min>` is a symlink into
/// `/sys/devices/…/<disk>/<partition>`, so appending `..` lands on the disk once
/// the kernel resolves the link; no string surgery on device names is needed.
fn sysfs_base(root: &Path, rdev: u64) -> Option<PathBuf> {
    let major = rustix::fs::major(rdev);
    let minor = rustix::fs::minor(rdev);
    let base = root.join(format!("{major}:{minor}"));
    if !base.exists() {
        return None;
    }
    let resolved = if base.join("queue").is_dir() { base } else { base.join("..") };
    if !resolved.join("queue").is_dir() {
        return None;
    }
    // Resolve the symlink and the `..` so logs and errors name the device the
    // knobs belong to, not the path we happened to walk there by.
    Some(std::fs::canonicalize(&resolved).unwrap_or(resolved))
}

/// `mq-deadline kyber [bfq] none` → `bfq`.
#[cfg(target_os = "linux")]
fn parse_active_scheduler(contents: &str) -> Option<String> {
    contents
        .split_whitespace()
        .find_map(|w| w.strip_prefix('[').and_then(|w| w.strip_suffix(']')))
        .map(str::to_owned)
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;

    #[test]
    fn active_scheduler_is_the_bracketed_one() {
        assert_eq!(
            parse_active_scheduler("mq-deadline kyber [bfq] none\n").as_deref(),
            Some("bfq")
        );
        assert_eq!(parse_active_scheduler("[none]\n").as_deref(), Some("none"));
        assert_eq!(parse_active_scheduler("none\n"), None);
    }

    #[test]
    fn missing_device_yields_a_failure_note_but_no_panic() {
        // Major 0 / minor 0 never has a sysfs entry.
        let tuning = BdevTuning::apply(0);
        assert!(!tuning.is_complete());
    }

    /// Build a fake sysfs: a whole disk carrying the knobs, a partition of it
    /// carrying none, and `dev/block/<maj>:<min>` symlinks into both — the same
    /// shape the kernel presents.
    fn fake_sysfs() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("devices/nvme0n1");
        let part = disk.join("nvme0n1p1");
        std::fs::create_dir_all(disk.join("queue")).unwrap();
        std::fs::create_dir_all(disk.join("bdi")).unwrap();
        std::fs::create_dir_all(&part).unwrap();
        std::fs::write(disk.join("queue/scheduler"), "[mq-deadline] none\n").unwrap();
        std::fs::write(disk.join("bdi/max_ratio"), "100\n").unwrap();

        let dev_block = dir.path().join("dev/block");
        std::fs::create_dir_all(&dev_block).unwrap();
        std::os::unix::fs::symlink(&disk, dev_block.join("259:0")).unwrap();
        std::os::unix::fs::symlink(&part, dev_block.join("259:1")).unwrap();
        dir
    }

    #[test]
    fn a_whole_disk_uses_its_own_knobs() {
        let dir = fake_sysfs();
        let root = dir.path().join("dev/block");
        let base = sysfs_base(&root, rustix::fs::makedev(259, 0)).unwrap();
        assert!(base.join("queue/scheduler").is_file());
    }

    #[test]
    fn a_partition_walks_up_to_the_parent_disk() {
        let dir = fake_sysfs();
        let root = dir.path().join("dev/block");
        let base = sysfs_base(&root, rustix::fs::makedev(259, 1)).unwrap();
        assert!(base.join("queue/scheduler").is_file(), "did not reach the disk's queue/");
        // The returned path names the disk directly — no `..` left in it, so a
        // log line says which device was tuned.
        assert!(!base.to_string_lossy().contains(".."), "unresolved path: {}", base.display());
        assert_eq!(base, std::fs::canonicalize(dir.path().join("devices/nvme0n1")).unwrap());
    }

    #[test]
    fn tuning_a_partition_writes_and_restores_the_parent_disk_knobs() {
        let dir = fake_sysfs();
        let root = dir.path().join("dev/block");
        let sched = dir.path().join("devices/nvme0n1/queue/scheduler");
        let ratio = dir.path().join("devices/nvme0n1/bdi/max_ratio");

        {
            let tuning = BdevTuning::apply_under(&root, rustix::fs::makedev(259, 1));
            assert!(tuning.is_complete(), "not applied: {:?}", tuning.failed);
            assert_eq!(std::fs::read_to_string(&sched).unwrap(), "none");
            assert_eq!(std::fs::read_to_string(&ratio).unwrap(), "1");
        }

        // The guard has dropped: the bracketed scheduler and the ratio are back.
        assert_eq!(std::fs::read_to_string(&sched).unwrap(), "mq-deadline");
        assert_eq!(std::fs::read_to_string(&ratio).unwrap(), "100");
    }

    #[test]
    fn an_unknown_device_number_finds_no_base() {
        let dir = fake_sysfs();
        let root = dir.path().join("dev/block");
        assert!(sysfs_base(&root, rustix::fs::makedev(259, 9)).is_none());
    }
}
