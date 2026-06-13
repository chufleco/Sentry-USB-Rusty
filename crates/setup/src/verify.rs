//! Pre-setup sanity checks — port of `verify-configuration.sh`.
//!
//! Split into three phases so we can bail loudly on conditions we can
//! detect up-front without also false-failing on conditions that the
//! setup wizard is *about* to fix:
//!
//!   * [`early_verify`] — hardware model, XFS+reflink support,
//!     required config keys. Runs BEFORE any destructive operation
//!     and BEFORE the dwc2 overlay phase. Checks that are safe to
//!     run on a stock Pi OS image without any SentryUSB-specific
//!     kernel modules loaded yet.
//!   * [`verify_udc`] — at least one UDC driver exposed under
//!     `/sys/class/udc/`. MUST run **after** the dwc2 overlay phase
//!     has completed (either "already set" or "just added + rebooted
//!     + resuming"). On a fresh Pi OS image `dtoverlay=dwc2` isn't
//!     in `config.txt` yet, so `/sys/class/udc/` is empty — the
//!     check would always false-fail on the very first pass.
//!   * [`verify_disk_space`] — SD card or USB drive has enough room
//!     for the backing-files partition. MUST run **after** the root
//!     shrink phase, because on a fresh Pi OS install the root
//!     partition fills the entire disk and the `sfdisk -F` query
//!     would report 0 bytes free. The shrink is what creates the
//!     8 GB we need; checking before it runs is a false-fail.
//!
//! On failure the returned `anyhow::Error` is logged and the runner
//! aborts before touching the filesystem.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::env::{PiModel, SetupEnv};
use crate::error::ConfigError;
use crate::SetupEmitter;

/// Minimum usable space on the SD card (8 GiB) after root-partition shrink.
/// Older code required 32 GiB which blocked anything under a ~38 GB card
/// even though the actual footprint is ~8 GB.
const MIN_SD_SPACE_BYTES: u64 = 8 * (1 << 30);

/// Minimum total size of an external USB drive (59 GiB, rounded to match
/// the bash threshold).
const MIN_USB_SIZE_BYTES: u64 = 59 * (1 << 30);

/// Early sanity checks: hardware, XFS, config vars. Call before the
/// dwc2 overlay phase. Deliberately excludes checks that depend on
/// kernel state the overlay/shrink phases will establish — see
/// [`verify_udc`] and [`verify_disk_space`] for those.
pub async fn early_verify(env: &SetupEnv, emitter: &SetupEmitter) -> Result<()> {
    // Announce the phase up-front. The XFS loopback check inside
    // `check_xfs_support` typically takes 30-60s (xfsprogs install on
    // fresh Pi OS images + the 1 GB truncate/mkfs/mount probe) and
    // without this the wizard's phase list sits empty for that whole
    // window — the user sees no progress even though we're actively
    // working. Idempotent: the phase is logged once per setup run.
    emitter.begin_phase("verify", "Verifying configuration");
    check_supported_hardware(env)?;
    check_xfs_support(emitter).await?;
    check_required_config(env)?;
    Ok(())
}

/// UDC driver presence check. Call **after** the dwc2 overlay phase has
/// completed (either the overlay was already in `config.txt`, or we
/// added it and are now resuming post-reboot with it loaded). Fails
/// loudly so we don't proceed into partition/gadget phases that assume
/// the USB gadget will come up.
pub fn verify_udc() -> Result<()> {
    check_udc()
}

/// Disk-space availability check. Call **after** the root shrink phase
/// has completed, because on a fresh Pi OS image root fills the whole
/// disk and there's zero unpartitioned space until shrink runs. On
/// repeat runs the fast-path (backingfiles/mutable labels already
/// present) makes this a cheap O(1) query.
pub async fn verify_disk_space(env: &SetupEnv, emitter: &SetupEmitter) -> Result<()> {
    check_available_space(env, emitter).await
}

// -----------------------------------------------------------------------------
// Individual checks
// -----------------------------------------------------------------------------

fn check_supported_hardware(env: &SetupEnv) -> Result<()> {
    // Not-a-Pi skips the check entirely — matches bash: non-Pi boards
    // (RockPi, Radxa) are handled by other setup paths and aren't our
    // problem here. Pi 2 has no USB gadget hardware; Pi Zero W (the
    // original armv6 board) was dropped in 2026 — too underpowered to
    // run the daemon comfortably, and the armv6 build was retired to
    // keep release artifact counts manageable.
    match env.pi_model {
        PiModel::Pi5 | PiModel::Pi4 | PiModel::Pi3 | PiModel::PiZero2 => {
            Ok(())
        }
        PiModel::PiZeroW => bail!(
            "STOP: unsupported hardware: Raspberry Pi Zero W. \
             SentryUSB requires Pi Zero 2 W or newer (Pi 3, Pi 4, Pi 5)."
        ),
        PiModel::Pi2 => bail!(
            "STOP: unsupported hardware: Raspberry Pi 2. \
             (only Pi Zero 2 W, Pi 3, Pi 4, and Pi 5 have the necessary hardware to run SentryUSB)"
        ),
        PiModel::Other => {
            // Could be a RockPi / Radxa Zero / genuinely unknown board.
            // Bash returns silently for non-Pi boards; we do the same.
            Ok(())
        }
    }
}

fn check_udc() -> Result<()> {
    let udc_dir = Path::new("/sys/class/udc");
    // Count symlinks under /sys/class/udc/. Bash uses `find -type l`.
    let count = match std::fs::read_dir(udc_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_symlink()).unwrap_or(false))
            .count(),
        Err(_) => 0,
    };
    if count == 0 {
        let model = std::fs::read_to_string("/sys/firmware/devicetree/base/model")
            .unwrap_or_default()
            .replace('\0', "");
        bail!(
            "STOP: this device ({}) does not have a UDC driver. \
             Check that dtoverlay=dwc2 is in the correct section of config.txt for your Pi model",
            model.trim()
        );
    }
    Ok(())
}

/// Filesystem chosen for the backing-files partition (where the disk-image
/// files and snapshots live). Picked by [`probe_backing_fs`] from what the
/// running kernel can actually mount.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackingFs {
    /// XFS with reflink — copy-on-write snapshots (fastest, least disk use).
    XfsReflink,
    /// Btrfs — also copy-on-write (cheap snapshots via `cp --reflink`). The CoW
    /// fallback for kernels that lack XFS but ship Btrfs (very common, e.g. the
    /// Allwinner A733 BSP kernel has `CONFIG_BTRFS_FS=y` but no XFS). Keeps
    /// snapshots cheap so small drives don't lose their RecentClips budget to
    /// full-copy snapshots. The big disk-image files are marked NoCOW
    /// (`chattr +C`) at creation to avoid random-write fragmentation —
    /// reflink snapshots still work on NoCOW Btrfs files.
    Btrfs,
    /// XFS without reflink/bigtime/inobtcount — broad kernel compat, snapshots
    /// become full copies.
    XfsPlain,
    /// ext4 — last resort for kernels with neither XFS nor Btrfs. Snapshots
    /// become full copies.
    Ext4,
}

impl BackingFs {
    /// blkid/fstab filesystem type string.
    pub fn fstype(self) -> &'static str {
        match self {
            BackingFs::Ext4 => "ext4",
            BackingFs::Btrfs => "btrfs",
            _ => "xfs",
        }
    }

    /// mkfs binary used to format this filesystem.
    pub fn mkfs_bin(self) -> &'static str {
        match self {
            BackingFs::Ext4 => "mkfs.ext4",
            BackingFs::Btrfs => "mkfs.btrfs",
            _ => "mkfs.xfs",
        }
    }

    /// True if this filesystem gives copy-on-write (cheap reflink snapshots).
    pub fn is_cow(self) -> bool {
        matches!(self, BackingFs::XfsReflink | BackingFs::Btrfs)
    }

    /// mkfs args to format `dev` (a block device or file) with the
    /// `backingfiles` label. `-K`/`-f` skip the slow full-device TRIM and
    /// force-overwrite an existing signature.
    pub fn mkfs_args(self, dev: &str) -> Vec<String> {
        let v: Vec<&str> = match self {
            BackingFs::XfsReflink => vec!["-f", "-K", "-m", "reflink=1", "-L", "backingfiles", dev],
            BackingFs::XfsPlain => vec!["-f", "-K", "-m", "reflink=0,bigtime=0,inobtcount=0", "-L", "backingfiles", dev],
            BackingFs::Btrfs => vec!["-f", "-L", "backingfiles", dev],
            BackingFs::Ext4 => vec!["-F", "-L", "backingfiles", dev],
        };
        v.into_iter().map(String::from).collect()
    }

    pub fn human(self) -> &'static str {
        match self {
            BackingFs::XfsReflink => "XFS with reflink (copy-on-write snapshots)",
            BackingFs::Btrfs => "Btrfs (copy-on-write snapshots — no XFS in this kernel)",
            BackingFs::XfsPlain => "XFS without reflink (full-copy snapshots)",
            BackingFs::Ext4 => "ext4 (full-copy snapshots — no XFS/Btrfs in this kernel)",
        }
    }
}

/// Probe what the running kernel can actually mount for the backing-files
/// partition, in preference order: reflink XFS → plain XFS → ext4. Never
/// fails — ext4 is supported by every Pi-class kernel, so there is always a
/// usable answer. This replaces the old hard "must mount reflink XFS or STOP"
/// gate, which bailed on vendor SBC kernels (e.g. the Allwinner A733, whose
/// BSP kernel ships no XFS driver at all — not even as a loadable module).
///
/// Snapshots only need reflink as an optimization: `usb_gadget/snapshot.rs`
/// uses `cp --reflink=auto`, which silently degrades to a full byte copy when
/// the filesystem can't do COW. So plain XFS / ext4 are fully functional, just
/// more I/O and disk on each snapshot.
pub async fn probe_backing_fs(emitter: &SetupEmitter) -> BackingFs {
    // Make sure mkfs.xfs exists before we test XFS candidates. Best-effort —
    // if the install fails (or the board has no XFS), we just fall through to
    // ext4. The apt fetch is the slow step on a fresh image, so announce it.
    if sentryusb_shell::run("which", &["mkfs.xfs"]).await.is_err() {
        emitter.progress("Installing xfsprogs (this can take 30-60 seconds)...");
        let _ = crate::apt::apt_install(
            |m| emitter.progress(m),
            &["xfsprogs"],
            Duration::from_secs(180),
        ).await;
    }
    let have_xfs_tools = sentryusb_shell::run("which", &["mkfs.xfs"]).await.is_ok();

    // Same for Btrfs tools — the CoW fallback when XFS is absent from the
    // kernel. Best-effort install; if it fails we just skip the Btrfs candidate.
    if sentryusb_shell::run("which", &["mkfs.btrfs"]).await.is_err() {
        emitter.progress("Installing btrfs-progs...");
        let _ = crate::apt::apt_install(
            |m| emitter.progress(m),
            &["btrfs-progs"],
            Duration::from_secs(180),
        ).await;
    }
    let have_btrfs_tools = sentryusb_shell::run("which", &["mkfs.btrfs"]).await.is_ok();

    let img = "/tmp/fsprobe.img";
    let mnt = "/tmp/fsprobemnt";
    let _ = sentryusb_shell::run("umount", &[mnt]).await;
    let _ = std::fs::remove_file(img);
    let _ = std::fs::remove_dir_all(mnt);
    let _ = std::fs::create_dir_all(mnt);

    // 1 GB sparse image — metadata-only truncate, near-instant.
    if sentryusb_shell::run_with_timeout(Duration::from_secs(30), "truncate", &["-s", "1GB", img]).await.is_err() {
        // Can't even create a scratch image — assume ext4 (universally mountable).
        let _ = std::fs::remove_dir_all(mnt);
        emitter.progress(&format!("Backing filesystem: {}", BackingFs::Ext4.human()));
        return BackingFs::Ext4;
    }

    // Format `img` with `mkfs_bin mkfs_args`, then try to mount it. Returns
    // true only if BOTH the format and the mount succeed — a kernel can have
    // mkfs.xfs (userspace) yet no XFS driver (kernel), so the mount is the
    // real test.
    async fn format_and_mount(img: &str, mnt: &str, bin: &str, args: &[&str]) -> bool {
        if sentryusb_shell::run_with_timeout(Duration::from_secs(30), bin, args).await.is_err() {
            return false;
        }
        let mounted = sentryusb_shell::run("mount", &[img, mnt]).await.is_ok();
        if mounted {
            let _ = sentryusb_shell::run("umount", &[mnt]).await;
        }
        mounted
    }

    // Preference order: reflink XFS → Btrfs → plain XFS → ext4. The first two
    // give copy-on-write (cheap snapshots); we only drop to a full-copy
    // filesystem (plain XFS / ext4) when no CoW option mounts on this kernel.
    let chosen = if have_xfs_tools
        && format_and_mount(img, mnt, "mkfs.xfs", &["-q", "-f", "-m", "reflink=1", img]).await
    {
        BackingFs::XfsReflink
    } else if have_btrfs_tools
        && format_and_mount(img, mnt, "mkfs.btrfs", &["-q", "-f", img]).await
    {
        BackingFs::Btrfs
    } else if have_xfs_tools
        && format_and_mount(img, mnt, "mkfs.xfs", &["-q", "-f", "-m", "reflink=0,bigtime=0,inobtcount=0", img]).await
    {
        BackingFs::XfsPlain
    } else {
        BackingFs::Ext4
    };

    let _ = std::fs::remove_file(img);
    let _ = std::fs::remove_dir_all(mnt);
    emitter.progress(&format!("Backing filesystem: {}", chosen.human()));
    chosen
}

/// Early-verify hook: probe the backing filesystem so the choice (and any
/// xfsprogs install) happens up front in the "verify" phase. Never STOPs —
/// see [`probe_backing_fs`]. partition.rs re-probes at format time and uses
/// the same cascade, so this is informational/warm-up.
async fn check_xfs_support(emitter: &SetupEmitter) -> Result<()> {
    let _ = probe_backing_fs(emitter).await;
    Ok(())
}

fn check_required_config(env: &SetupEnv) -> Result<()> {
    // Bash bails if CAM_SIZE isn't set at all. In Rust the config already
    // has a default of "0" (unset/zero triggers the SD fallback), so an
    // explicitly empty or literal-0 CAM_SIZE still runs the setup — but
    // a truly missing key is a user-config error we should surface.
    if !env.config.contains_key("CAM_SIZE") {
        // User-config error (a missing key fails identically on retry) →
        // ConfigError so the boot-loop auto-resume halts and surfaces it.
        return Err(ConfigError(
            "STOP: Define the variable CAM_SIZE in sentryusb.conf like this: \
             export CAM_SIZE=32"
                .into(),
        )
        .into());
    }
    Ok(())
}

async fn check_available_space(env: &SetupEnv, emitter: &SetupEmitter) -> Result<()> {
    match env.data_drive.as_deref() {
        None => {
            emitter.progress("DATA_DRIVE is not set. SD card will be used.");
            check_available_space_sd(env, emitter).await
        }
        Some(drive) if Path::new(drive).exists() => {
            emitter.progress(&format!(
                "DATA_DRIVE is set to {}. This will be used for /mutable and /backingfiles.",
                drive
            ));
            check_available_space_usb(drive, emitter).await
        }
        // Keep a missing DATA_DRIVE TRANSIENT (not ConfigError): env.data_drive
        // is the raw config value with no existence check (env.rs), and this is
        // the first existence gate. A USB/SSD that's just slow to enumerate — or
        // not back yet after a mid-setup reboot, realistic on a brownout-prone
        // Pi — must self-heal via the auto-resume retry rather than halt setup
        // with a "fix your config" wall. A genuine typo only loops (pre-existing
        // behavior); a transient absence recovers, which is the safer trade.
        Some(drive) => bail!(
            "STOP: DATA_DRIVE is set to {}, which does not exist.",
            drive
        ),
    }
}

async fn check_available_space_sd(env: &SetupEnv, emitter: &SetupEmitter) -> Result<()> {
    emitter.progress("Verifying that there is sufficient space available on the MicroSD card...");

    // Fast path: partitions already exist from a previous run.
    let backingfiles_dev = "/dev/disk/by-label/backingfiles";
    let mutable_dev = "/dev/disk/by-label/mutable";
    if Path::new(backingfiles_dev).exists() && Path::new(mutable_dev).exists() {
        let size_output = sentryusb_shell::run(
            "blockdev",
            &["--getsize64", backingfiles_dev],
        )
        .await
        .context("blockdev --getsize64 backingfiles")?;
        let size: u64 = size_output.trim().parse().unwrap_or(0);
        if size < MIN_SD_SPACE_BYTES {
            bail!(
                "STOP: Existing backingfiles partition is too small ({}GB, need at least {}GB)",
                size / 1024 / 1024 / 1024,
                MIN_SD_SPACE_BYTES / 1024 / 1024 / 1024
            );
        }
        emitter.progress("There is sufficient space available.");
        return Ok(());
    }

    // Fresh partition: `sfdisk -F <disk>` reports free space. The first
    // line of the "free space" report has "XXX bytes" which we parse.
    let boot_disk = env
        .boot_disk
        .as_deref()
        .context("check_available_space_sd: BOOT_DISK is not set")?;

    let sfdisk_out =
        sentryusb_shell::run("sfdisk", &["-F", boot_disk])
            .await
            .context("sfdisk -F")?;

    // First "N bytes" match wins — matches bash `grep -o '[0-9]* bytes' | head -1`.
    let available_space = sfdisk_out
        .lines()
        .find_map(parse_bytes_from_line)
        .unwrap_or(0);

    if available_space < MIN_SD_SPACE_BYTES {
        let parted = sentryusb_shell::run("parted", &[boot_disk, "print"])
            .await
            .unwrap_or_default();
        bail!(
            "STOP: The MicroSD card is too small: {}GB available, need at least {}GB.\n{}",
            available_space / 1024 / 1024 / 1024,
            MIN_SD_SPACE_BYTES / 1024 / 1024 / 1024,
            parted
        );
    }

    emitter.progress("There is sufficient space available.");
    Ok(())
}

async fn check_available_space_usb(drive: &str, emitter: &SetupEmitter) -> Result<()> {
    emitter.progress("Verifying that there is sufficient space available on the USB drive ...");

    // 30-second timeout — a sleeping / I/O-error USB drive can hang lsblk
    // indefinitely otherwise. Match bash's explicit `timeout 30` wrapping.
    let lsblk_out = sentryusb_shell::run_with_timeout(
        Duration::from_secs(30),
        "lsblk",
        &["-pno", "TYPE", drive],
    )
    .await
    .with_context(|| {
        format!(
            "Could not read {} (drive may be unresponsive or disconnected). \
             Try unplugging and reconnecting it.",
            drive
        )
    })?;

    let drive_type = lsblk_out.lines().next().unwrap_or("").trim();
    if drive_type != "disk" {
        bail!(
            "STOP: The specified drive ({}) is not a disk (TYPE={}). \
             Please specify path to the disk.",
            drive,
            drive_type
        );
    }

    let size_out = sentryusb_shell::run_with_timeout(
        Duration::from_secs(30),
        "blockdev",
        &["--getsize64", drive],
    )
    .await
    .with_context(|| {
        format!(
            "Could not read size of {} (drive may be unresponsive). \
             Try unplugging and reconnecting it.",
            drive
        )
    })?;

    let drive_size: u64 = size_out.trim().parse().unwrap_or(0);
    if drive_size < MIN_USB_SIZE_BYTES {
        let parted = sentryusb_shell::run("parted", &[drive, "print"])
            .await
            .unwrap_or_default();
        bail!(
            "STOP: The USB drive is too small: {}GB available. Expected at least 64GB\n{}",
            drive_size / 1024 / 1024 / 1024,
            parted
        );
    }

    emitter.progress("There is sufficient space available.");
    Ok(())
}

// -----------------------------------------------------------------------------
// Parsing helpers
// -----------------------------------------------------------------------------

/// Parse the first "N bytes" occurrence on a line — e.g.
/// `Unpartitioned space /dev/mmcblk0: 10737418240 bytes, 10.7 GiB`.
fn parse_bytes_from_line(line: &str) -> Option<u64> {
    // Scan for a run of digits immediately followed by " bytes".
    let bytes_idx = line.find(" bytes")?;
    let prefix = &line[..bytes_idx];
    let digits: String = prefix
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::{PiModel, SetupEnv};
    use crate::error::ConfigError;
    use crate::SetupEmitter;
    use std::collections::HashMap;

    fn env_with(pairs: &[(&str, &str)]) -> SetupEnv {
        let config: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        SetupEnv {
            pi_model: PiModel::Other,
            boot_path: String::new(),
            cmdline_path: None,
            piconfig_path: None,
            boot_disk: None,
            root_partition: None,
            data_drive: None,
            config,
        }
    }

    #[test]
    fn missing_cam_size_is_a_config_error() {
        // A missing required key is a user-config error: it fails identically
        // on every retry, so it must classify as ConfigError (which stops the
        // setup boot-loop auto-resume) rather than a transient failure.
        let env = env_with(&[]);
        let err = check_required_config(&env).unwrap_err();
        assert!(
            err.downcast_ref::<ConfigError>().is_some(),
            "missing CAM_SIZE must be a ConfigError, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn nonexistent_data_drive_stays_transient() {
        // A missing DATA_DRIVE must NOT be a ConfigError. `env.data_drive`
        // is the raw config value with no existence check (env.rs), and this
        // is the first existence gate — a USB/SSD that's merely slow to
        // enumerate, or not back yet after a mid-setup reboot (realistic on
        // a brownout-prone Pi), must auto-resume and retry, NOT halt setup as
        // a config error. Keep it transient so it self-heals.
        let mut env = env_with(&[]);
        env.data_drive = Some("/no/such/sentryusb/drive".to_string());
        let emitter = SetupEmitter::new(|_| {}, |_, _| {});
        let err = check_available_space(&env, &emitter).await.unwrap_err();
        assert!(
            err.downcast_ref::<ConfigError>().is_none(),
            "nonexistent DATA_DRIVE must stay transient (self-heals on retry), got ConfigError: {err:?}"
        );
    }

    #[test]
    fn parse_bytes_picks_trailing_number_before_bytes() {
        // Real sfdisk output shape.
        let line = "Unpartitioned space /dev/mmcblk0: 10737418240 bytes, 10.7 GiB";
        assert_eq!(parse_bytes_from_line(line), Some(10_737_418_240));
    }

    #[test]
    fn parse_bytes_none_when_absent() {
        // " bytes" matches but no digits immediately before → None.
        assert_eq!(parse_bytes_from_line("no bytes here"), None);
        assert_eq!(
            parse_bytes_from_line("/dev/mmcblk0 30GB"),
            None,
            "no `bytes` substring → no match"
        );
    }

    #[test]
    fn parse_bytes_handles_leading_text() {
        assert_eq!(
            parse_bytes_from_line("size: 123456789 bytes total"),
            Some(123_456_789)
        );
    }
}
