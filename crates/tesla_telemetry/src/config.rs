//! Loads telemetry settings on each main-loop iteration so changes do not
//! require a daemon restart.

use anyhow::Result;

/// Default adapter when `BLE_ADAPTER` is unset.
pub const DEFAULT_ADAPTER: &str = "hci0";

/// Default home-geofence radius in meters.
pub const DEFAULT_HOME_RADIUS_M: f64 = 120.0;

/// Default post-charge release grace in minutes.
pub const DEFAULT_CHARGE_GRACE_MIN: u64 = 45;

/// Keep-accessory automation; requires explicit enablement and a home center.
#[derive(Debug, Clone, Default)]
pub struct KeepAccessoryConfig {
    pub enabled: bool,
    pub home_lat: Option<f64>,
    pub home_lon: Option<f64>,
    pub home_radius_m: f64,
    /// Opt-in: hold 12V through a home charge instead of releasing after the archive.
    pub hold_for_charge: bool,
    /// Minutes to keep 12V on after a charge completes before releasing.
    pub charge_grace_min: u64,
}

/// Snapshot of the BLE-relevant config values.
#[derive(Debug, Clone)]
pub struct BleConfig {
    pub enabled: bool,
    pub vin: String,
    /// Bluetooth device ID (`hci0`, `hci1`, ...).
    pub adapter: String,
    /// Keep-Accessory-Power automation (12V-powered Pis only).
    pub keep_accessory: KeepAccessoryConfig,
    /// Keeps GPS polling active for automatic Away Mode evaluation.
    pub away_auto_enabled: bool,
    /// Master opt-in for experimental telemetry fields.
    pub experimental: bool,
    /// Seconds between keep-awake `charge-port-close` nudges.
    pub keep_awake_interval_secs: u64,
    /// Telemetry source. When true (`TELEMETRY_SOURCE=c6_primary`), the sampler
    /// prefers the ESP32-C6 co-processor's readings and only polls BLE itself
    /// when the C6 is genuinely failing (see `c6_source`). Default OFF — a
    /// normal box polls BLE exactly as before; this only activates on a
    /// C6-equipped box that opts in for the C6-primary soak.
    pub c6_primary: bool,
    /// Bench/soak only (`C6_BACKFILL=1`): per-domain BLE backfill beside the
    /// C6. Both radios talk to the car, so never with a shared key.
    pub c6_backfill: bool,
    /// C6 supervisor REST address; car actions route here while the C6 owns
    /// the link (`C6_SUPERVISOR_API`, loopback only).
    pub c6_supervisor_api: String,
}

/// Default C6 supervisor API address (matches its `api_bind` default).
pub const DEFAULT_C6_SUPERVISOR_API: &str = "127.0.0.1:8787";

/// Default keep-awake nudge interval in seconds.
pub const DEFAULT_KEEP_AWAKE_INTERVAL_SECS: u64 = 60;

impl Default for BleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            vin: String::new(),
            adapter: DEFAULT_ADAPTER.to_string(),
            keep_accessory: KeepAccessoryConfig::default(),
            away_auto_enabled: false,
            experimental: false,
            keep_awake_interval_secs: DEFAULT_KEEP_AWAKE_INTERVAL_SECS,
            c6_primary: false,
            c6_backfill: false,
            c6_supervisor_api: DEFAULT_C6_SUPERVISOR_API.to_string(),
        }
    }
}

impl BleConfig {
    /// Reads the current configuration. BLE is disabled unless explicitly enabled.
    pub fn load() -> Result<Self> {
        let config_path = sentryusb_config::find_config_path();
        let (active, commented) = sentryusb_config::parse_file(config_path)?;

        // BLE telemetry requires explicit enablement.
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

        // Accept hci-prefixed adapters that currently exist; otherwise use hci0.
        let configured = active
            .get("BLE_ADAPTER")
            .map(|s| s.trim().to_string())
            .filter(|s| s.starts_with("hci"));
        let adapter = match configured {
            Some(want) if adapter_exists(&want) => want,
            Some(want) => {
                // Keep telemetry running when a configured dongle disappears.
                tracing::warn!(
                    "configured BLE_ADAPTER={} not present; falling back to {}",
                    want,
                    DEFAULT_ADAPTER
                );
                DEFAULT_ADAPTER.to_string()
            }
            None => DEFAULT_ADAPTER.to_string(),
        };

        // Keep-accessory remains inert without explicit enablement.
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
        let ka_hold_for_charge = active
            .get("KEEP_ACCESSORY_HOLD_FOR_CHARGE")
            .map(|v| matches!(v.trim(), "yes" | "true" | "1"))
            .unwrap_or(false);
        let charge_grace_min = active
            .get("KEEP_ACCESSORY_CHARGE_GRACE_MIN")
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|m| *m > 0)
            .unwrap_or(DEFAULT_CHARGE_GRACE_MIN);
        let keep_accessory = KeepAccessoryConfig {
            enabled: ka_enabled,
            home_lat,
            home_lon,
            home_radius_m,
            hold_for_charge: ka_hold_for_charge,
            charge_grace_min,
        };

        // Automatic Away Mode requires fresh GPS for the API watcher.
        let away_auto_enabled = active
            .get("AWAY_MODE_AUTO_ENABLED")
            .map(|v| matches!(v.trim(), "yes" | "true" | "1"))
            .unwrap_or(false);

        // Experimental telemetry is opt-in.
        let experimental = sentryusb_config::get_config_value(
            &active,
            &commented,
            "SENTRYUSB_EXPERIMENTAL",
        )
        .map(|v| matches!(v.as_str(), "yes" | "true" | "1"))
        .unwrap_or(false);

        // Clamp the nudge cadence to avoid radio spam or sleep-window overruns.
        let keep_awake_interval_secs = sentryusb_config::get_config_value(
            &active,
            &commented,
            "BLE_KEEP_AWAKE_INTERVAL_SEC",
        )
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| (15..=900).contains(s))
        .unwrap_or(DEFAULT_KEEP_AWAKE_INTERVAL_SECS);

        // Telemetry source selector. Only "c6_primary" flips the sampler to
        // prefer the C6; anything else (incl. unset) keeps the BLE-only path.
        let c6_primary = sentryusb_config::get_config_value(&active, &commented, "TELEMETRY_SOURCE")
            .map(|v| v.trim().eq_ignore_ascii_case("c6_primary"))
            .unwrap_or(false);
        let c6_backfill = sentryusb_config::get_config_value(&active, &commented, "C6_BACKFILL")
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "yes" | "true"))
            .unwrap_or(false);
        // Loopback only: this carries signed car commands.
        let c6_supervisor_api = sentryusb_config::get_config_value(&active, &commented, "C6_SUPERVISOR_API")
            .map(|v| v.trim().to_string())
            .filter(|v| v.starts_with("127.0.0.1:") || v.starts_with("localhost:"))
            .unwrap_or_else(|| DEFAULT_C6_SUPERVISOR_API.to_string());

        Ok(Self {
            enabled,
            vin,
            adapter,
            keep_accessory,
            away_auto_enabled,
            experimental,
            keep_awake_interval_secs,
            c6_primary,
            c6_backfill,
            c6_supervisor_api,
        })
    }
}

/// Checks whether the configured Bluetooth adapter currently exists.
fn adapter_exists(adapter: &str) -> bool {
    std::path::Path::new(&format!("/sys/class/bluetooth/{adapter}")).exists()
}
