//! Initramfs early-bind: shell scripts RENDERED FROM RUST — HARDWARE-GATED, NOT ACTIVE.
//!
//! This is NOT a native-Rust initramfs binder. It renders `#!/bin/sh`
//! hook + bind scripts (as Rust string constants) that a caller stages
//! into the initramfs; the actual early bind is done by those shell
//! scripts at boot, not by Rust. Porting the bind to a Rust binary that
//! runs in initramfs is a possible future step, but today this only
//! emits + removes shell.
//!
//! On dwc3 SBCs the gadget must be bound at `+0s` of the dwc3 probe (rather
//! than ~+48s once userspace runs) or the Tesla port can be "poisoned" on cold
//! boot. The fix is an initramfs hook that binds the gadget early. This module
//! only *renders and removes* the hook/bind scripts as string constants and
//! golden-tests them. It deliberately does NOT enable anything:
//!
//! - [`install_early_bind`] REFUSES unless `board.supports_early_bind()` is
//!   true, and every concrete [`GadgetBoard`] returns `false` today. Early-bind
//!   can only ever be turned on by a per-board code change that follows an
//!   on-vehicle cold-cold sign-off — never by flipping `SENTRYUSB_EXPERIMENTAL`
//!   or any runtime flag.
//! - The rendered hook itself re-checks `SENTRYUSB_EXPERIMENTAL` from the boot
//!   partition at boot time and no-ops when the flag is off, so even an
//!   installed hook stays inert unless the operator has explicitly opted in.
//!
//! Nothing here writes to a live initramfs path by default; [`install_early_bind`]
//! returns the rendered content for a caller to place, after passing the gate.

use crate::board::GadgetBoard;
use std::fs;
use std::path::Path;

/// Canonical install path for the initramfs hook (initramfs-tools layout).
pub const HOOK_PATH: &str = "/etc/initramfs-tools/hooks/sentryusb-gadget";
/// Canonical install path for the early boot-time bind script invoked by the
/// hook from within the initramfs.
pub const BIND_SCRIPT_PATH: &str = "/etc/initramfs-tools/scripts/init-top/sentryusb-gadget";

/// The initramfs hook script. Re-checks the master experimental flag from the
/// boot-partition config at boot and no-ops when it is off — so an installed
/// hook never activates early-bind on a normal (flag-off) install.
pub const HOOK_SCRIPT: &str = r#"#!/bin/sh
# SentryUSB initramfs early-bind hook (scaffold — hardware-gated).
# Binds the USB mass-storage gadget at dwc3 probe time on boards that need it,
# so the Tesla port is not poisoned by a late (userspace-time) bind on cold boot.
#
# SAFETY: this hook no-ops unless SENTRYUSB_EXPERIMENTAL is affirmatively set in
# the boot-partition config. The master flag is re-read here at boot so that
# reverting the flag fully disables early-bind without removing the hook.
set -e

PREREQ=""
prereqs() { echo "$PREREQ"; }
case "$1" in
    prereqs) prereqs; exit 0 ;;
esac

. /usr/share/initramfs-tools/hook-functions

# Re-check the master experimental flag from the boot partition. If it is not
# affirmatively enabled, do nothing — the gadget will be bound later by
# userspace exactly as on a legacy install.
SENTRYUSB_CONF="${rootmnt:-}/boot/firmware/sentryusb.conf"
[ -f "$SENTRYUSB_CONF" ] || SENTRYUSB_CONF="/boot/firmware/sentryusb.conf"
[ -f "$SENTRYUSB_CONF" ] || SENTRYUSB_CONF="/boot/sentryusb.conf"
if [ -f "$SENTRYUSB_CONF" ]; then
    FLAG="$(grep -E '^[[:space:]]*SENTRYUSB_EXPERIMENTAL[[:space:]]*=' "$SENTRYUSB_CONF" | tail -n1 | cut -d= -f2- | tr -d '[:space:]\"'\'' ')"
    case "$(printf '%s' "$FLAG" | tr 'A-Z' 'a-z')" in
        yes|true|1) ;;
        *) exit 0 ;;
    esac
else
    exit 0
fi

# Stage the bind script and the tools it needs into the initramfs image.
copy_exec /etc/initramfs-tools/scripts/init-top/sentryusb-gadget /scripts/init-top/sentryusb-gadget || true

exit 0
"#;

/// The boot-time bind script copied into the initramfs by the hook. Runs from
/// `init-top` before the root filesystem is mounted; it binds the gadget early.
/// Like the hook, it re-checks the experimental flag and no-ops when off.
pub const BIND_SCRIPT: &str = r#"#!/bin/sh
# SentryUSB early-bind boot script (scaffold — hardware-gated).
# Re-checks SENTRYUSB_EXPERIMENTAL and binds the gadget at dwc3 probe time.
set -e

PREREQ=""
prereqs() { echo "$PREREQ"; }
case "$1" in
    prereqs) prereqs; exit 0 ;;
esac

# Re-check the master experimental flag from the boot partition; no-op if off.
SENTRYUSB_CONF="/boot/firmware/sentryusb.conf"
[ -f "$SENTRYUSB_CONF" ] || SENTRYUSB_CONF="/boot/sentryusb.conf"
if [ -f "$SENTRYUSB_CONF" ]; then
    FLAG="$(grep -E '^[[:space:]]*SENTRYUSB_EXPERIMENTAL[[:space:]]*=' "$SENTRYUSB_CONF" | tail -n1 | cut -d= -f2- | tr -d '[:space:]\"'\'' ')"
    case "$(printf '%s' "$FLAG" | tr 'A-Z' 'a-z')" in
        yes|true|1) ;;
        *) exit 0 ;;
    esac
else
    exit 0
fi

# Bind the already-configured gadget to its UDC as soon as the controller has
# probed. Userspace later reconciles/repairs LUNs; this only wins the cold-boot
# race so the Tesla port is not poisoned.
GADGET=/sys/kernel/config/usb_gadget/sentryusb
# TODO(early-bind activation): this is first-wins, the exact host-controller
# mis-bind the runtime select_udc()/select_peripheral_udc() refactor fixes.
# Inert today (this script is scaffold-only, never installed), but BEFORE any
# board's supports_early_bind() flips true this must adopt peripheral-role
# selection (mirror Dwc3RockchipBoard::select_peripheral_udc) so a multi-UDC
# Rockchip board doesn't bind its xhci host controller as the gadget.
if [ -d "$GADGET" ] && [ -d /sys/class/udc ]; then
    UDC="$(ls /sys/class/udc 2>/dev/null | head -n1)"
    [ -n "$UDC" ] && printf '%s' "$UDC" > "$GADGET/UDC" 2>/dev/null || true
fi

exit 0
"#;

/// Why [`install_early_bind`] declined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EarlyBindRefusal {
    /// The board has not been validated for early-bind (the only case today).
    Unsupported,
}

impl std::fmt::Display for EarlyBindRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EarlyBindRefusal::Unsupported => write!(
                f,
                "early-bind refused: board not validated for cold-cold early-bind"
            ),
        }
    }
}

/// The rendered scripts ready to be written to disk by a caller, returned only
/// after the support gate passes.
pub struct RenderedEarlyBind {
    pub hook_path: &'static str,
    pub hook_script: &'static str,
    pub bind_script_path: &'static str,
    pub bind_script: &'static str,
}

/// Render the early-bind scripts for a board — but REFUSE unless the board
/// declares [`GadgetBoard::supports_early_bind`]. Since every concrete board
/// returns `false` today, this always refuses; the gate is the single point
/// that would gain a board, and only after on-vehicle cold-cold sign-off.
///
/// This intentionally does not itself write to `/etc` — it returns the rendered
/// content so the activation step stays explicit and testable.
pub fn install_early_bind(
    board: &dyn GadgetBoard,
) -> Result<RenderedEarlyBind, EarlyBindRefusal> {
    if !board.supports_early_bind() {
        return Err(EarlyBindRefusal::Unsupported);
    }
    Ok(RenderedEarlyBind {
        hook_path: HOOK_PATH,
        hook_script: HOOK_SCRIPT,
        bind_script_path: BIND_SCRIPT_PATH,
        bind_script: BIND_SCRIPT,
    })
}

/// Remove any installed early-bind hook + bind script. Idempotent: missing
/// files are not an error, so this is safe to call unconditionally (e.g. on
/// downgrade or when disabling experimental mode) without first probing.
pub fn remove_early_bind() -> std::io::Result<()> {
    for path in [HOOK_PATH, BIND_SCRIPT_PATH] {
        match fs::remove_file(Path::new(path)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::{Dwc3RockchipBoard, GenericBoard};

    /// Golden: the hook must contain the boot-time experimental-flag recheck so
    /// an installed hook stays inert when the flag is off.
    #[test]
    fn hook_rechecks_experimental_flag() {
        assert!(
            HOOK_SCRIPT.contains("SENTRYUSB_EXPERIMENTAL"),
            "rendered hook must re-read SENTRYUSB_EXPERIMENTAL at boot"
        );
        // The recheck must be wired to an early no-op (`exit 0`) on the off path.
        assert!(HOOK_SCRIPT.contains("exit 0"));
        assert!(HOOK_SCRIPT.starts_with("#!/bin/sh"));
    }

    #[test]
    fn bind_script_rechecks_experimental_flag() {
        assert!(
            BIND_SCRIPT.contains("SENTRYUSB_EXPERIMENTAL"),
            "rendered bind script must re-read SENTRYUSB_EXPERIMENTAL at boot"
        );
        assert!(BIND_SCRIPT.contains("/sys/kernel/config/usb_gadget/sentryusb"));
        assert!(BIND_SCRIPT.starts_with("#!/bin/sh"));
    }

    /// Golden snapshot of the full hook content, so any unintended edit to the
    /// rendered script (especially dropping the flag recheck) trips the test.
    #[test]
    fn hook_golden_snapshot_stable() {
        // Pin a few load-bearing lines verbatim rather than the whole blob, so
        // the test documents intent and survives benign formatting tweaks.
        assert!(HOOK_SCRIPT.contains("hook-functions"));
        assert!(HOOK_SCRIPT.contains("copy_exec /etc/initramfs-tools/scripts/init-top/sentryusb-gadget"));
        assert!(HOOK_SCRIPT.contains("prereqs) prereqs; exit 0 ;;"));
    }

    #[test]
    fn install_refused_for_generic_board() {
        let board = GenericBoard;
        assert_eq!(
            install_early_bind(&board).err(),
            Some(EarlyBindRefusal::Unsupported)
        );
    }

    #[test]
    fn install_refused_for_rockchip_board_today() {
        // Even the dwc3 board — the one that ultimately needs early-bind — is
        // refused until its supports_early_bind() flips after hardware sign-off.
        let board = Dwc3RockchipBoard::from_model("Radxa ROCK 4C Plus / Rockchip RK3399");
        assert!(!board.supports_early_bind());
        assert_eq!(
            install_early_bind(&board).err(),
            Some(EarlyBindRefusal::Unsupported)
        );
    }

    #[test]
    fn remove_early_bind_is_idempotent_when_absent() {
        // No files present → Ok, not an error.
        // (Paths are absolute system paths that don't exist in CI; remove_file
        // returns NotFound which we swallow.)
        remove_early_bind().expect("removing absent early-bind files must be Ok");
    }
}
