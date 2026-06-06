//! Master experimental-flag gate for the native per-board gadget path.
//!
//! Thin wrapper over [`sentryusb_config::experimental_enabled`] so the gadget
//! crate has a single, intention-revealing call site for the
//! `SENTRYUSB_EXPERIMENTAL` opt-in and does NOT depend on the api crate (which
//! itself depends on this crate — depending back would form a cycle).
//!
//! Read fresh on every call: there is no caching here and none in the config
//! layer, so toggling the key in the on-disk config takes effect on the next
//! `enable`/`disable`/`is_active`, and reverting it instantly restores the
//! legacy byte-for-byte path with no daemon restart.

/// `true` when the master experimental opt-in is set to an affirmative value.
/// Delegates to the config crate's canonical reader so every consumer agrees
/// on what "on" means; a missing key / unreadable file answers `false`,
/// keeping a normal install on the legacy path.
pub fn experimental_enabled() -> bool {
    sentryusb_config::experimental_enabled()
}
