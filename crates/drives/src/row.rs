//! Typed mirror of one `routes` table row.
//!
//! This is the clean-DB codec: a single struct whose fields correspond
//! one-to-one with the physical
//! columns of the `routes` table, in the exact order SQLite reports them
//! from `pragma_table_info('routes')`.
//!
//! The whole point of the type is to put the column list in *one* place
//! ([`RouteRow::COLUMNS`]) so the typed export/import path can build its
//! `SELECT` / `INSERT` without re-spelling 55 column names at every call
//! site, and so a schema drift (a future `ALTER TABLE` whose author forgot
//! to update the codec) is caught by [`tests::columns_match_schema`] — a
//! non-`#[ignore]` test that runs in the normal suite. If the const and the
//! migrated schema ever diverge, the build's test gate fails loudly rather
//! than silently writing a mis-shaped row at runtime.
//!
//! Type policy:
//! * v1 `NOT NULL` columns are plain (`String` / `i64` / `f64` / `Vec<u8>`).
//! * every column added after v1 — and every v1 column that is nullable in
//!   the DDL (`start_ts`, `end_ts`, `first_lat`, `first_lon`, and the
//!   optional BLOBs) — is `Option<T>`, matching the SQLite NULL it can hold.
//! * BLOB columns stay **encoded** (`Option<Vec<u8>>`): the row is a faithful
//!   transport of bytes. Decoding into `Vec<GpsPoint>` etc. happens in
//!   [`RouteRow::to_route`], only when a `Route` is actually needed (JSON
//!   export). Nothing here force-decodes a BLOB it doesn't have to.

use anyhow::{Context, Result};
use rusqlite::Row;

use crate::blob::{
    decode_f32s, decode_gear_runs, decode_points, decode_u8s, encode_f32s, encode_gear_runs,
    encode_points, encode_u8s,
};
use crate::db::normalize_path;
use crate::types::{Route, RouteAggregates};

/// The canonical `routes` column order — the ONE place it is defined.
///
/// Order matches the physical table layout: the v1 `CREATE TABLE` columns
/// first, then each `ALTER TABLE ADD COLUMN` group in the exact sequence
/// `schema::migrate` applies them (V2 → V3 → V4 → V6 → V7 → V9 → V10).
/// SQLite appends `ADD COLUMN`s in execution order, so this list equals
/// `pragma_table_info('routes')`'s name column on any migrated DB. The
/// `columns_match_schema` test enforces that equality so the const can
/// never silently drift.
///
/// `RouteRow`'s field declaration order, `from_row`'s positional `get`s,
/// and `bind_insert`'s parameter order all follow this same sequence.
pub const COLUMNS: &[&str] = &[
    // ── v1 CREATE TABLE ──────────────────────────────────────────────
    "file",
    "date_dir",
    "point_count",
    "raw_park_count",
    "raw_frame_count",
    "start_ts",
    "end_ts",
    "distance_m",
    "first_lat",
    "first_lon",
    "points_blob",
    "gear_states_blob",
    "ap_states_blob",
    "speeds_blob",
    "accel_blob",
    "gear_runs_blob",
    "updated_at",
    // ── v2 aggregate columns ─────────────────────────────────────────
    "max_speed_mps",
    "avg_speed_mps",
    "speed_sample_count",
    "valid_point_count",
    "fsd_engaged_ms",
    "autosteer_engaged_ms",
    "tacc_engaged_ms",
    "fsd_distance_m",
    "autosteer_distance_m",
    "tacc_distance_m",
    "assisted_distance_m",
    "fsd_disengagements",
    "fsd_accel_pushes",
    "start_lat",
    "start_lon",
    "end_lat",
    "end_lon",
    // ── v3 cloud-uploader bookkeeping ────────────────────────────────
    "cloud_uploaded_at",
    "cloud_route_id",
    // ── v4 Tessie provenance ─────────────────────────────────────────
    "source",
    "external_signature",
    "tessie_autopilot_percent",
    // ── v6 telemetry rollup ──────────────────────────────────────────
    "battery_pct_start",
    "battery_pct_end",
    "battery_temp_avg",
    "interior_temp_min",
    "interior_temp_max",
    "exterior_temp_avg",
    "hvac_runtime_s",
    // ── v7 TPMS rollup ───────────────────────────────────────────────
    "tire_fl_psi",
    "tire_fr_psi",
    "tire_rl_psi",
    "tire_rr_psi",
    // ── v9 odometer + software version ───────────────────────────────
    "odometer_mi_start",
    "odometer_mi_end",
    "software_version",
    // ── v10 location-name rollups ────────────────────────────────────
    "location_name_start",
    "location_name_end",
];

/// A single `routes` row, one field per physical column, in [`COLUMNS`]
/// order. BLOBs are kept encoded (`Option<Vec<u8>>`); call [`to_route`]
/// to decode into a [`Route`].
///
/// [`to_route`]: RouteRow::to_route
#[derive(Debug, Clone, PartialEq)]
pub struct RouteRow {
    // v1
    pub file: String,
    pub date_dir: String,
    pub point_count: i64,
    pub raw_park_count: i64,
    pub raw_frame_count: i64,
    pub start_ts: Option<i64>,
    pub end_ts: Option<i64>,
    pub distance_m: f64,
    pub first_lat: Option<f64>,
    pub first_lon: Option<f64>,
    pub points_blob: Option<Vec<u8>>,
    pub gear_states_blob: Option<Vec<u8>>,
    pub ap_states_blob: Option<Vec<u8>>,
    pub speeds_blob: Option<Vec<u8>>,
    pub accel_blob: Option<Vec<u8>>,
    pub gear_runs_blob: Option<Vec<u8>>,
    pub updated_at: i64,
    // v2
    pub max_speed_mps: Option<f64>,
    pub avg_speed_mps: Option<f64>,
    pub speed_sample_count: Option<i64>,
    pub valid_point_count: Option<i64>,
    pub fsd_engaged_ms: Option<i64>,
    pub autosteer_engaged_ms: Option<i64>,
    pub tacc_engaged_ms: Option<i64>,
    pub fsd_distance_m: Option<f64>,
    pub autosteer_distance_m: Option<f64>,
    pub tacc_distance_m: Option<f64>,
    pub assisted_distance_m: Option<f64>,
    pub fsd_disengagements: Option<i64>,
    pub fsd_accel_pushes: Option<i64>,
    pub start_lat: Option<f64>,
    pub start_lon: Option<f64>,
    pub end_lat: Option<f64>,
    pub end_lon: Option<f64>,
    // v3
    pub cloud_uploaded_at: Option<i64>,
    pub cloud_route_id: Option<String>,
    // v4
    pub source: Option<String>,
    pub external_signature: Option<String>,
    pub tessie_autopilot_percent: Option<f64>,
    // v6
    pub battery_pct_start: Option<f64>,
    pub battery_pct_end: Option<f64>,
    pub battery_temp_avg: Option<f64>,
    pub interior_temp_min: Option<f64>,
    pub interior_temp_max: Option<f64>,
    pub exterior_temp_avg: Option<f64>,
    pub hvac_runtime_s: Option<i64>,
    // v7
    pub tire_fl_psi: Option<f64>,
    pub tire_fr_psi: Option<f64>,
    pub tire_rl_psi: Option<f64>,
    pub tire_rr_psi: Option<f64>,
    // v9
    pub odometer_mi_start: Option<f64>,
    pub odometer_mi_end: Option<f64>,
    pub software_version: Option<String>,
    // v10
    pub location_name_start: Option<String>,
    pub location_name_end: Option<String>,
}

impl RouteRow {
    /// Decode one row, reading columns **positionally** in [`COLUMNS`]
    /// order. The `SELECT` that produced `row` MUST list columns in that
    /// same order — [`select_sql`] builds exactly such a statement, and the
    /// schema test pins the order to the table, so positional reads are safe.
    ///
    /// [`select_sql`]: RouteRow::select_sql
    pub fn from_row(row: &Row<'_>) -> rusqlite::Result<RouteRow> {
        Ok(RouteRow {
            file: row.get(0)?,
            date_dir: row.get(1)?,
            point_count: row.get(2)?,
            raw_park_count: row.get(3)?,
            raw_frame_count: row.get(4)?,
            start_ts: row.get(5)?,
            end_ts: row.get(6)?,
            distance_m: row.get(7)?,
            first_lat: row.get(8)?,
            first_lon: row.get(9)?,
            points_blob: row.get(10)?,
            gear_states_blob: row.get(11)?,
            ap_states_blob: row.get(12)?,
            speeds_blob: row.get(13)?,
            accel_blob: row.get(14)?,
            gear_runs_blob: row.get(15)?,
            updated_at: row.get(16)?,
            max_speed_mps: row.get(17)?,
            avg_speed_mps: row.get(18)?,
            speed_sample_count: row.get(19)?,
            valid_point_count: row.get(20)?,
            fsd_engaged_ms: row.get(21)?,
            autosteer_engaged_ms: row.get(22)?,
            tacc_engaged_ms: row.get(23)?,
            fsd_distance_m: row.get(24)?,
            autosteer_distance_m: row.get(25)?,
            tacc_distance_m: row.get(26)?,
            assisted_distance_m: row.get(27)?,
            fsd_disengagements: row.get(28)?,
            fsd_accel_pushes: row.get(29)?,
            start_lat: row.get(30)?,
            start_lon: row.get(31)?,
            end_lat: row.get(32)?,
            end_lon: row.get(33)?,
            cloud_uploaded_at: row.get(34)?,
            cloud_route_id: row.get(35)?,
            source: row.get(36)?,
            external_signature: row.get(37)?,
            tessie_autopilot_percent: row.get(38)?,
            battery_pct_start: row.get(39)?,
            battery_pct_end: row.get(40)?,
            battery_temp_avg: row.get(41)?,
            interior_temp_min: row.get(42)?,
            interior_temp_max: row.get(43)?,
            exterior_temp_avg: row.get(44)?,
            hvac_runtime_s: row.get(45)?,
            tire_fl_psi: row.get(46)?,
            tire_fr_psi: row.get(47)?,
            tire_rl_psi: row.get(48)?,
            tire_rr_psi: row.get(49)?,
            odometer_mi_start: row.get(50)?,
            odometer_mi_end: row.get(51)?,
            software_version: row.get(52)?,
            location_name_start: row.get(53)?,
            location_name_end: row.get(54)?,
        })
    }

    /// `SELECT <COLUMNS> FROM routes ORDER BY file`. Centralises the
    /// column list so `from_row`'s positional indices always line up.
    pub fn select_sql() -> String {
        format!("SELECT {} FROM routes ORDER BY file", COLUMNS.join(", "))
    }

    /// Bind every field as a positional parameter (`?1..?N`, [`COLUMNS`]
    /// order) into an `INSERT OR REPLACE` and execute it. `INSERT OR
    /// REPLACE` keyed on the `file` primary key gives the same
    /// last-writer-wins semantics the JSON importer relies on.
    pub fn bind_insert(&self, conn: &rusqlite::Connection) -> rusqlite::Result<usize> {
        let placeholders = (1..=COLUMNS.len())
            .map(|i| format!("?{}", i))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "INSERT OR REPLACE INTO routes ({}) VALUES ({})",
            COLUMNS.join(", "),
            placeholders
        );
        conn.execute(
            &sql,
            rusqlite::params![
                self.file,
                self.date_dir,
                self.point_count,
                self.raw_park_count,
                self.raw_frame_count,
                self.start_ts,
                self.end_ts,
                self.distance_m,
                self.first_lat,
                self.first_lon,
                self.points_blob,
                self.gear_states_blob,
                self.ap_states_blob,
                self.speeds_blob,
                self.accel_blob,
                self.gear_runs_blob,
                self.updated_at,
                self.max_speed_mps,
                self.avg_speed_mps,
                self.speed_sample_count,
                self.valid_point_count,
                self.fsd_engaged_ms,
                self.autosteer_engaged_ms,
                self.tacc_engaged_ms,
                self.fsd_distance_m,
                self.autosteer_distance_m,
                self.tacc_distance_m,
                self.assisted_distance_m,
                self.fsd_disengagements,
                self.fsd_accel_pushes,
                self.start_lat,
                self.start_lon,
                self.end_lat,
                self.end_lon,
                self.cloud_uploaded_at,
                self.cloud_route_id,
                self.source,
                self.external_signature,
                self.tessie_autopilot_percent,
                self.battery_pct_start,
                self.battery_pct_end,
                self.battery_temp_avg,
                self.interior_temp_min,
                self.interior_temp_max,
                self.exterior_temp_avg,
                self.hvac_runtime_s,
                self.tire_fl_psi,
                self.tire_fr_psi,
                self.tire_rl_psi,
                self.tire_rr_psi,
                self.odometer_mi_start,
                self.odometer_mi_end,
                self.software_version,
                self.location_name_start,
                self.location_name_end,
            ],
        )
    }

    /// Lossless decode into a [`Route`] for JSON export. Decodes the BLOB
    /// columns; carries the wire-relevant scalar fields straight across.
    ///
    /// The set of fields copied here is exactly the set that
    /// [`Route`] serializes — the aggregate / cloud-bookkeeping columns
    /// (`distance_m`, `cloud_*`, `fsd_*`, …) are derived data that the JSON
    /// shape never carried, so they are intentionally not surfaced on the
    /// `Route`. This keeps the typed export byte-identical to the legacy
    /// `json_compat::export_json`, which read the same subset of columns.
    pub fn to_route(&self) -> Result<Route> {
        let points = decode_points(self.points_blob.as_deref())
            .with_context(|| format!("decode points {}", self.file))?
            .unwrap_or_default();
        let gear_states = decode_u8s(self.gear_states_blob.as_deref()).unwrap_or_default();
        let autopilot_states = decode_u8s(self.ap_states_blob.as_deref()).unwrap_or_default();
        let speeds = decode_f32s(self.speeds_blob.as_deref())
            .with_context(|| format!("decode speeds {}", self.file))?
            .unwrap_or_default();
        let accel_positions = decode_f32s(self.accel_blob.as_deref())
            .with_context(|| format!("decode accel {}", self.file))?
            .unwrap_or_default();
        let gear_runs = decode_gear_runs(self.gear_runs_blob.as_deref())
            .with_context(|| format!("decode gear_runs {}", self.file))?
            .unwrap_or_default();

        Ok(Route {
            file: self.file.clone(),
            date: self.date_dir.clone(),
            points,
            gear_states,
            autopilot_states,
            speeds,
            accel_positions,
            raw_park_count: self.raw_park_count as u32,
            raw_frame_count: self.raw_frame_count as u32,
            gear_runs,
            source: self.source.clone(),
            external_signature: self.external_signature.clone(),
            tessie_autopilot_percent: self.tessie_autopilot_percent,
            battery_pct_start: self.battery_pct_start,
            battery_pct_end: self.battery_pct_end,
            interior_temp_min: self.interior_temp_min,
            interior_temp_max: self.interior_temp_max,
            exterior_temp_avg: self.exterior_temp_avg,
            hvac_runtime_s: self.hvac_runtime_s,
            tire_fl_psi: self.tire_fl_psi,
            tire_fr_psi: self.tire_fr_psi,
            tire_rl_psi: self.tire_rl_psi,
            tire_rr_psi: self.tire_rr_psi,
            odometer_mi_start: self.odometer_mi_start,
            odometer_mi_end: self.odometer_mi_end,
            location_name_start: self.location_name_start.clone(),
            location_name_end: self.location_name_end.clone(),
        })
    }

    /// Build a row from a [`Route`] plus its precomputed [`RouteAggregates`].
    /// The `file` is normalized, BLOBs are re-encoded, aggregates land in
    /// their columns, `start_ts`/`end_ts` stay NULL, and cloud bookkeeping
    /// starts NULL.
    ///
    /// NOTE: this is a *superset* of the legacy `insert_imported_route`,
    /// not a mirror — it additionally persists the v6+ telemetry columns
    /// (battery / temps / TPMS / odometer / location) straight from the
    /// wire `Route`, which the legacy importer leaves NULL. Harmless today
    /// (typed import still delegates to the legacy path), but wiring this
    /// into the import path is a behavioral change that needs its own
    /// byte-equality gate before it ships — it is NOT a drop-in swap.
    pub fn from_route(r: &Route, a: &RouteAggregates) -> RouteRow {
        RouteRow {
            file: normalize_path(&r.file),
            date_dir: r.date.clone(),
            point_count: r.points.len() as i64,
            raw_park_count: r.raw_park_count as i64,
            raw_frame_count: r.raw_frame_count as i64,
            start_ts: None,
            end_ts: None,
            distance_m: a.distance_m,
            first_lat: r.points.first().map(|p| p[0]),
            first_lon: r.points.first().map(|p| p[1]),
            points_blob: encode_points(Some(&r.points)),
            gear_states_blob: encode_u8s(Some(&r.gear_states)),
            ap_states_blob: encode_u8s(Some(&r.autopilot_states)),
            speeds_blob: encode_f32s(Some(&r.speeds)),
            accel_blob: encode_f32s(Some(&r.accel_positions)),
            gear_runs_blob: encode_gear_runs(Some(&r.gear_runs)),
            updated_at: now_unix(),
            max_speed_mps: Some(a.max_speed_mps),
            avg_speed_mps: Some(a.avg_speed_mps),
            speed_sample_count: Some(a.speed_sample_count),
            valid_point_count: Some(a.valid_point_count),
            fsd_engaged_ms: Some(a.fsd_engaged_ms),
            autosteer_engaged_ms: Some(a.autosteer_engaged_ms),
            tacc_engaged_ms: Some(a.tacc_engaged_ms),
            fsd_distance_m: Some(a.fsd_distance_m),
            autosteer_distance_m: Some(a.autosteer_distance_m),
            tacc_distance_m: Some(a.tacc_distance_m),
            assisted_distance_m: Some(a.assisted_distance_m),
            fsd_disengagements: Some(a.fsd_disengagements as i64),
            fsd_accel_pushes: Some(a.fsd_accel_pushes as i64),
            start_lat: a.start_lat,
            start_lon: a.start_lng,
            end_lat: a.end_lat,
            end_lon: a.end_lng,
            cloud_uploaded_at: None,
            cloud_route_id: None,
            source: r.source.clone(),
            external_signature: r.external_signature.clone(),
            tessie_autopilot_percent: r.tessie_autopilot_percent,
            battery_pct_start: r.battery_pct_start,
            battery_pct_end: r.battery_pct_end,
            battery_temp_avg: None,
            interior_temp_min: r.interior_temp_min,
            interior_temp_max: r.interior_temp_max,
            exterior_temp_avg: r.exterior_temp_avg,
            hvac_runtime_s: r.hvac_runtime_s,
            tire_fl_psi: r.tire_fl_psi,
            tire_fr_psi: r.tire_fr_psi,
            tire_rl_psi: r.tire_rl_psi,
            tire_rr_psi: r.tire_rr_psi,
            odometer_mi_start: r.odometer_mi_start,
            odometer_mi_end: r.odometer_mi_end,
            software_version: None,
            location_name_start: r.location_name_start.clone(),
            location_name_end: r.location_name_end.clone(),
        }
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn migrated() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&conn).unwrap();
        conn
    }

    /// HARD GATE (intentionally NOT `#[ignore]`): the canonical
    /// [`COLUMNS`] const must equal the `routes` table's column names, in
    /// order, on a freshly migrated DB. If a future migration adds, drops,
    /// or reorders a column without updating `COLUMNS` (and the matching
    /// `RouteRow` field + `from_row`/`bind_insert` position), this fails in
    /// the normal `cargo test` run instead of corrupting a row at runtime.
    #[test]
    fn columns_match_schema() {
        let conn = migrated();
        let mut stmt = conn
            .prepare("SELECT name FROM pragma_table_info('routes')")
            .unwrap();
        let live: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            live,
            COLUMNS,
            "RouteRow::COLUMNS drifted from the routes schema — update row.rs \
             (COLUMNS, the RouteRow fields, from_row, and bind_insert) to match",
        );
    }

    /// `from_row(select_sql)` then `bind_insert` then re-`from_row` must
    /// reproduce the same `RouteRow` — proves the positional read/write
    /// stays aligned with `select_sql`/`COLUMNS`.
    #[test]
    fn row_insert_select_roundtrips() {
        let conn = migrated();
        let pts = vec![[37.5, -122.1], [37.6, -122.2]];
        let route = Route {
            file: "2025-01-15_10-00-00/clip-front.mp4".into(),
            date: "2025-01-15".into(),
            points: pts.clone(),
            gear_states: vec![4, 4],
            autopilot_states: vec![1, 0],
            speeds: vec![20.0, 21.0],
            accel_positions: vec![0.1, 0.2],
            raw_park_count: 0,
            raw_frame_count: 2,
            gear_runs: vec![crate::types::GearRun { gear: 4, frames: 2 }],
            source: Some("sei".into()),
            external_signature: None,
            tessie_autopilot_percent: None,
            battery_pct_start: Some(80.0),
            battery_pct_end: None, // exercises a NULL aggregate column
            ..Default::default()
        };
        let agg = crate::aggregate::compute_route_aggregates(&route);
        let row = RouteRow::from_route(&route, &agg);
        row.bind_insert(&conn).unwrap();

        let mut stmt = conn.prepare(&RouteRow::select_sql()).unwrap();
        let got: Vec<RouteRow> = stmt
            .query_map([], RouteRow::from_row)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], row);

        // And the decoded Route preserves the BLOB payloads.
        let back = got[0].to_route().unwrap();
        assert_eq!(back.points, pts);
        assert_eq!(back.gear_states, vec![4, 4]);
        assert_eq!(back.speeds, vec![20.0, 21.0]);
        assert_eq!(back.battery_pct_start, Some(80.0));
        assert_eq!(back.battery_pct_end, None);
    }
}
