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

/// Default home geofence radius (meters) when `KEEP_ACCESSORY_HOME_RADIUS_M`
/// is unset. ~120m comfortably swallows the reverse-geocode drift that
/// makes Tesla occasionally report a neighbor's address.
pub const DEFAULT_HOME_RADIUS_M: f64 = 120.0;

/// Keep-Accessory-Power automation config (see `keep_accessory.rs`).
/// Inert unless the user declares their Pi is powered from the 12V
/// accessory outlet (`KEEP_ACCESSORY_ENABLED`) and sets a home
/// geofence center. Radius defaults to `DEFAULT_HOME_RADIUS_M`.
#[derive(Debug, Clone, Default)]
pub struct KeepAccessoryConfig {
    pub enabled: bool,
    pub home_lat: Option<f64>,
    pub home_lon: Option<f64>,
    pub home_radius_m: f64,
}

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
    /// Keep-Accessory-Power automation (12V-powered Pis only).
    pub keep_accessory: KeepAccessoryConfig,
    /// Automatic Away Mode is on (geofence-driven WiFi AP). The daemon
    /// only cares whether it's enabled — the geofence decision itself
    /// lives in the API server (`away_mode.rs`). When on, the daemon
    /// keeps polling GPS (see the location-poll gate in `main.rs`) so
    /// the API watcher has a fresh fix to evaluate home/away against.
    pub away_auto_enabled: bool,
    /// Master opt-in for in-progress consolidation features (expanded
    /// sampler decode, etc.). Default OFF — set `SENTRYUSB_EXPERIMENTAL`
    /// to enable. Anything gated by this flag stays dormant on a normal
    /// install, so a pre-release build never changes behavior unless a
    /// tester explicitly turns it on. See the consolidation RFC.
    pub experimental: bool,
    /// Emit periodic keep-awake nudges from inside the sampler on its
    /// already-held PersistentSession instead of having `awake_start`
    /// spawn a separate `ble-action` process every 300s. Default OFF
    /// — set `BLE_KEEP_AWAKE_VIA_SAMPLER=yes` to enable. When on,
    /// `awake_start`'s Case-3 BLE branch skips its `ble-action` call
    /// while the sampler socket is present and lets the sampler emit
    /// the nudge. When off (or the sampler is down), the legacy
    /// spawned-loop path stays in charge. See task #329.
    pub keep_awake_via_sampler: bool,
}

impl Default for BleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            vin: String::new(),
            adapter: DEFAULT_ADAPTER.to_string(),
            keep_accessory: KeepAccessoryConfig::default(),
            away_auto_enabled: false,
            experimental: false,
            keep_awake_via_sampler: false,
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

        // Keep-Accessory-Power automation. Inert unless explicitly
        // enabled (the user-declared "Pi powered from 12V" gate). Home
        // geofence center is lat/lon; radius defaults to ~120m.
        let ka_enabled = active
            .get("KEEP_ACCESSORY_ENABLED")
            .map(|v| matches!(v.trim(), "yes" | "true" | "1"))
            .unwrap_or(false);
        let home_lat = active
            .get("KEEP_ACCESSORY_HOME_LAT")
            .and_then(|s| s.trim().parse::<f64>().ok());
        let home_lon = active
            .get("KEEP_ACCESSORY_HOME_LON")
            .and_then(|s| s.trim().parse::<f64>().ok());
        let home_radius_m = active
            .get("KEEP_ACCESSORY_HOME_RADIUS_M")
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|r| *r > 0.0)
            .unwrap_or(DEFAULT_HOME_RADIUS_M);
        let keep_accessory = KeepAccessoryConfig {
            enabled: ka_enabled,
            home_lat,
            home_lon,
            home_radius_m,
        };

        // Automatic Away Mode. Like keep-accessory it's a write-once
        // gate the daemon reads each loop — when on, we keep GPS warm
        // for the API server's geofence watcher.
        let away_auto_enabled = active
            .get("AWAY_MODE_AUTO_ENABLED")
            .map(|v| matches!(v.trim(), "yes" | "true" | "1"))
            .unwrap_or(false);

        // Master experimental opt-in. Default OFF. Gates in-progress
        // consolidation features so a pre-release build is byte-for-byte
        // current behavior until a tester sets SENTRYUSB_EXPERIMENTAL.
        let experimental = sentryusb_config::get_config_value(
            &active,
            &commented,
            "SENTRYUSB_EXPERIMENTAL",
        )
        .map(|v| matches!(v.as_str(), "yes" | "true" | "1"))
        .unwrap_or(false);

        // Sampler-emitted keep-awake nudge opt-in. Default OFF — when
        // the flag isn't set, behavior is byte-for-byte the legacy
        // spawned `ble-action charge-port-close` loop. Read fresh on
        // every loop iteration so a tester can flip it via a conf edit
        // without bouncing the service.
        let keep_awake_via_sampler = sentryusb_config::get_config_value(
            &active,
            &commented,
            "BLE_KEEP_AWAKE_VIA_SAMPLER",
        )
        .map(|v| matches!(v.as_str(), "yes" | "true" | "1"))
        .unwrap_or(false);

        Ok(Self {
            enabled,
            vin,
            adapter,
            keep_accessory,
            away_auto_enabled,
            experimental,
            keep_awake_via_sampler,
        })
    }
}

/// Check whether `/sys/class/bluetooth/<adapter>` exists. Used by
/// BleConfig::load to validate the configured adapter is currently
/// present (vs the user having unplugged a USB dongle since they
/// last picked it in settings).
fn adapter_exists(adapter: &str) -> bool {
    std::path::Path::new(&format!("/sys/class/bluetooth/{adapter}")).exists()
}
