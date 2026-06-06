//! Experimental feature gate for the telemetry sampler.
//!
//! Thin delegation to the canonical reader in `sentryusb_config` so
//! every consumer in the workspace agrees on what "on" means and reads
//! the same on-disk key (`SENTRYUSB_EXPERIMENTAL`). There is no local
//! caching: each call re-parses the config, so toggling the flag takes
//! effect on the next tick with no daemon restart, and reverting it
//! restores byte-for-byte legacy behavior immediately.
//!
//! The radio-actor path (see `radio.rs`) is selected by this gate at
//! daemon startup. Flag OFF means the existing `tokio::select!` loop in
//! `main()` runs exactly as it does today — the actor is never
//! constructed and never sits in the sampling path.

/// Whether the master experimental opt-in is active. Delegates to the
/// canonical config reader; see its docs for the accepted truthy
/// values and the fail-closed (returns `false`) behavior on a missing
/// or unreadable config.
pub fn experimental_enabled() -> bool {
    sentryusb_config::experimental_enabled()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gate is a pure delegation — calling it must never panic and
    /// must agree with the canonical reader. On a dev/CI box with no
    /// `SENTRYUSB_EXPERIMENTAL` in the resolved config, both answer the
    /// same value (almost always `false`); the point is the delegation
    /// is wired, not the specific boolean.
    #[test]
    fn delegates_to_canonical_reader() {
        assert_eq!(experimental_enabled(), sentryusb_config::experimental_enabled());
    }
}
