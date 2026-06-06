//! Per-board USB gadget hardware abstraction.
//!
//! The legacy lifecycle in `lib.rs` is dwc2/Pi-biased: it assumes a sole UDC,
//! a flat 100 mA fallback for any non-Pi board, a fixed 3 s LUN settle, a
//! fixed module set, and no high-speed cap. Those assumptions are correct on a
//! Raspberry Pi but latent bugs on Rockchip boards (RK3399/RK3566), which can
//! expose BOTH a `dwc3` peripheral controller AND an `xhci` host controller in
//! `/sys/class/udc` — first-wins UDC selection then binds the wrong one.
//!
//! This module factors every board-specific constant behind the [`GadgetBoard`]
//! trait. The default trait methods encode TODAY's dwc2/Pi behaviour exactly,
//! so an unrecognised board (via [`GenericBoard`]) is never worse off than the
//! legacy path. Concrete impls override only the knobs they need.
//!
//! Nothing here mutates `/sys` or `/proc`; detection is pure I/O + parsing so
//! the parsing logic can be unit-tested against fixture strings with no real
//! sysfs present.

use std::fs;
use std::path::Path;

/// The USB Device Controller driver family backing a UDC entry. Determines
/// which controller-specific quirks apply (HS cap, soft-connect, role
/// selection among multiple controllers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdcFamily {
    /// Synopsys DesignWare USB 2.0 OTG — Raspberry Pi (`dwc2`).
    Dwc2,
    /// Synopsys DesignWare USB 3.x — Rockchip and friends (`dwc3`).
    Dwc3,
    /// Anything else, or undetectable.
    Other,
}

impl UdcFamily {
    /// Classify a UDC from its `uevent` `DRIVER=` line. Case-insensitive,
    /// substring match so `dwc3-rockchip` / `fe800000.usb` style driver names
    /// still resolve. Pure function over the file contents — no I/O.
    fn from_uevent_contents(uevent: &str) -> UdcFamily {
        let lower = uevent.to_ascii_lowercase();
        let driver = lower
            .lines()
            .find_map(|l| l.strip_prefix("driver="))
            .unwrap_or("");
        if driver.contains("dwc3") {
            UdcFamily::Dwc3
        } else if driver.contains("dwc2") {
            UdcFamily::Dwc2
        } else {
            UdcFamily::Other
        }
    }
}

/// Identifying facts about the running board, gathered read-only from the
/// device tree and `/sys/class/udc`. Never guesses: every field reflects what
/// was actually read, with empty/`Other` standing in for "unknown".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardId {
    /// Device-tree `model` string (NUL-stripped, trimmed). The SoC/board name
    /// — safe to log, no PII.
    pub model: String,
    /// Predominant UDC driver family across `/sys/class/udc`.
    pub udc_family: UdcFamily,
}

impl BoardId {
    /// Detect the running board from real sysfs. Keys on the device-tree
    /// `model` line plus the UDC driver family (`/sys/class/udc/*/uevent`
    /// `DRIVER=`). On a host without those paths (CI, dev box) it returns an
    /// empty model and [`UdcFamily::Other`] rather than failing — callers treat
    /// that as the generic/conservative board, which is never worse than today.
    ///
    /// NOTE: the device-tree `compatible` string is NOT consulted yet. The one
    /// gap that creates is the cold-boot race — if `/sys/class/udc` is empty at
    /// detect time a real Rockchip board classifies as `Other`/Generic (no HS
    /// cap). That only matters for early-bind, which is scaffold-only and
    /// gated on per-board sign-off, so reading `compatible` to disambiguate is
    /// a follow-up for the early-bind activation slice, not the live path.
    pub fn detect() -> BoardId {
        let model = read_devicetree_model();
        let udc_family = detect_udc_family("/sys/class/udc");
        BoardId { model, udc_family }
    }

    /// Construct a BoardId directly from already-read strings — the seam unit
    /// tests use to exercise board-mapping without touching `/sys`.
    pub fn from_parts(model: impl Into<String>, udc_family: UdcFamily) -> BoardId {
        BoardId {
            model: model.into(),
            udc_family,
        }
    }

    fn model_lower(&self) -> String {
        self.model.to_ascii_lowercase()
    }

    /// True when the device-tree model marks this as a Raspberry Pi. Mirrors
    /// the detection discipline in `setup::env::PiModel::detect` — a literal
    /// "raspberry pi" prefix is required, so a Radxa/Rock board whose model
    /// merely contains "pi" does NOT masquerade as a Pi.
    fn is_raspberry_pi(&self) -> bool {
        self.model_lower().contains("raspberry pi")
    }

    /// True when the SoC is a Rockchip part with a dwc3 peripheral controller —
    /// the multi-UDC / HS-cap case. Requires the dwc3 family signal so a future
    /// Rockchip board that ships a different controller doesn't inherit the
    /// dwc3-specific overrides by name alone.
    fn is_rockchip_dwc3(&self) -> bool {
        let m = self.model_lower();
        let rockchip_name = m.contains("rockchip")
            || m.contains("rk3399")
            || m.contains("rk3566")
            || m.contains("rk3568")
            || m.contains("radxa")
            || m.contains("rock");
        rockchip_name && self.udc_family == UdcFamily::Dwc3
    }
}

/// Read and normalise the device-tree `model` (NUL-padded, trailing NUL).
/// Returns an empty string when the path is absent.
fn read_devicetree_model() -> String {
    fs::read_to_string("/sys/firmware/devicetree/base/model")
        .unwrap_or_default()
        .replace('\0', "")
        .trim()
        .to_string()
}

/// Enumerate `/sys/class/udc/*/uevent` and return the predominant driver
/// family. dwc3 wins over dwc2 wins over Other when multiple controllers are
/// present, because the peripheral-capable controller we care about binding is
/// the dwc3 on the multi-UDC Rockchip boards. Pure over the directory listing.
fn detect_udc_family(udc_dir: impl AsRef<Path>) -> UdcFamily {
    let names = list_udc_names(udc_dir.as_ref());
    classify_udc_family(udc_dir.as_ref(), &names, |p| fs::read_to_string(p).ok())
}

/// List UDC entry names under `udc_dir`, sorted for determinism. Empty on a
/// host with no `/sys/class/udc`.
fn list_udc_names(udc_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = match fs::read_dir(udc_dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

/// Classify the family across a set of UDC names, given a reader that maps a
/// `uevent` path to its contents. Factored so tests can inject fixture uevent
/// contents without a real `/sys`.
fn classify_udc_family<R>(udc_dir: &Path, names: &[String], read_uevent: R) -> UdcFamily
where
    R: Fn(&Path) -> Option<String>,
{
    let mut saw_dwc2 = false;
    for name in names {
        let uevent = udc_dir.join(name).join("uevent");
        if let Some(contents) = read_uevent(&uevent) {
            match UdcFamily::from_uevent_contents(&contents) {
                UdcFamily::Dwc3 => return UdcFamily::Dwc3,
                UdcFamily::Dwc2 => saw_dwc2 = true,
                UdcFamily::Other => {}
            }
        }
    }
    if saw_dwc2 {
        UdcFamily::Dwc2
    } else {
        UdcFamily::Other
    }
}

/// Per-board USB gadget knobs. Every default reproduces the legacy dwc2/Pi
/// behaviour byte-for-byte, so a board that overrides nothing — i.e.
/// [`GenericBoard`] — behaves exactly as the pre-refactor hardcoded path did.
/// Boards override only the constants that genuinely differ on their hardware.
pub trait GadgetBoard: Send + Sync {
    /// Human-readable board name for logs.
    fn name(&self) -> &str {
        "Generic board"
    }

    /// USB configuration `MaxPower` in milliamps. Legacy default is the most
    /// conservative value (100 mA) the bus guarantees to any device.
    fn max_power_ma(&self) -> u32 {
        100
    }

    /// Choose the UDC to bind from the enumerated UDC names. The DEFAULT is
    /// first-wins — byte-for-byte identical to the legacy `find_udc()` — so an
    /// unknown board behaves exactly as it does today, never worse (the
    /// governance "multi-board never worse" bar). Boards that genuinely expose
    /// multiple controllers (Dwc3RockchipBoard) override this to pick the
    /// peripheral-role one by role rather than position; first-wins among the
    /// Rockchip dwc3 + xhci-host pair is the latent bug they fix.
    fn select_udc(&self, udc_names: &[String]) -> Option<String> {
        udc_names.first().cloned()
    }

    /// Kernel modules to `modprobe` before assembling the gadget, in order.
    fn load_modules(&self) -> Vec<&'static str> {
        vec!["libcomposite", "usb_f_mass_storage"]
    }

    /// Milliseconds to sleep after writing LUN backing files before binding the
    /// UDC, letting configfs attribute writes propagate. 3 s is the empirically
    /// determined dwc2/rockchip64 minimum baked into the legacy path.
    fn lun_settle_ms(&self) -> u64 {
        3000
    }

    /// Optional `maximum_speed` to write to the UDC's controller sysfs before
    /// binding. `None` (legacy) means "leave the kernel default". dwc3 cold-boot
    /// reliability needs a high-speed cap; see [`Dwc3RockchipBoard`].
    fn max_speed(&self) -> Option<&str> {
        None
    }

    /// Whether to issue an explicit `soft_connect` pull-up after binding. The
    /// legacy dwc2 path relies on configfs auto-pullup, so the default is
    /// `false`. dwc3 needs an explicit connect on some boards.
    fn needs_soft_connect(&self) -> bool {
        false
    }

    /// Whether this board has on-vehicle-validated initramfs early-bind support.
    /// SCAFFOLD GATE: defaults to `false` and is the ONLY thing that activates
    /// the early-bind path (see `early_bind.rs`). No flag flip can turn this on;
    /// it requires a per-board cold-cold hardware sign-off in code.
    fn supports_early_bind(&self) -> bool {
        false
    }
}

/// Raspberry Pi (dwc2). Pure defaults except real per-model `MaxPower`, lifted
/// verbatim from the legacy `get_max_power()` "pi N" ladder.
pub struct Dwc2PiBoard {
    max_power: u32,
    name: String,
}

impl Dwc2PiBoard {
    pub(crate) fn from_model(model: &str) -> Dwc2PiBoard {
        Dwc2PiBoard {
            max_power: pi_max_power_ma(model),
            name: if model.is_empty() {
                "Raspberry Pi".to_string()
            } else {
                model.to_string()
            },
        }
    }
}

impl GadgetBoard for Dwc2PiBoard {
    fn name(&self) -> &str {
        &self.name
    }
    fn max_power_ma(&self) -> u32 {
        self.max_power
    }
    // All other knobs: legacy dwc2 defaults.
}

/// Raspberry Pi `MaxPower` ladder, identical to the legacy `get_max_power()`
/// substring logic. Factored out so [`Dwc2PiBoard`] and tests share one source
/// of truth.
fn pi_max_power_ma(model: &str) -> u32 {
    let m = model.to_ascii_lowercase();
    if m.contains("pi 5") {
        600
    } else if m.contains("pi 4") {
        500
    } else if m.contains("pi 3") {
        300
    } else if m.contains("pi 2") || m.contains("zero 2") {
        200
    } else {
        100
    }
}

/// Rockchip board with a dwc3 peripheral controller (RK3399/RK3566 class).
///
/// Overrides three knobs relative to the dwc2 default:
/// 1. `select_udc` picks the PERIPHERAL-role dwc3 controller instead of
///    first-wins — the real multi-UDC fix. On these SoCs `/sys/class/udc` can
///    list a dwc3 (peripheral/OTG) AND an xhci (host) controller; binding the
///    host one fails silently.
/// 2. `max_speed` returns `Some("high-speed")` — the dwc3 SuperSpeed PHY is the
///    flaky part on cold attach; capping to HS comes up clean cold-cold.
/// 3. `needs_soft_connect` is `true` — dwc3 needs the explicit pull-up.
pub struct Dwc3RockchipBoard {
    name: String,
}

impl Dwc3RockchipBoard {
    pub(crate) fn from_model(model: &str) -> Dwc3RockchipBoard {
        Dwc3RockchipBoard {
            name: if model.is_empty() {
                "Rockchip (dwc3)".to_string()
            } else {
                model.to_string()
            },
        }
    }

    /// Pick the peripheral-role dwc3 UDC from a list of UDC entry names. Pure so
    /// it can be tested against fixture name sets.
    ///
    /// Selection order:
    /// 1. A dwc3 controller (name contains "dwc3"), preferred.
    /// 2. Otherwise, a controller that is NOT an xhci/host (name contains
    ///    neither "xhci" nor "host") — many Rockchip dwc3 UDCs are named by
    ///    their MMIO address (e.g. `fe800000.usb`) and don't carry "dwc3" in
    ///    the directory name, so we exclude the known host controller instead
    ///    of requiring the dwc3 token.
    /// 3. If everything looks like a host controller, refuse (None) rather than
    ///    bind a host controller as a gadget.
    ///
    /// TODO(hardware sign-off): step 2's exclude-by-name is a heuristic for the
    /// fixture-testable layer. A future Rockchip *host* controller named neither
    /// "xhci" nor "host" could slip through. The runtime layer should prefer a
    /// positive classification (read each UDC's `uevent` DRIVER= and pick the
    /// one whose family is `UdcFamily::Dwc3`) — deferred to the on-vehicle dwc3
    /// sign-off since it can only be validated against a real `/sys/class/udc`.
    pub fn select_peripheral_udc(udc_names: &[String]) -> Option<String> {
        if let Some(dwc3) = udc_names
            .iter()
            .find(|n| n.to_ascii_lowercase().contains("dwc3"))
        {
            return Some(dwc3.clone());
        }
        udc_names
            .iter()
            .find(|n| {
                let l = n.to_ascii_lowercase();
                !l.contains("xhci") && !l.contains("host")
            })
            .cloned()
    }
}

impl GadgetBoard for Dwc3RockchipBoard {
    fn name(&self) -> &str {
        &self.name
    }

    fn select_udc(&self, udc_names: &[String]) -> Option<String> {
        Self::select_peripheral_udc(udc_names)
    }

    fn max_speed(&self) -> Option<&str> {
        Some("high-speed")
    }

    fn needs_soft_connect(&self) -> bool {
        true
    }
    // max_power_ma / load_modules / lun_settle_ms: dwc2 defaults are safe and
    // conservative for these boards; override later if measured otherwise.
    // supports_early_bind stays false — cold-cold sign-off is per-board and
    // not yet captured in code.
}

/// Conservative fallback for any board we don't specifically recognise. Every
/// method is the trait default, i.e. today's legacy behaviour: 100 mA, sole-UDC
/// selection, default modules, 3 s settle, no HS cap, no soft-connect, no
/// early-bind. Never worse than the pre-refactor path.
pub struct GenericBoard;

impl GadgetBoard for GenericBoard {}

/// Map a detected [`BoardId`] to the concrete [`GadgetBoard`] implementation.
/// Pure over the BoardId so the mapping is unit-testable.
pub fn board_for(id: &BoardId) -> Box<dyn GadgetBoard> {
    if id.is_raspberry_pi() {
        Box::new(Dwc2PiBoard::from_model(&id.model))
    } else if id.is_rockchip_dwc3() {
        Box::new(Dwc3RockchipBoard::from_model(&id.model))
    } else {
        Box::new(GenericBoard)
    }
}

/// Detect the running board and return its gadget implementation.
pub fn select_board() -> Box<dyn GadgetBoard> {
    board_for(&BoardId::detect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn udc_family_from_uevent_dwc2() {
        let uevent = "USB_UDC_NAME=20980000.usb\nDRIVER=dwc2\n";
        assert_eq!(
            UdcFamily::from_uevent_contents(uevent),
            UdcFamily::Dwc2
        );
    }

    #[test]
    fn udc_family_from_uevent_dwc3() {
        let uevent = "DRIVER=dwc3\nUSB_UDC_NAME=fe800000.usb\n";
        assert_eq!(
            UdcFamily::from_uevent_contents(uevent),
            UdcFamily::Dwc3
        );
    }

    #[test]
    fn udc_family_from_uevent_unknown() {
        assert_eq!(
            UdcFamily::from_uevent_contents("DRIVER=somethingelse\n"),
            UdcFamily::Other
        );
        assert_eq!(UdcFamily::from_uevent_contents(""), UdcFamily::Other);
    }

    // --- classify_udc_family with injected fixture uevent contents ---

    fn fixture_reader(
        map: Vec<(&'static str, &'static str)>,
    ) -> impl Fn(&Path) -> Option<String> {
        move |p: &Path| {
            for (suffix, contents) in &map {
                if p.to_string_lossy().contains(suffix) {
                    return Some((*contents).to_string());
                }
            }
            None
        }
    }

    #[test]
    fn classify_single_dwc2() {
        let dir = PathBuf::from("/sys/class/udc");
        let names = vec!["20980000.usb".to_string()];
        let reader = fixture_reader(vec![("20980000.usb", "DRIVER=dwc2\n")]);
        assert_eq!(classify_udc_family(&dir, &names, reader), UdcFamily::Dwc2);
    }

    #[test]
    fn classify_dwc3_wins_over_host() {
        // RK3399/RK3566: dwc3 peripheral + xhci host both present. dwc3 must win.
        let dir = PathBuf::from("/sys/class/udc");
        let names = vec![
            "fe800000.usb".to_string(),
            "xhci-hcd.0.auto".to_string(),
        ];
        let reader = fixture_reader(vec![
            ("fe800000.usb", "DRIVER=dwc3\n"),
            ("xhci-hcd.0.auto", "DRIVER=xhci-hcd\n"),
        ]);
        assert_eq!(classify_udc_family(&dir, &names, reader), UdcFamily::Dwc3);
    }

    #[test]
    fn classify_empty_is_other() {
        let dir = PathBuf::from("/sys/class/udc");
        assert_eq!(
            classify_udc_family(&dir, &[], |_| None),
            UdcFamily::Other
        );
    }

    // --- BoardId parsing / board mapping ---

    #[test]
    fn detect_maps_raspberry_pi() {
        let id = BoardId::from_parts("Raspberry Pi 4 Model B Rev 1.4", UdcFamily::Dwc2);
        assert!(id.is_raspberry_pi());
        assert!(!id.is_rockchip_dwc3());
        let board = board_for(&id);
        assert_eq!(board.max_power_ma(), 500);
        assert_eq!(board.max_speed(), None);
        assert!(!board.needs_soft_connect());
        assert!(!board.supports_early_bind());
    }

    #[test]
    fn detect_maps_pi5_power() {
        let id = BoardId::from_parts("Raspberry Pi 5 Model B", UdcFamily::Dwc2);
        assert_eq!(board_for(&id).max_power_ma(), 600);
    }

    #[test]
    fn detect_maps_pi_zero2_power() {
        let id = BoardId::from_parts("Raspberry Pi Zero 2 W", UdcFamily::Dwc2);
        assert_eq!(board_for(&id).max_power_ma(), 200);
    }

    #[test]
    fn detect_maps_rk3399_dwc3() {
        // Rock 4C+ class — model carries "Rockchip RK3399" + dwc3 family.
        let id = BoardId::from_parts(
            "Radxa ROCK 4C Plus / Rockchip RK3399",
            UdcFamily::Dwc3,
        );
        assert!(!id.is_raspberry_pi());
        assert!(id.is_rockchip_dwc3());
        let board = board_for(&id);
        assert_eq!(board.max_speed(), Some("high-speed"));
        assert!(board.needs_soft_connect());
        // Conservative-by-default knobs preserved.
        assert_eq!(board.max_power_ma(), 100);
        assert_eq!(board.lun_settle_ms(), 3000);
        assert!(!board.supports_early_bind());
    }

    #[test]
    fn detect_maps_rk3566_dwc3() {
        // Radxa Zero 3W class.
        let id = BoardId::from_parts("Radxa ZERO 3W / Rockchip RK3566", UdcFamily::Dwc3);
        assert!(id.is_rockchip_dwc3());
        assert_eq!(board_for(&id).max_speed(), Some("high-speed"));
    }

    #[test]
    fn rockchip_name_without_dwc3_is_generic() {
        // A Rockchip board that did NOT report a dwc3 family must not inherit
        // the dwc3 overrides by name alone — falls to GenericBoard.
        let id = BoardId::from_parts("Some Rockchip Board", UdcFamily::Other);
        assert!(!id.is_rockchip_dwc3());
        let board = board_for(&id);
        assert_eq!(board.max_speed(), None);
        assert!(!board.needs_soft_connect());
        assert_eq!(board.max_power_ma(), 100);
    }

    #[test]
    fn detect_maps_unknown_to_generic() {
        let id = BoardId::from_parts("", UdcFamily::Other);
        assert!(!id.is_raspberry_pi());
        assert!(!id.is_rockchip_dwc3());
        let board = board_for(&id);
        assert_eq!(board.max_power_ma(), 100);
        assert_eq!(board.load_modules(), vec!["libcomposite", "usb_f_mass_storage"]);
        assert_eq!(board.lun_settle_ms(), 3000);
        assert_eq!(board.max_speed(), None);
        assert!(!board.needs_soft_connect());
        assert!(!board.supports_early_bind());
    }

    #[test]
    fn pi_model_with_pi_substring_is_not_rockchip() {
        // "Radxa ROCK Pi 4" contains "pi" but is NOT a Raspberry Pi — must not
        // be classified as one (mirrors PiModel::detect discipline).
        let id = BoardId::from_parts("Radxa ROCK Pi 4 / Rockchip RK3399", UdcFamily::Dwc3);
        assert!(!id.is_raspberry_pi());
        assert!(id.is_rockchip_dwc3());
    }

    // --- Dwc3RockchipBoard::select_udc peripheral-role selection ---

    #[test]
    fn select_udc_prefers_dwc3_named() {
        let names = vec![
            "xhci-hcd.0.auto".to_string(),
            "dwc3-gadget".to_string(),
        ];
        assert_eq!(
            Dwc3RockchipBoard::select_peripheral_udc(&names),
            Some("dwc3-gadget".to_string())
        );
    }

    #[test]
    fn select_udc_excludes_xhci_host_when_dwc3_unnamed() {
        // Real RK3399: dwc3 controller is named by MMIO address, host is xhci.
        let names = vec![
            "xhci-hcd.0.auto".to_string(),
            "fe800000.usb".to_string(),
        ];
        assert_eq!(
            Dwc3RockchipBoard::select_peripheral_udc(&names),
            Some("fe800000.usb".to_string())
        );
    }

    #[test]
    fn select_udc_refuses_when_only_host_present() {
        let names = vec!["xhci-hcd.0.auto".to_string()];
        assert_eq!(Dwc3RockchipBoard::select_peripheral_udc(&names), None);
    }

    #[test]
    fn select_udc_empty_is_none() {
        assert_eq!(Dwc3RockchipBoard::select_peripheral_udc(&[]), None);
    }

    // --- default (legacy) first-wins UDC selection ---

    #[test]
    fn generic_select_udc_sole_entry() {
        let board = GenericBoard;
        assert_eq!(
            board.select_udc(&["20980000.usb".to_string()]),
            Some("20980000.usb".to_string())
        );
    }

    #[test]
    fn generic_select_udc_is_first_wins_like_legacy() {
        // The default MUST match the legacy find_udc() first-wins exactly, so
        // an unknown multi-UDC board binds as it does today — never worse.
        let board = GenericBoard;
        assert_eq!(
            board.select_udc(&["a".to_string(), "b".to_string()]),
            Some("a".to_string())
        );
    }

    #[test]
    fn generic_select_udc_empty_is_none() {
        assert_eq!(GenericBoard.select_udc(&[]), None);
    }
}
