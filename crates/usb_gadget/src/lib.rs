//! USB gadget control via Linux configfs.
//!
//! Replaces `enable_gadget.sh` and `disable_gadget.sh` with native Rust
//! operations on `/sys/kernel/config/usb_gadget/sentryusb`.

pub mod board;
pub mod early_bind;
pub mod flag;
pub mod snapshot;
pub mod space;

pub use board::{select_board, BoardId, GadgetBoard, UdcFamily};

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::info;

const GADGET_NAME: &str = "sentryusb";
// US English. Must match the form used by `run/enable_gadget.sh:13` (`0x409`)
// — the kernel parses `0x0409` and `0x409` to the same numeric langid (0x409)
// but the configfs dentry takes whichever string mkdir'd first. Boot runs the
// shell script which uses `0x409`; if Rust uses `0x0409`, `disable()` can't
// rmdir `strings/0x409` and the orphan dir pins libcomposite forever, while
// `enable()` then tries to mkdir `strings/0x0409` and the kernel rejects it
// with EEXIST because language 0x409 is already registered.
const LANG: &str = "0x409";
const CFG: &str = "c";

/// Disk images that can be exposed as USB mass storage LUNs.
const DISK_IMAGES: &[(&str, &str)] = &[
    ("/backingfiles/cam_disk.bin", "CAM"),
    ("/backingfiles/music_disk.bin", "MUSIC"),
    ("/backingfiles/lightshow_disk.bin", "LIGHTSHOW"),
    ("/backingfiles/boombox_disk.bin", "BOOMBOX"),
];

/// Find the configfs root mount point.
fn find_configfs_root() -> Result<PathBuf> {
    let mounts = fs::read_to_string("/proc/mounts")
        .context("failed to read /proc/mounts")?;
    for line in mounts.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 3 && fields[2] == "configfs" {
            return Ok(PathBuf::from(fields[1]));
        }
    }
    bail!("configfs not mounted")
}

/// Write a string to a sysfs/configfs file.
fn write_file(path: &Path, content: &str) -> Result<()> {
    fs::write(path, content)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Create `link -> target`, replacing any stale entry at `link` first.
/// Uses `symlink_metadata()` (which does NOT follow symlinks) so a dangling
/// symlink — e.g. a previous `disable()` left `configs/c.1/mass_storage.0`
/// pointing at a now-torn-down `functions/mass_storage.0` — is detected and
/// removed instead of triggering EEXIST on the recreate. The plain
/// `Path::exists()` check this replaces returned `false` for dangling links
/// because it follows the link to the missing target, then `symlink()` would
/// fail because the link path itself still exists.
#[cfg(unix)]
fn ensure_symlink(target: &Path, link: &Path) -> Result<()> {
    match link.symlink_metadata() {
        Ok(_) => fs::remove_file(link)
            .with_context(|| format!("failed to remove stale symlink {}", link.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("failed to stat {}", link.display())),
    }
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("failed to symlink {} -> {}", link.display(), target.display()))
}

#[cfg(not(unix))]
fn ensure_symlink(_target: &Path, _link: &Path) -> Result<()> {
    bail!("USB gadget control requires Linux")
}

/// Get the SBC model and return the appropriate MaxPower value (mA).
fn get_max_power() -> u32 {
    let model = fs::read_to_string("/proc/device-tree/model").unwrap_or_default();
    let model = model.to_lowercase();
    if model.contains("pi 5") {
        600
    } else if model.contains("pi 4") {
        500
    } else if model.contains("pi 3") {
        300
    } else if model.contains("pi 2") || model.contains("zero 2") {
        200
    } else {
        100
    }
}

/// Machine-ID-derived serial: `SentryUSB-<hex sha256(machine-id)>`.
/// Ensures Tesla's cached pairing survives the
/// Go→Rust transition.
fn get_machine_serial() -> String {
    let mid = fs::read_to_string("/etc/machine-id").unwrap_or_default();
    let mid = mid.trim();
    if mid.is_empty() {
        return "SentryUSB-unknown".to_string();
    }
    let h = ring::digest::digest(&ring::digest::SHA256, mid.as_bytes());
    format!("SentryUSB-{}", hex::encode(h.as_ref()))
}

/// True if a configured gadget dir looks complete enough to safely re-bind.
/// Checks that the mass_storage function exists with a readable lun.0/file
/// pointing at a real backing file. Anything weaker than this means a prior
/// enable crashed mid-setup and we should start fresh.
fn gadget_dir_is_complete(gadget: &Path) -> bool {
    let func = gadget.join("functions/mass_storage.0");
    let lun0_file = func.join("lun.0/file");
    match fs::read_to_string(&lun0_file) {
        Ok(s) => !s.trim().is_empty(),
        Err(_) => false,
    }
}

/// Enable the USB gadget.
///
/// FACADE: when the master experimental flag is on, dispatch to the native
/// per-board path ([`enable_with`] with the detected board). Otherwise run the
/// legacy hardcoded path ([`legacy_enable`]) byte-for-byte. The flag is read
/// fresh per call, so reverting it instantly restores legacy behaviour.
pub fn enable() -> Result<()> {
    if flag::experimental_enabled() {
        let board = select_board();
        info!("USB gadget: native per-board path (board: {})", board.name());
        enable_with(&*board)
    } else {
        legacy_enable()
    }
}

/// Legacy hardcoded enable — equivalent to `enable_gadget.sh`. Preserved
/// byte-for-byte as the flag-off path; do NOT alter its behaviour.
fn legacy_enable() -> Result<()> {
    let configfs = find_configfs_root()?;
    let gadget = configfs.join("usb_gadget").join(GADGET_NAME);

    // Unload legacy g_mass_storage so it doesn't hold the UDC — drop the
    // single-function legacy gadget before assembling the composite one.
    let _ = std::process::Command::new("modprobe")
        .args(["-q", "-r", "g_mass_storage"])
        .status();

    // If the gadget dir already exists AND looks complete, only a UDC
    // (re)bind is required — a prior enable may have failed to bind because
    // the UDC was busy, leaving an otherwise-valid config.
    //
    // If it exists but is INCOMPLETE (crashed mid-enable), tear it down and
    // rebuild from scratch — trying to bind a half-configured gadget produces
    // a device that enumerates but exposes no LUNs. Matches the defensive
    // stance of `enable_gadget.sh:19-23`.
    if gadget.exists() {
        if gadget_dir_is_complete(&gadget) {
            // On kernel 6.18+, a UDC unbind closes the LUN backing file.
            // Rebinding without refreshing the LUN produces "(no medium)".
            // Clear and rewrite each LUN file so the kernel re-opens it.
            let func_dir = gadget.join("functions/mass_storage.0");
            for (i, (image_path, _)) in DISK_IMAGES.iter().enumerate() {
                let lun_file = func_dir.join(format!("lun.{}/file", i));
                if lun_file.exists() && Path::new(image_path).exists() {
                    let _ = fs::write(&lun_file, "\n");
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    let _ = fs::write(&lun_file, image_path);
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(3));
            return bind_udc(&gadget);
        }
        info!("USB gadget dir exists but is incomplete — tearing down and rebuilding");
        disable()?;
    }

    // Load the composite module and the mass_storage function module as
    // separate modprobe calls. Passing both on one command line causes
    // `libcomposite: unknown parameter 'usb_f_mass_storage' ignored` on
    // kernel 6.18+ — the second name is parsed as a module parameter,
    // not a separate module to load.
    let _ = std::process::Command::new("modprobe")
        .arg("libcomposite")
        .status();
    let _ = std::process::Command::new("modprobe")
        .arg("usb_f_mass_storage")
        .status();

    // Create gadget directory structure
    let cfg_dir = gadget.join(format!("configs/{}.1", CFG));
    fs::create_dir_all(&cfg_dir)
        .with_context(|| format!("failed to create {}", cfg_dir.display()))?;

    // Common USB descriptor setup
    write_file(&gadget.join("idVendor"), "0x1d6b")?;  // Linux Foundation
    write_file(&gadget.join("idProduct"), "0x0104")?;  // Composite Gadget
    write_file(&gadget.join("bcdDevice"), "0x0100")?;  // v1.0.0
    write_file(&gadget.join("bcdUSB"), "0x0200")?;     // USB 2.0

    // String descriptors
    let strings_dir = gadget.join(format!("strings/{}", LANG));
    fs::create_dir_all(&strings_dir)
        .with_context(|| format!("failed to create {}", strings_dir.display()))?;
    let cfg_strings = gadget.join(format!("configs/{}.1/strings/{}", CFG, LANG));
    fs::create_dir_all(&cfg_strings)
        .with_context(|| format!("failed to create {}", cfg_strings.display()))?;

    write_file(&strings_dir.join("serialnumber"), &get_machine_serial())?;
    write_file(&strings_dir.join("manufacturer"), "SentryUSB")?;
    write_file(&strings_dir.join("product"), "SentryUSB Composite Gadget")?;
    write_file(&cfg_strings.join("configuration"), "SentryUSB Config")?;

    // MaxPower based on Pi model
    write_file(
        &cfg_dir.join("MaxPower"),
        &get_max_power().to_string(),
    )?;

    // Mass storage function with LUNs for each disk image
    let func_dir = gadget.join("functions/mass_storage.0");
    fs::create_dir_all(&func_dir)
        .with_context(|| format!("failed to create {}", func_dir.display()))?;

    let mut lun = 0;
    for (image_path, label) in DISK_IMAGES {
        if Path::new(image_path).exists() {
            let lun_dir = func_dir.join(format!("lun.{}", lun));
            // Create every LUN dir, including lun.0 — depending on the
            // kernel's configfs version, lun.0 is NOT guaranteed to be
            // auto-created when the mass_storage function is instantiated.
            // Writing to `lun.0/file` before the dir exists silently fails.
            fs::create_dir_all(&lun_dir)
                .with_context(|| format!("failed to create lun.{} at {}", lun, lun_dir.display()))?;
            write_file(&lun_dir.join("file"), image_path)?;

            // Get file size for inquiry string
            let size = fs::metadata(image_path)
                .map(|m| format_size(m.len()))
                .unwrap_or_else(|_| "?".to_string());
            write_file(
                &lun_dir.join("inquiry_string"),
                &format!("SentryUSB {} {}", label, size),
            )?;

            lun += 1;
        }
    }

    // Link the function to the configuration. `ensure_symlink` handles the
    // dangling-symlink case where a previous teardown left the link pointing
    // at a no-longer-existent function dir — plain `Path::exists` returned
    // false for that and led to EEXIST on the recreate.
    ensure_symlink(&func_dir, &cfg_dir.join("mass_storage.0"))?;

    info!("USB gadget configured with {} LUN(s)", lun);

    // Kernel 6.18+ needs time for the configfs LUN file attribute writes
    // to propagate before the UDC bind activates the mass_storage function.
    // Without this, the function activates with "LUN: removable file:
    // (no medium)" even though the file attribute reads back correctly.
    // 3 seconds is the empirically determined minimum on rockchip64.
    std::thread::sleep(std::time::Duration::from_secs(3));

    bind_udc(&gadget)
}

/// Native per-board enable. Structurally identical to [`legacy_enable`], but
/// every board-specific constant — `MaxPower`, the module set, the LUN settle
/// duration, and the UDC selection / max-speed / soft-connect at bind time —
/// is read from `board` instead of being hardcoded. For [`board::GenericBoard`]
/// (and any board that overrides nothing) the resulting behaviour is identical
/// to the legacy path, so a flag flip is never worse than today.
fn enable_with(board: &dyn GadgetBoard) -> Result<()> {
    let configfs = find_configfs_root()?;
    let gadget = configfs.join("usb_gadget").join(GADGET_NAME);

    let _ = std::process::Command::new("modprobe")
        .args(["-q", "-r", "g_mass_storage"])
        .status();

    // Reuse-or-rebuild, mirroring legacy_enable's defensive stance. Teardown of
    // an incomplete dir routes through disable_with so the same board context
    // (module set) is used.
    if gadget.exists() {
        if gadget_dir_is_complete(&gadget) {
            let func_dir = gadget.join("functions/mass_storage.0");
            for (i, (image_path, _)) in DISK_IMAGES.iter().enumerate() {
                let lun_file = func_dir.join(format!("lun.{}/file", i));
                if lun_file.exists() && Path::new(image_path).exists() {
                    let _ = fs::write(&lun_file, "\n");
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    let _ = fs::write(&lun_file, image_path);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(board.lun_settle_ms()));
            return bind_udc_with(board, &gadget);
        }
        info!("USB gadget dir exists but is incomplete — tearing down and rebuilding");
        disable_with(board)?;
    }

    // Load the board's module set, one modprobe per module (the combined-args
    // form is misparsed as a module parameter on kernel 6.18+).
    for module in board.load_modules() {
        let _ = std::process::Command::new("modprobe").arg(module).status();
    }

    let cfg_dir = gadget.join(format!("configs/{}.1", CFG));
    fs::create_dir_all(&cfg_dir)
        .with_context(|| format!("failed to create {}", cfg_dir.display()))?;

    write_file(&gadget.join("idVendor"), "0x1d6b")?;
    write_file(&gadget.join("idProduct"), "0x0104")?;
    write_file(&gadget.join("bcdDevice"), "0x0100")?;
    write_file(&gadget.join("bcdUSB"), "0x0200")?;

    let strings_dir = gadget.join(format!("strings/{}", LANG));
    fs::create_dir_all(&strings_dir)
        .with_context(|| format!("failed to create {}", strings_dir.display()))?;
    let cfg_strings = gadget.join(format!("configs/{}.1/strings/{}", CFG, LANG));
    fs::create_dir_all(&cfg_strings)
        .with_context(|| format!("failed to create {}", cfg_strings.display()))?;

    write_file(&strings_dir.join("serialnumber"), &get_machine_serial())?;
    write_file(&strings_dir.join("manufacturer"), "SentryUSB")?;
    write_file(&strings_dir.join("product"), "SentryUSB Composite Gadget")?;
    write_file(&cfg_strings.join("configuration"), "SentryUSB Config")?;

    // Per-board MaxPower instead of the Pi-only ladder.
    write_file(&cfg_dir.join("MaxPower"), &board.max_power_ma().to_string())?;

    let func_dir = gadget.join("functions/mass_storage.0");
    fs::create_dir_all(&func_dir)
        .with_context(|| format!("failed to create {}", func_dir.display()))?;

    let mut lun = 0;
    for (image_path, label) in DISK_IMAGES {
        if Path::new(image_path).exists() {
            let lun_dir = func_dir.join(format!("lun.{}", lun));
            fs::create_dir_all(&lun_dir)
                .with_context(|| format!("failed to create lun.{} at {}", lun, lun_dir.display()))?;
            write_file(&lun_dir.join("file"), image_path)?;

            let size = fs::metadata(image_path)
                .map(|m| format_size(m.len()))
                .unwrap_or_else(|_| "?".to_string());
            write_file(
                &lun_dir.join("inquiry_string"),
                &format!("SentryUSB {} {}", label, size),
            )?;

            lun += 1;
        }
    }

    ensure_symlink(&func_dir, &cfg_dir.join("mass_storage.0"))?;

    info!(
        "USB gadget configured with {} LUN(s) for board {}",
        lun,
        board.name()
    );

    std::thread::sleep(std::time::Duration::from_millis(board.lun_settle_ms()));

    bind_udc_with(board, &gadget)
}

/// Board-aware UDC bind. Picks the UDC via the board's `select_udc` policy
/// (sole-entry by default; peripheral-role on multi-controller dwc3 boards),
/// optionally writes the board's `max_speed` cap before binding, and issues a
/// `soft_connect` pull-up afterward when the board requires it. The bind /
/// retry / readback loop is identical to [`bind_udc`].
fn bind_udc_with(board: &dyn GadgetBoard, gadget: &Path) -> Result<()> {
    let udc_names = list_udc_names_for_bind()?;
    let udc = board.select_udc(&udc_names).ok_or_else(|| {
        anyhow::anyhow!(
            "board {} could not select a UDC among {:?}",
            board.name(),
            udc_names
        )
    })?;

    // dwc3 HS cap: write the controller's maximum_speed before binding. The
    // attribute lives under the UDC's device node; best-effort because not all
    // controllers expose it and the kernel default is acceptable when absent.
    if let Some(speed) = board.max_speed() {
        let max_speed_path = Path::new("/sys/class/udc").join(&udc).join("device/maximum_speed");
        match fs::write(&max_speed_path, speed) {
            Ok(()) => info!("UDC {} max_speed capped to {}", udc, speed),
            Err(e) => info!("UDC {} max_speed cap skipped ({})", udc, e),
        }
    }

    let udc_path = gadget.join("UDC");
    let _ = fs::write(&udc_path, "");

    for attempt in 1..=5 {
        match fs::write(&udc_path, &udc) {
            Ok(()) => match fs::read_to_string(&udc_path) {
                Ok(s) if s.trim() == udc.trim() => {
                    info!("USB gadget bound to UDC: {}", udc);
                    maybe_soft_connect(board, &udc);
                    return Ok(());
                }
                Ok(other) if attempt < 5 => {
                    info!(
                        "UDC bind attempt {} wrote {:?} but sysfs reads back {:?}; retrying",
                        attempt,
                        udc,
                        other.trim()
                    );
                    let _ = fs::write(&udc_path, "");
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                Ok(other) => {
                    return Err(anyhow::anyhow!(
                        "UDC bind silently rejected: wrote {:?}, readback {:?}",
                        udc,
                        other.trim()
                    ));
                }
                Err(_) => {
                    info!("USB gadget bound to UDC: {} (readback failed)", udc);
                    maybe_soft_connect(board, &udc);
                    return Ok(());
                }
            },
            Err(e) if attempt < 5 => {
                info!("UDC bind attempt {} failed ({}), retrying", attempt, e);
                let _ = fs::write(&udc_path, "");
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            Err(e) => {
                return Err(anyhow::anyhow!("failed to bind UDC {}: {}", udc, e));
            }
        }
    }
    Ok(())
}

/// Issue an explicit `soft_connect` pull-up if the board needs one (dwc3).
/// Best-effort: the attribute is absent on dwc2 and on controllers that
/// auto-connect, where the write simply fails and is logged.
fn maybe_soft_connect(board: &dyn GadgetBoard, udc: &str) {
    if !board.needs_soft_connect() {
        return;
    }
    let path = Path::new("/sys/class/udc").join(udc).join("device/soft_connect");
    match fs::write(&path, "connect") {
        Ok(()) => info!("UDC {} soft_connect issued", udc),
        Err(e) => info!("UDC {} soft_connect skipped ({})", udc, e),
    }
}

/// Enumerate `/sys/class/udc` entry names (sorted) for board UDC selection.
/// Errors when the directory is unreadable, matching `find_udc`'s "no UDC"
/// failure contract.
fn list_udc_names_for_bind() -> Result<Vec<String>> {
    let udc_dir = Path::new("/sys/class/udc");
    let mut names: Vec<String> = match fs::read_dir(udc_dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect(),
        Err(_) => bail!("no UDC found in /sys/class/udc"),
    };
    names.sort();
    if names.is_empty() {
        bail!("no UDC found in /sys/class/udc");
    }
    Ok(names)
}

/// Bind (or rebind) the UDC for an already-configured gadget dir. If the UDC
/// is busy, blank the UDC slot, wait briefly, and retry so stale bindings
/// clear. Returns the underlying error if the final attempt fails.
fn bind_udc(gadget: &Path) -> Result<()> {
    let udc = find_udc()?;
    let udc_path = gadget.join("UDC");

    // Clear any stale binding before writing the new one.
    let _ = fs::write(&udc_path, "");

    for attempt in 1..=5 {
        match fs::write(&udc_path, &udc) {
            Ok(()) => {
                // Sysfs writes to `UDC` can return Ok even when the kernel
                // silently rejected the bind — e.g. the gadget config is
                // incomplete or the UDC refused attachment. Read back to
                // confirm the binding actually stuck; if not, treat as a
                // retryable error rather than a silent success.
                match fs::read_to_string(&udc_path) {
                    Ok(s) if s.trim() == udc.trim() => {
                        info!("USB gadget bound to UDC: {}", udc);
                        return Ok(());
                    }
                    Ok(other) if attempt < 5 => {
                        info!(
                            "UDC bind attempt {} wrote {:?} but sysfs reads back {:?}; retrying",
                            attempt, udc, other.trim()
                        );
                        let _ = fs::write(&udc_path, "");
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                    Ok(other) => {
                        return Err(anyhow::anyhow!(
                            "UDC bind silently rejected: wrote {:?}, readback {:?}",
                            udc,
                            other.trim()
                        ));
                    }
                    Err(_) => {
                        // UDC file unreadable post-write — treat as success
                        // rather than false-failing. Trust the Ok from the
                        // write call in this edge case.
                        info!("USB gadget bound to UDC: {} (readback failed)", udc);
                        return Ok(());
                    }
                }
            }
            Err(e) if attempt < 5 => {
                info!("UDC bind attempt {} failed ({}), retrying", attempt, e);
                let _ = fs::write(&udc_path, "");
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            Err(e) => {
                return Err(anyhow::anyhow!("failed to bind UDC {}: {}", udc, e));
            }
        }
    }
    Ok(())
}

/// Disable the USB gadget.
///
/// FACADE: native per-board path when the experimental flag is on, else the
/// legacy hardcoded teardown byte-for-byte. Flag read fresh per call.
pub fn disable() -> Result<()> {
    if flag::experimental_enabled() {
        disable_with(&*select_board())
    } else {
        legacy_disable()
    }
}

/// Native per-board disable. The configfs teardown cascade is hardware-agnostic
/// and identical to [`legacy_disable`]; the only board-derived input is the
/// module set unloaded at the end (so a board that loaded a different set tears
/// the same ones down). All other steps are reproduced exactly so a flag flip
/// mid-session tears down a legacy-built gadget correctly.
fn disable_with(board: &dyn GadgetBoard) -> Result<()> {
    let _ = std::process::Command::new("modprobe")
        .args(["-q", "-r", "g_mass_storage"])
        .status();

    let configfs = find_configfs_root()?;
    let gadget = configfs.join("usb_gadget").join(GADGET_NAME);

    if !gadget.exists() {
        info!("USB gadget already disabled");
        return Ok(());
    }

    let _ = fs::write(gadget.join("UDC"), "\n");

    let cfg_dir = gadget.join(format!("configs/{}.1", CFG));
    let _ = fs::remove_file(cfg_dir.join("mass_storage.0"));
    let cfg_strings = cfg_dir.join(format!("strings/{}", LANG));
    let _ = fs::remove_dir(&cfg_strings);
    let _ = fs::remove_dir(cfg_dir.join("strings/0x0409"));

    let func_dir = gadget.join("functions/mass_storage.0");
    for i in 0..=4 {
        let _ = fs::write(func_dir.join(format!("lun.{}/file", i)), "\n");
    }
    for i in 1..=4 {
        let _ = fs::remove_dir(func_dir.join(format!("lun.{}", i)));
    }
    let _ = fs::remove_dir(&func_dir);

    let _ = fs::remove_dir(&cfg_dir);
    let _ = fs::remove_dir(gadget.join(format!("strings/{}", LANG)));
    let _ = fs::remove_dir(gadget.join("strings/0x0409"));
    let _ = fs::remove_dir(&gadget);

    // Unload the board's module set plus the legacy networking functions the
    // teardown has always swept (harmless when absent). libcomposite goes last.
    let mut rm: Vec<&str> = board.load_modules();
    for extra in ["g_ether", "usb_f_ecm", "usb_f_rndis", "libcomposite"] {
        if !rm.contains(&extra) {
            rm.push(extra);
        }
    }
    // Ensure libcomposite is last so dependent function modules drop first.
    rm.retain(|m| *m != "libcomposite");
    rm.push("libcomposite");
    let mut args: Vec<&str> = vec!["-r"];
    args.extend(rm);
    let _ = std::process::Command::new("modprobe").args(&args).status();

    if gadget.exists() {
        tracing::warn!(
            "disable_with() completed but {} still present (incomplete teardown)",
            gadget.display()
        );
    }

    info!("USB gadget disabled (board: {})", board.name());
    Ok(())
}

/// Legacy hardcoded disable — equivalent to `disable_gadget.sh`. Preserved
/// byte-for-byte as the flag-off path; do NOT alter its behaviour.
fn legacy_disable() -> Result<()> {
    // Unload g_mass_storage FIRST so it releases the UDC before we try to
    // deactivate it. If we leave this for the end, the kernel may keep the
    // UDC bound, the `echo "" > UDC` below silently no-ops, and the next
    // `enable()` hangs on "UDC busy" forever.
    //
    // Go `disable_gadget.sh:5` does this as step 1 for the same reason.
    let _ = std::process::Command::new("modprobe")
        .args(["-q", "-r", "g_mass_storage"])
        .status();

    let configfs = find_configfs_root()?;
    let gadget = configfs.join("usb_gadget").join(GADGET_NAME);

    if !gadget.exists() {
        info!("USB gadget already disabled");
        return Ok(());
    }

    // Deactivate UDC. Write a newline rather than a zero-byte string —
    // some configfs UDC handlers reject empty writes outright. If the
    // gadget already wasn't bound (e.g. a prior `disable()` that ran
    // halfway through, or boot before the first enable), the kernel
    // returns ENODEV; that's harmless and we discard it.
    let _ = fs::write(gadget.join("UDC"), "\n");

    // Detach the function from the configuration. While this symlink exists
    // the kernel treats the function as "in use" — LUN `file` attributes are
    // pinned read-only with EBUSY and `rmdir functions/mass_storage.0` fails.
    // Removing the symlink first is what unblocks the rest of the cascade.
    let cfg_dir = gadget.join(format!("configs/{}.1", CFG));
    let _ = fs::remove_file(cfg_dir.join("mass_storage.0"));
    let cfg_strings = cfg_dir.join(format!("strings/{}", LANG));
    let _ = fs::remove_dir(&cfg_strings);
    // Legacy form: the pre-fix Rust binary used LANG="0x0409", which on
    // install paths that route archiveloop through Rust (install-pi.sh
    // shim, sentryusb gadget enable CLI shim) created the dir literally
    // as `0x0409`. Hot-upgrading to the current binary would otherwise
    // leave the orphan, pinning libcomposite forever. NotFound on
    // shell-script installs is silently ignored.
    let _ = fs::remove_dir(cfg_dir.join("strings/0x0409"));

    // Now that the function is detached, release each LUN's backing-file
    // handle by clearing its `file` attribute. On kernels that aggressively
    // cascade-cleanup the function once its last symlink is removed (Pi 5
    // / Linux 6.x), the LUN paths may already be gone — writes to
    // non-existent paths are silently ignored. On kernels that don't
    // cascade, this is the step that lets the LUN/function rmdirs below
    // succeed instead of hitting EBUSY.
    let func_dir = gadget.join("functions/mass_storage.0");
    for i in 0..=4 {
        let _ = fs::write(func_dir.join(format!("lun.{}/file", i)), "\n");
    }

    // Remove the non-default LUNs (lun.1 through lun.4). lun.0 is the
    // *implicit* default LUN that the mass_storage function creates as part of
    // its own configfs node — on most kernels `rmdir lun.0` returns EPERM
    // and the kernel only releases lun.0 when the parent `mass_storage.0` is
    // removed. The shell-script reference at `run/disable_gadget.sh:23-26` skips
    // lun.0 for exactly this reason.
    //
    // The rmdir on lun.0 silently fails (the
    // result was discarded), but that left lun.0 sitting under `mass_storage.0`,
    // which made the subsequent `rmdir mass_storage.0` fail, which made the
    // gadget-root rmdir fail, which left configfs pinning `libcomposite`. The
    // next `enable()` would then log "Module libcomposite is in use" from
    // `modprobe -r` and bail out without rebuilding — so the web-UI toggle
    // appeared to error out and only a reboot could unstick it.
    for i in 1..=4 {
        let _ = fs::remove_dir(func_dir.join(format!("lun.{}", i)));
    }
    let _ = fs::remove_dir(&func_dir);

    // Remove config and string dirs
    let _ = fs::remove_dir(&cfg_dir);
    let _ = fs::remove_dir(gadget.join(format!("strings/{}", LANG)));
    let _ = fs::remove_dir(gadget.join("strings/0x0409")); // legacy form — see above
    let _ = fs::remove_dir(&gadget);

    // Unload remaining function modules (mass storage is already gone).
    let _ = std::process::Command::new("modprobe")
        .args(["-r", "usb_f_mass_storage", "g_ether", "usb_f_ecm", "usb_f_rndis", "libcomposite"])
        .status();

    // Best-effort teardown — every rmdir above used `let _ =`, so residue
    // from kernel-version-specific quirks (lun.0 implicit-default behavior,
    // cascade timing) is invisible to the caller. Surface it in the journal
    // so future flakes are diagnosable without code changes.
    if gadget.exists() {
        tracing::warn!(
            "disable() completed but {} still present (incomplete teardown)",
            gadget.display()
        );
    }

    info!("USB gadget disabled");
    Ok(())
}

/// Check if the gadget is currently active and healthy — bound to a UDC
/// AND has a populated `lun.0/file` entry.
///
/// Earlier versions only checked the UDC file, which meant a gadget that
/// was bound but had lost its LUN backing file (e.g. a manual tear-down
/// that removed `lun.0/file` without unbinding the UDC) showed as
/// "active" and the idempotent `gadget_enable` API handler skipped the
/// full rebuild — leaving Tesla plugged into a device with no LUNs.
/// Requiring both signals means a partially-torn-down gadget correctly
/// reports as inactive so the next enable call reconstructs it.
pub fn is_active() -> bool {
    // FACADE: dispatches like enable/disable, but BOTH branches MUST evaluate
    // the identical two-signal contract (UDC bound + lun.0/file non-empty).
    // is_active is the gate the idempotent enable handler checks; if the native
    // and legacy answers ever diverged, a flag flip mid-session could
    // false-trigger a needless rebuild (or skip a needed one). is_active_with
    // is therefore the SAME logic as legacy_is_active — the board argument does
    // not influence the result; it exists only for symmetry with the lifecycle.
    if flag::experimental_enabled() {
        is_active_with(&*select_board())
    } else {
        legacy_is_active()
    }
}

/// Native-path liveness check. Identical two-signal contract to
/// [`legacy_is_active`]; `board` is unused on purpose (the signals are
/// hardware-agnostic) so the answer can never diverge between paths.
fn is_active_with(_board: &dyn GadgetBoard) -> bool {
    gadget_two_signal_active()
}

/// Legacy-path liveness check — byte-for-byte the original `is_active` body.
fn legacy_is_active() -> bool {
    gadget_two_signal_active()
}

/// The two-signal liveness contract shared by both paths: the gadget is active
/// iff it is bound to a UDC AND has a populated `lun.0/file`. Single source of
/// truth so legacy and native answers are provably identical.
fn gadget_two_signal_active() -> bool {
    let root = Path::new("/sys/kernel/config/usb_gadget/sentryusb");
    let udc_bound = fs::read_to_string(root.join("UDC"))
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if !udc_bound {
        return false;
    }
    gadget_dir_is_complete(root)
}

/// Find the first available UDC (USB Device Controller).
fn find_udc() -> Result<String> {
    let udc_dir = Path::new("/sys/class/udc");
    if let Ok(entries) = fs::read_dir(udc_dir) {
        for entry in entries.flatten() {
            return Ok(entry.file_name().to_string_lossy().to_string());
        }
    }
    bail!("no UDC found in /sys/class/udc")
}

/// Format a byte count as human-readable (e.g., "32G", "512M").
fn format_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{}G", bytes / 1_073_741_824)
    } else if bytes >= 1_048_576 {
        format!("{}M", bytes / 1_048_576)
    } else if bytes >= 1024 {
        format!("{}K", bytes / 1024)
    } else {
        format!("{}B", bytes)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn ensure_symlink_creates_fresh_link() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        ensure_symlink(&target, &link).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), target);
    }

    #[test]
    fn ensure_symlink_replaces_valid_link() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        ensure_symlink(&target, &link).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), target);
    }

    #[test]
    fn ensure_symlink_replaces_dangling_link() {
        // Exact regression scenario for the EEXIST bug: a previous teardown
        // left the symlink pointing at a function dir that no longer exists.
        // Plain `Path::exists()` follows the link and returns false, the old
        // code then called `symlink()` which failed with EEXIST. With
        // symlink_metadata-based detection we replace it instead.
        let dir = tempfile::tempdir().unwrap();
        let stale_target = dir.path().join("gone");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&stale_target, &link).unwrap();
        assert!(!link.exists(), "dangling link should report not-existent");
        let new_target = dir.path().join("real");
        fs::create_dir(&new_target).unwrap();
        ensure_symlink(&new_target, &link).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), new_target);
    }
}
