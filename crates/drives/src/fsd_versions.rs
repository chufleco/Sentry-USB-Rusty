//! Maps Tesla OS versions (the `car_version` string, e.g.
//! `"2026.2.9.10"`) to the FSD release that shipped with them. Used
//! by the per-drive rollup to label drives that had FSD engaged at
//! some point.
//!
//! **Adding a new mapping**: append a row to `MAPPINGS` below and
//! recompile. The lookup is O(n) — n is small enough (one entry per
//! Tesla OS bump that carried an FSD update, so a handful per year)
//! that a sorted binary-search wouldn't pay back its complexity.
//!
//! When the user is on a Tesla OS version that isn't in this map but
//! the drive had FSD engaged, the UI falls back to "?" — the caller
//! (see `grouper::build_summary_from_aggregates`) decides whether to
//! surface that fallback string or just omit the badge.

/// Hardcoded software → FSD version mappings. Keep ordered roughly
/// by release date for human readability — lookup doesn't depend on
/// order. Sourced from teslascope.com / community release trackers.
pub const MAPPINGS: &[(&str, &str)] = &[
    // ── Late 2025 holiday release train (2025.45.x) ─────────────
    ("2025.45.5",    "v14.2.2"),
    ("2025.45.7",    "v14.2.2.2"),
    ("2025.45.8",    "v14.2.2.3"),
    ("2025.45.9",    "v14.2.2.4"),
    ("2025.45.9.1",  "v14.2.2.4"),
    ("2025.45.10",   "v14.2.2.5"),
    ("2025.46.5",    "v13.2.9"),

    // ── 2026.2.9.x ──────────────────────────────────────────────
    ("2026.2.9.1",   "v14.2.2.5"),
    ("2026.2.9.2",   "v14.2.2.5"),
    ("2026.2.9.3",   "v14.2.2.5"),
    ("2026.2.9.7",   "v14.3.1"),
    ("2026.2.9.9",   "v14.3.2"),
    ("2026.2.9.10",  "v14.3.2"),

    // ── Misc point releases ─────────────────────────────────────
    ("2026.4.5",     "v14.1.4"),
    ("2026.8.3.10",  "v13.2.9"),

    // ── 2026.14.x ───────────────────────────────────────────────
    ("2026.14.1",    "v14.2.2.5"),
    ("2026.14.2",    "v14.2.2.5"),
    ("2026.14.3",    "v14.2.2.5"),
    ("2026.14.6",    "v14.2.2.5"),
    ("2026.14.6.6",  "v14.3.3"),
];

/// Look up the FSD version that shipped with a given Tesla OS
/// version string. Returns `None` when no mapping exists; the caller
/// decides whether to render "?" or omit entirely.
pub fn fsd_version_for(car_version: &str) -> Option<&'static str> {
    MAPPINGS
        .iter()
        .find(|(sw, _)| *sw == car_version)
        .map(|(_, fsd)| *fsd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_mappings_resolve() {
        assert_eq!(fsd_version_for("2026.2.9.10"), Some("v14.3.2"));
        assert_eq!(fsd_version_for("2026.14.6.6"), Some("v14.3.3"));
    }

    #[test]
    fn unknown_software_returns_none() {
        assert_eq!(fsd_version_for("9999.99.99.99"), None);
        assert_eq!(fsd_version_for(""), None);
    }
}
