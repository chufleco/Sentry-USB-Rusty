//! Reads the bits of `sentryusb.conf` the telemetry sampler cares
//! about: the master BLE toggle, the Tesla VIN, and the BLE adapter
//! ID (hci0 onboard vs hci1+ external dongle). Re-evaluated on every
//! main-loop iteration so the daemon picks up settings changes
//! without a restart.

use anyhow::Result;

/// Default BLE adapter when `BLE_ADAPTER` is unset in the config.
/// `hci0` is always the Pi's onboard radio. External USB BLE dongles
/// enumerate as `hci1`, `hci2`, etc.
pub const DEFAULT_ADAPTER: &str = "hci0";

/// Snapshot of the BLE-relevant config values.
#[derive(Debug, Clone)]
pub struct BleConfig {
    pub enabled: bool,
    pub vin: String,
    /// hci device ID (`hci0`, `hci1`, ...). Passed to `tesla-control`
    /// via `-bt-adapter` so the sampler talks to the chosen radio.
    /// When an external dongle is plugged in and the user opts to
    /// use it, this gets set to `hci1` and the onboard radio is
    /// left alone.
    pub adapter: String,
}

impl Default for BleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            vin: String::new(),
            adapter: DEFAULT_ADAPTER.to_string(),
        }
    }
}

impl BleConfig {
    /// Read the current config. Defaults to a permissive "enabled+VIN
    /// set" interpretation matching the api crate's `is_ble_enabled`
    /// resolution order so behavior is consistent across surfaces.
    pub fn load() -> Result<Self> {
        let config_path = sentryusb_config::find_config_path();
        let (active, commented) = sentryusb_config::parse_file(config_path)?;

        // BLE_ENABLED is now telemetry-specific and strictly explicit
        // — no more implicit yes-if-VIN-set. The api crate runs
        // `migrate_legacy_ble_flag` at startup which writes an
        // explicit BLE_ENABLED for existing users so they don't lose
        // their telemetry on upgrade. See api/src/ble.rs.
        let enabled =
            match sentryusb_config::get_config_value(&active, &commented, "BLE_ENABLED") {
                Some(v) => matches!(v.as_str(), "yes" | "true" | "1"),
                None => false,
            };

        let vin = active
            .get("TESLA_BLE_VIN")
            .cloned()
            .unwrap_or_default()
            .to_uppercase();

        // BLE_ADAPTER — defaults to hci0. Three checks:
        //   1. Set in config, starts with "hci"
        //   2. The device actually exists under /sys/class/bluetooth/
        // If the user unplugs their external dongle without changing
        // settings, the configured `hci1` would fail check 2, and we
        // fall back to `hci0` automatically. The next config reload
        // (every loop iteration) picks the dongle back up if it gets
        // re-plugged, no service restart needed.
        let configured = active
            .get("BLE_ADAPTER")
            .map(|s| s.trim().to_string())
            .filter(|s| s.starts_with("hci"));
        let adapter = match configured {
            Some(want) if adapter_exists(&want) => want,
            Some(want) => {
                // Configured adapter is gone (dongle unplugged?).
                // Don't error — fall back to onboard so telemetry
                // keeps working. Logged at this layer so the
                // diagnostics panel shows the fallback.
                tracing::warn!(
                    "configured BLE_ADAPTER={} not present; falling back to {}",
                    want,
                    DEFAULT_ADAPTER
                );
                DEFAULT_ADAPTER.to_string()
            }
            None => DEFAULT_ADAPTER.to_string(),
        };

        Ok(Self { enabled, vin, adapter })
    }
}

/// Check whether `/sys/class/bluetooth/<adapter>` exists. Used by
/// BleConfig::load to validate the configured adapter is currently
/// present (vs the user having unplugged a USB dongle since they
/// last picked it in settings).
fn adapter_exists(adapter: &str) -> bool {
    std::path::Path::new(&format!("/sys/class/bluetooth/{adapter}")).exists()
}
