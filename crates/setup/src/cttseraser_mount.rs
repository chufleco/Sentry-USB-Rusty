//! TeslaCam web mount wiring — port of `configure-web.sh`.
//!
//! Wires the bind mount of /mutable/TeslaCam at /var/www/html/TeslaCam,
//! which is where the Axum server's ServeDir route reads from. Prior
//! versions of this phase configured a cttseraser FUSE mount to strip
//! the `ctts` atom from MP4 files for browsers that couldn't parse it;
//! modern browsers (Chrome 80+, Firefox 70+, Safari iOS 13+, ExoPlayer)
//! handle the atom natively, so the FUSE layer is replaced with a
//! kernel-level bind mount for correctness, throughput, and reliability.
//!
//! The cttseraser binary and `/sbin/mount.ctts` helper are still installed
//! by the build scripts as opt-in scaffolding; this module simply does not
//! reference them by default.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::SetupEmitter;

/// Canonical fstab entry that bind-mounts the TeslaCam source tree at the
/// path the Axum ServeDir route reads from.
const FSTAB_BIND_LINE: &str =
    "/mutable/TeslaCam /var/www/html/TeslaCam none bind,nofail,x-systemd.requires=/mutable 0 0";

pub async fn configure_web_mount(emitter: &SetupEmitter) -> Result<bool> {
    // Idempotency check — if the canonical bind entry is already present
    // and no legacy cttseraser entry remains, nothing to do.
    let fstab = std::fs::read_to_string("/etc/fstab").unwrap_or_default();
    let fstab_has_bind = fstab.lines().any(|l| {
        !l.trim_start().starts_with('#')
            && l.contains("/mutable/TeslaCam")
            && l.contains("/var/www/html/TeslaCam")
            && l.contains("bind")
    });
    let fstab_has_legacy = fstab
        .lines()
        .any(|l| !l.trim_start().starts_with('#') && l.contains("mount.ctts#"));

    if fstab_has_bind && !fstab_has_legacy {
        return Ok(false);
    }

    emitter.begin_phase("web_mount", "TeslaCam mount");
    emitter.progress("configuring web (SentryUSB mode)");

    // Install runtime packages for the network status APIs. The bind mount
    // itself requires no userspace tooling beyond `mount(8)` (built-in).
    sentryusb_shell::run_with_timeout(
        Duration::from_secs(300),
        "apt-get",
        &["-y", "install", "net-tools", "wireless-tools", "ethtool"],
    ).await.context("failed to install networking runtime packages")?;

    // Nginx fight — SentryUSB owns port 80.
    if sentryusb_shell::run("systemctl", &["is-active", "--quiet", "nginx"]).await.is_ok() {
        let _ = sentryusb_shell::run("systemctl", &["stop", "nginx"]).await;
    }
    if sentryusb_shell::run("systemctl", &["is-enabled", "--quiet", "nginx"]).await.is_ok() {
        let _ = sentryusb_shell::run("systemctl", &["disable", "nginx"]).await;
    }

    // Source + target dirs.
    std::fs::create_dir_all("/mutable/TeslaCam")?;
    std::fs::create_dir_all("/var/www/html/TeslaCam")?;

    // Replace any legacy cttseraser entry with the bind-mount entry, then
    // clear systemd's cached failed state so the unit activates immediately
    // (without requiring a reboot on upgrade).
    install_bind_mount_fstab()?;
    let _ = sentryusb_shell::run("systemctl", &["daemon-reload"]).await;
    let _ = sentryusb_shell::run(
        "systemctl",
        &["reset-failed", "var-www-html-TeslaCam.mount"],
    ).await;
    let _ = sentryusb_shell::run(
        "systemctl",
        &["start", "var-www-html-TeslaCam.mount"],
    ).await;

    // (Samba reads from /mutable/TeslaCam directly, so no FUSE allow_other
    // configuration is required for it.)

    // Optional auto.www autofs for music/lightshow/boombox disk images.
    if Path::new("/backingfiles/music_disk.bin").exists()
        || Path::new("/backingfiles/lightshow_disk.bin").exists()
        || Path::new("/backingfiles/boombox_disk.bin").exists()
    {
        std::fs::create_dir_all("/var/www/html/fs")?;
        std::fs::create_dir_all("/etc/auto.master.d")?;
        std::fs::write(
            "/etc/auto.master.d/www.autofs",
            "/var/www/html/fs  /root/bin/auto.www --timeout=0\n",
        )?;
        // `zip` is used by the web UI to offer bulk download of music dirs.
        let _ = sentryusb_shell::run_with_timeout(
            Duration::from_secs(180),
            "apt-get", &["-y", "install", "zip"],
        ).await;
    }

    emitter.progress("done configuring web");
    Ok(true)
}

/// Strip any existing TeslaCam mount entry (legacy cttseraser or prior bind)
/// from /etc/fstab and add the canonical bind-mount entry.
fn install_bind_mount_fstab() -> Result<()> {
    let content = std::fs::read_to_string("/etc/fstab").unwrap_or_default();
    let kept: Vec<&str> = content
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            if t.starts_with('#') {
                return true;
            }
            // Drop any existing line that targets /var/www/html/TeslaCam.
            !l.contains("/var/www/html/TeslaCam")
        })
        .collect();
    let mut new = kept.join("\n");
    if !new.is_empty() && !new.ends_with('\n') {
        new.push('\n');
    }
    new.push_str(FSTAB_BIND_LINE);
    new.push('\n');
    std::fs::write("/etc/fstab", new)?;
    Ok(())
}
