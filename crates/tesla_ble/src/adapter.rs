//! Radio topology abstraction for BLE adapter coordination.
//!
//! The sampler holds a single GATT central connection to the car. On a
//! typical Pi there is exactly one controller, so the car link and the
//! iOS-app peripheral (GATT server) share one radio and must serialize;
//! a phone that wants the link preempts the sampler via a bounded lease.
//! Some boards expose a second controller (an external USB dongle), on
//! which the car central and the phone work can run on separate radios
//! concurrently.
//!
//! This module never hardcodes a board or an adapter index. Topology is
//! detected from the kernel's adapter list under `/sys/class/bluetooth`
//! — the same board-agnostic existence check the telemetry config uses —
//! and the resulting [`RadioTopology`] tells the radio actor whether the
//! phone contends with the car link (single radio) or can run on a
//! second adapter (dual radio).
//!
//! ## Status
//!
//! [`SingleRadio`] is the live, fully-modeled topology. [`DualRadio`] is
//! a documented STUB: [`detect_topology`] never returns it yet (it falls
//! through to single even when two adapters exist), because true
//! concurrent second-adapter operation — BlueZ central+peripheral
//! coexistence and the preempt/resume latency budget — needs on-vehicle
//! validation before it can be trusted.

/// Default kernel path enumerating present BLE controllers. Each present
/// adapter appears as a child directory (`hci0`, `hci1`, ...). This is
/// the board-agnostic glob root the telemetry config already relies on
/// for adapter-existence checks.
pub const SYS_BLUETOOTH_ROOT: &str = "/sys/class/bluetooth";

/// How a board's BLE radio(s) relate the car central link to phone GATT
/// work. The radio actor consults this to decide whether a phone request
/// must preempt the sampler (single radio) or can proceed on its own
/// adapter (dual radio).
pub trait RadioTopology: Send + Sync {
    /// Adapter name the car central link uses (e.g. `"hci0"`).
    fn car_adapter(&self) -> &str;

    /// Adapter name phone GATT work should use. On a single-radio board
    /// this is the same controller as [`car_adapter`]; on a dual-radio
    /// board it is the second controller.
    fn phone_adapter(&self) -> &str;

    /// Whether phone work and the car link contend for one controller.
    /// `true` (single radio) means a phone request must preempt the
    /// sampler via a bounded lease; `false` (dual radio) means the two
    /// can run concurrently on separate adapters.
    fn phone_contends_with_car(&self) -> bool;
}

/// One controller shared by the car central link and phone GATT work.
/// The common Pi case. Phone work preempts the sampler via a bounded
/// lease because two centrals can't reliably share one controller.
#[derive(Debug, Clone)]
pub struct SingleRadio {
    adapter: String,
}

impl SingleRadio {
    /// Build a single-radio topology pinned to one adapter (car and
    /// phone share it).
    pub fn new(adapter: impl Into<String>) -> Self {
        Self { adapter: adapter.into() }
    }
}

impl RadioTopology for SingleRadio {
    fn car_adapter(&self) -> &str {
        &self.adapter
    }
    fn phone_adapter(&self) -> &str {
        // Same controller as the car link — hence the contention.
        &self.adapter
    }
    fn phone_contends_with_car(&self) -> bool {
        true
    }
}

/// Two controllers: the car central link on one adapter, phone GATT work
/// on the other, runnable concurrently.
///
/// STUB. The type and its `RadioTopology` impl are complete so the actor
/// can be written against the trait, but [`detect_topology`] never
/// constructs a `DualRadio` yet: it falls through to [`SingleRadio`]
/// even when a second adapter is present. Activating real concurrent
/// second-adapter operation (BlueZ central+peripheral coexistence,
/// preempt/resume latency) is slice 5 and requires on-vehicle sign-off
/// with a phone in the loop. Until then the conservative single-radio
/// serialization is always used — correct everywhere, just not maximally
/// concurrent on dual-radio boards.
#[derive(Debug, Clone)]
pub struct DualRadio {
    car: String,
    phone: String,
}

impl DualRadio {
    /// Build a dual-radio topology: `car` runs the central link, `phone`
    /// runs GATT work, on separate controllers.
    pub fn new(car: impl Into<String>, phone: impl Into<String>) -> Self {
        Self { car: car.into(), phone: phone.into() }
    }
}

impl RadioTopology for DualRadio {
    fn car_adapter(&self) -> &str {
        &self.car
    }
    fn phone_adapter(&self) -> &str {
        &self.phone
    }
    fn phone_contends_with_car(&self) -> bool {
        false
    }
}

/// Enumerate present BLE controllers under `root`, sorted by name. Each
/// present adapter is a child directory; absent/unreadable root yields
/// an empty list (caller falls back to a sensible default). Pure on the
/// filesystem so it can be table-tested over a fixture directory.
fn list_adapters(root: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = match std::fs::read_dir(root) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            // Only real controllers — guard against stray non-hci
            // entries some kernels expose under this node.
            .filter(|n| n.starts_with("hci"))
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

/// Decide the topology from a set of present adapter names and the
/// configured car adapter. Split out from the filesystem so it is
/// directly table-testable.
///
/// Rules:
///   * The car adapter is whatever the config resolved (`configured`),
///     provided it is actually present; otherwise the first present
///     adapter; otherwise the conservative default `hci0`.
///   * Always returns [`SingleRadio`] today. A second adapter is noted
///     for the future dual-radio path but NOT activated — see the
///     [`DualRadio`] stub docs.
fn topology_from_adapters(
    present: &[String],
    configured: Option<&str>,
) -> Box<dyn RadioTopology> {
    // Pick the car adapter: prefer the configured one if present, else
    // the first present adapter, else the board-agnostic default. We
    // never assume hci0 is the right one — it's only the last-resort
    // fallback when /sys told us nothing.
    let car = configured
        .filter(|c| present.iter().any(|p| p == c))
        .map(|c| c.to_string())
        .or_else(|| present.first().cloned())
        .unwrap_or_else(|| "hci0".to_string());

    // A second distinct adapter is the prerequisite for dual-radio, but
    // we deliberately do NOT construct DualRadio here: concurrent
    // second-adapter operation is unproven (slice 5). Falling through to
    // SingleRadio keeps the conservative serialize-everything behavior.
    let _second_adapter = present.iter().find(|p| **p != car);

    Box::new(SingleRadio::new(car))
}

/// Detect the board's radio topology from the kernel adapter list.
/// Board-agnostic: reads `/sys/class/bluetooth`, never hardcodes an
/// index. `configured_car_adapter` is the adapter the telemetry config
/// resolved (e.g. from `BLE_ADAPTER`); it wins if present.
///
/// Always returns a [`SingleRadio`] today (the [`DualRadio`] path is a
/// documented stub pending on-vehicle sign-off).
pub fn detect_topology(configured_car_adapter: Option<&str>) -> Box<dyn RadioTopology> {
    let present = list_adapters(std::path::Path::new(SYS_BLUETOOTH_ROOT));
    topology_from_adapters(&present, configured_car_adapter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // ── topology_from_adapters table tests over fixture adapter sets ──

    #[test]
    fn single_adapter_uses_it_as_car() {
        let t = topology_from_adapters(&names(&["hci0"]), None);
        assert_eq!(t.car_adapter(), "hci0");
        assert_eq!(t.phone_adapter(), "hci0");
        assert!(t.phone_contends_with_car(), "single radio => phone contends");
    }

    #[test]
    fn configured_car_adapter_wins_when_present() {
        // Two adapters present; config asked for the dongle (hci1).
        let t = topology_from_adapters(&names(&["hci0", "hci1"]), Some("hci1"));
        assert_eq!(t.car_adapter(), "hci1");
    }

    #[test]
    fn configured_car_adapter_ignored_when_absent() {
        // Config asked for hci1 but only hci0 is present (dongle
        // unplugged) — fall back to the first present adapter, never
        // error.
        let t = topology_from_adapters(&names(&["hci0"]), Some("hci1"));
        assert_eq!(t.car_adapter(), "hci0");
    }

    #[test]
    fn no_adapters_falls_back_to_default() {
        let t = topology_from_adapters(&[], None);
        assert_eq!(t.car_adapter(), "hci0");
        assert!(t.phone_contends_with_car());
    }

    #[test]
    fn two_adapters_still_single_radio_until_dual_is_proven() {
        // The dual-radio prerequisite (a second adapter) is met, but the
        // stub must NOT be activated: detect always serializes today.
        let t = topology_from_adapters(&names(&["hci0", "hci1"]), None);
        assert!(
            t.phone_contends_with_car(),
            "DualRadio is a stub; detect must fall through to SingleRadio",
        );
    }

    #[test]
    fn never_hardcodes_hci0_when_only_a_dongle_is_present() {
        // Board-agnostic: if the only controller the kernel reports is
        // hci1, that's the car adapter — we don't assume hci0 exists.
        let t = topology_from_adapters(&names(&["hci1"]), None);
        assert_eq!(t.car_adapter(), "hci1");
    }

    // ── list_adapters over a real fixture directory (board-agnostic glob) ──

    #[test]
    fn list_adapters_reads_present_controllers_sorted() {
        let dir = tempfile::tempdir().unwrap();
        // Simulate the kernel's child-directory-per-controller layout,
        // plus a stray non-hci node that must be filtered out.
        fs::create_dir(dir.path().join("hci1")).unwrap();
        fs::create_dir(dir.path().join("hci0")).unwrap();
        fs::create_dir(dir.path().join("rfkill")).unwrap();
        let got = list_adapters(dir.path());
        assert_eq!(got, names(&["hci0", "hci1"]), "sorted, hci-only");
    }

    #[test]
    fn list_adapters_missing_root_is_empty() {
        let got = list_adapters(std::path::Path::new(
            "/nonexistent/sys/class/bluetooth/path",
        ));
        assert!(got.is_empty());
    }

    #[test]
    fn detect_from_fixture_dir_picks_dongle_when_configured() {
        // Exercise the same logic detect_topology uses, but over a
        // fixture root so it's hermetic.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("hci0")).unwrap();
        fs::create_dir(dir.path().join("hci1")).unwrap();
        let present = list_adapters(dir.path());
        let t = topology_from_adapters(&present, Some("hci1"));
        assert_eq!(t.car_adapter(), "hci1");
    }
}
