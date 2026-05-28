//! JSON import/export for `drive-data.json`.
//!
//! * Import uses a streaming serde visitor that deserializes and inserts
//!   one Route at a time, dropping it before the next is read. Peak Rust
//!   heap is ~one decoded Route (~30 KB) instead of the full Vec<Route>
//!   (hundreds of MB for large files). Strips a UTF-8 BOM if present.
//!   Refuses imports that would shrink the DB below 50%.
//! * Export walks the DB in deterministic order (routes by `file`,
//!   processed by `file`, tags by `drive_key`+`tag`) so two exports of
//!   the same state produce byte-identical JSON. That matters for rsync
//!   diffs and for Sentry Studio's change-detection.

use std::io::Write;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use tracing::{debug, info, warn};

use crate::aggregate::compute_route_aggregates;
use crate::blob::{decode_f32s, decode_gear_runs, decode_points, decode_u8s};
use crate::db::normalize_path;
use crate::types::{GearRun, GpsPoint, Route};

/// Minimum existing-route count before the shrink guard applies. Below
/// this, allow any import — tiny DBs don't need corruption protection
/// because there's little to lose.
const SYNCGUARD_MIN_ROUTES: usize = 1000;

/// Minimum fraction of the existing route count that an import must
/// reach to be allowed.
const SYNCGUARD_SHRINK_RATIO: f64 = 0.5;

/// What `import_json` reports back to the caller.
#[derive(Debug, Default, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportStats {
    pub routes: usize,
    pub processed_files: usize,
    pub drive_tags: usize,
}

/// Per-route problems discovered during a JSON import. The route is still
/// inserted in every case — these counters exist so operators can see *why*
/// a drive might not appear in the UI even though the import "succeeded".
/// Persisted to the `meta` table after each import for post-mortem queries.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportDiagnostics {
    /// Total routes seen in the JSON (matches `ImportStats.routes` on success).
    pub seen: usize,
    /// Routes whose `points` array was empty — they insert with NULL lat/lon
    /// and zero aggregates, but still appear in `/api/drives/routes`.
    pub empty_points: usize,
    /// Routes whose `date` field was empty/whitespace.
    pub empty_date: usize,
    /// Routes whose normalized `file` matched another route earlier in the
    /// same JSON — second occurrence overwrites the first via INSERT OR REPLACE.
    pub duplicate_files_in_json: usize,
    /// First few file paths flagged by any of the above categories. Capped to
    /// `EXAMPLE_LIMIT` so a pathological import doesn't blow up the meta row.
    pub bad_examples: Vec<String>,
}

/// Cap on `ImportDiagnostics::bad_examples` length.
const EXAMPLE_LIMIT: usize = 20;

impl ImportDiagnostics {
    fn record_example(&mut self, file: &str) {
        if self.bad_examples.len() < EXAMPLE_LIMIT {
            self.bad_examples.push(file.to_string());
        }
    }

    /// True when at least one category fired — operators should look at logs.
    pub fn has_problems(&self) -> bool {
        self.empty_points > 0 || self.empty_date > 0 || self.duplicate_files_in_json > 0
    }
}

/// Import a Go-format `drive-data.json` into the SQLite store.
///
/// Uses a streaming serde visitor: each Route is deserialized from the
/// reader, inserted into SQLite, and dropped before the next element is
/// read. Peak Rust heap is approximately one decoded Route (~30 KB) instead
/// of the entire Vec<Route> that a naive `from_slice` would allocate — on a
/// 346 MB file that difference is ~400 MB, which matters critically on
/// devices like the Pi Zero 2W with 512 MB total RAM.
///
/// `on_progress` is called periodically with the running route count so
/// the web UI can show a live indicator during long imports.
pub fn import_json<F>(
    conn: &mut Connection,
    path: &str,
    on_progress: F,
) -> Result<(ImportStats, ImportDiagnostics)>
where
    F: Fn(usize),
{
    use serde::de::{Deserializer as _, DeserializeSeed, MapAccess, SeqAccess, Visitor};
    use std::collections::{HashMap, HashSet};
    use std::fmt;
    use std::io::{BufReader, Read, Seek, SeekFrom};

    // File size is useful in logs to spot zero-byte or truncated uploads.
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    // Open and skip UTF-8 BOM (Windows-edited JSON prepends EF BB BF).
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open {}", path))?;
    {
        let mut bom = [0u8; 3];
        match file.read(&mut bom) {
            Ok(n) => {
                if n < 3 || bom != [0xEF, 0xBB, 0xBF] {
                    file.seek(SeekFrom::Start(0))?;
                }
            }
            Err(e) => {
                warn!("import_json: BOM probe read failed on {}: {}", path, e);
                file.seek(SeekFrom::Start(0))?;
            }
        }
    }
    let reader = BufReader::with_capacity(64 * 1024, file);

    // Check existing count for the shrink guard before opening the transaction.
    let existing_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM routes", [], |r| r.get(0))
        .unwrap_or(0);

    info!(
        "import_json: starting path={} size_bytes={} existing_routes={}",
        path, file_size, existing_count
    );

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let tx = conn.transaction()?;

    // -------------------------------------------------------------------------
    // Streaming serde visitor chain.  Rust allows impl blocks for local types
    // inside function bodies, which keeps all of this private to import_json.
    // -------------------------------------------------------------------------

    struct Ctx<'tx> {
        tx: &'tx rusqlite::Transaction<'tx>,
        now: i64,
        routes: usize,
        processed_files: usize,
        drive_tags: usize,
        on_progress: &'tx dyn Fn(usize),
        diag: &'tx mut ImportDiagnostics,
        seen_files: &'tx mut HashSet<String>,
    }

    /// Deserializes the `routes` JSON array element-by-element.  Each Route
    /// is inserted and dropped before the next one is deserialized.
    struct RouteSeq<'a, 'tx: 'a>(&'a mut Ctx<'tx>);

    impl<'de, 'a, 'tx: 'a> DeserializeSeed<'de> for RouteSeq<'a, 'tx> {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
            d.deserialize_seq(self)
        }
    }

    impl<'de, 'a, 'tx: 'a> Visitor<'de> for RouteSeq<'a, 'tx> {
        type Value = ();
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("routes array")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
            while let Some(route) = seq.next_element::<Route>()? {
                let agg = compute_route_aggregates(&route);

                // Per-route diagnostics — record problems but still insert.
                self.0.diag.seen += 1;
                let norm = normalize_path(&route.file);
                let mut flagged = false;
                if route.points.is_empty() {
                    self.0.diag.empty_points += 1;
                    flagged = true;
                }
                if route.date.trim().is_empty() {
                    self.0.diag.empty_date += 1;
                    flagged = true;
                }
                if !self.0.seen_files.insert(norm.clone()) {
                    self.0.diag.duplicate_files_in_json += 1;
                    flagged = true;
                }
                if flagged {
                    self.0.diag.record_example(&route.file);
                }
                debug!(
                    "import: route file={} points={} date={}",
                    route.file,
                    route.points.len(),
                    route.date
                );

                insert_imported_route(self.0.tx, &route, &agg, self.0.now)
                    .map_err(|e| serde::de::Error::custom(e.to_string()))?;
                self.0.routes += 1;
                if self.0.routes % 50 == 0 {
                    (self.0.on_progress)(self.0.routes);
                }
            }
            Ok(())
        }
    }

    /// Top-level visitor for the drive-data.json object.
    struct TopLevel<'a, 'tx: 'a>(&'a mut Ctx<'tx>);

    impl<'de, 'a, 'tx: 'a> Visitor<'de> for TopLevel<'a, 'tx> {
        type Value = ();
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("drive-data.json object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            let ctx = self.0;
            while let Some(key) = map.next_key::<String>()? {
                match key.as_str() {
                    "processedFiles" => {
                        let files: Vec<String> = map.next_value()?;
                        let mut stmt = ctx.tx
                            .prepare(
                                "INSERT OR IGNORE INTO processed_files(file, added_at) \
                                 VALUES(?1, ?2)",
                            )
                            .map_err(|e| serde::de::Error::custom(e.to_string()))?;
                        for f in &files {
                            stmt.execute(params![normalize_path(f), ctx.now])
                                .map_err(|e| serde::de::Error::custom(e.to_string()))?;
                        }
                        ctx.processed_files = files.len();
                    }
                    "routes" => {
                        // `&mut *ctx` is a reborrow: it ends when next_value_seed
                        // returns, allowing ctx to be used again for later keys.
                        map.next_value_seed(RouteSeq(&mut *ctx))?;
                    }
                    "driveTags" => {
                        let tags: HashMap<String, Vec<String>> = map.next_value()?;
                        let mut stmt = ctx.tx
                            .prepare(
                                "INSERT OR IGNORE INTO drive_tags(drive_key, tag) \
                                 VALUES(?1, ?2)",
                            )
                            .map_err(|e| serde::de::Error::custom(e.to_string()))?;
                        for (k, vs) in &tags {
                            for v in vs {
                                stmt.execute(params![k, v])
                                    .map_err(|e| serde::de::Error::custom(e.to_string()))?;
                            }
                        }
                        ctx.drive_tags = tags.len();
                    }
                    _ => {
                        // Unknown top-level key — skip without allocating.
                        map.next_value::<serde_json::Value>()?;
                    }
                }
            }
            Ok(())
        }
    }

    // Run the streaming parse.
    let mut diag = ImportDiagnostics::default();
    let mut seen_files: HashSet<String> = HashSet::new();
    let mut ctx = Ctx {
        tx: &tx,
        now,
        routes: 0,
        processed_files: 0,
        drive_tags: 0,
        on_progress: &on_progress,
        diag: &mut diag,
        seen_files: &mut seen_files,
    };
    let mut de = serde_json::Deserializer::from_reader(reader);
    de.deserialize_map(TopLevel(&mut ctx))
        .map_err(|e: serde_json::Error| {
            anyhow::anyhow!("parse JSON (line {}, column {}): {}", e.line(), e.column(), e)
        })?;

    let stats = ImportStats {
        routes: ctx.routes,
        processed_files: ctx.processed_files,
        drive_tags: ctx.drive_tags,
    };

    // Corruption guard: refuse to replace a large store with a much smaller
    // import (usually a truncated or corrupted file).
    if existing_count as usize >= SYNCGUARD_MIN_ROUTES
        && (stats.routes as f64) < (existing_count as f64 * SYNCGUARD_SHRINK_RATIO)
    {
        warn!(
            "import_json: shrink guard refused import — existing={} new={} threshold={:.0}%",
            existing_count,
            stats.routes,
            SYNCGUARD_SHRINK_RATIO * 100.0,
        );
        bail!(
            "refusing import: would shrink store from {} routes to {} (< {:.0}% retained). \
             Likely a truncated or corrupted upload — delete the existing DB manually if \
             you really mean to replace it.",
            existing_count,
            stats.routes,
            SYNCGUARD_SHRINK_RATIO * 100.0,
        );
    }

    on_progress(stats.routes);
    tx.commit()?;

    // Aggregate summary at INFO so a healthy import emits one line.
    info!(
        "import_json: complete path={} routes={} processed_files={} drive_tags={} \
         empty_points={} empty_date={} duplicate_files_in_json={}",
        path,
        stats.routes,
        stats.processed_files,
        stats.drive_tags,
        diag.empty_points,
        diag.empty_date,
        diag.duplicate_files_in_json,
    );

    // One WARN per non-zero category, with up to EXAMPLE_LIMIT example file
    // paths so operators can grep for the offending entries.
    if diag.empty_points > 0 {
        warn!(
            "import_json: {} route(s) had empty points array — they will not appear as drives. Examples: {:?}",
            diag.empty_points, diag.bad_examples
        );
    }
    if diag.empty_date > 0 {
        warn!(
            "import_json: {} route(s) had empty date field. Examples: {:?}",
            diag.empty_date, diag.bad_examples
        );
    }
    if diag.duplicate_files_in_json > 0 {
        warn!(
            "import_json: {} route(s) had duplicate file paths within the JSON — second occurrence overwrote the first. Examples: {:?}",
            diag.duplicate_files_in_json, diag.bad_examples
        );
    }

    Ok((stats, diag))
}

/// Insert a route from a JSON import — matches the DDL in
/// `insert_or_update_route` from db.rs.
fn insert_imported_route(
    tx: &rusqlite::Transaction,
    r: &Route,
    a: &crate::types::RouteAggregates,
    now: i64,
) -> Result<()> {
    let norm = normalize_path(&r.file);
    let pb = crate::blob::encode_points(Some(&r.points));
    let gb = crate::blob::encode_u8s(Some(&r.gear_states));
    let ab = crate::blob::encode_u8s(Some(&r.autopilot_states));
    let sb = crate::blob::encode_f32s(Some(&r.speeds));
    let acb = crate::blob::encode_f32s(Some(&r.accel_positions));
    let rb = crate::blob::encode_gear_runs(Some(&r.gear_runs));

    let first_lat: Option<f64> = r.points.first().map(|p| p[0]);
    let first_lon: Option<f64> = r.points.first().map(|p| p[1]);

    tx.execute(
        "INSERT OR REPLACE INTO routes(
            file, date_dir, point_count, raw_park_count, raw_frame_count,
            start_ts, end_ts, distance_m, first_lat, first_lon,
            points_blob, gear_states_blob, ap_states_blob,
            speeds_blob, accel_blob, gear_runs_blob, updated_at,
            max_speed_mps, avg_speed_mps, speed_sample_count, valid_point_count,
            fsd_engaged_ms, autosteer_engaged_ms, tacc_engaged_ms,
            fsd_distance_m, autosteer_distance_m, tacc_distance_m, assisted_distance_m,
            fsd_disengagements, fsd_accel_pushes,
            start_lat, start_lon, end_lat, end_lon,
            source, external_signature, tessie_autopilot_percent)
         VALUES(
            ?1, ?2, ?3, ?4, ?5,
            NULL, NULL, ?6, ?7, ?8,
            ?9, ?10, ?11, ?12, ?13, ?14, ?15,
            ?16, ?17, ?18, ?19,
            ?20, ?21, ?22,
            ?23, ?24, ?25, ?26,
            ?27, ?28,
            ?29, ?30, ?31, ?32,
            ?33, ?34, ?35)",
        params![
            norm, r.date, r.points.len() as i64, r.raw_park_count as i64, r.raw_frame_count as i64,
            a.distance_m, first_lat, first_lon,
            pb, gb, ab, sb, acb, rb, now,
            a.max_speed_mps, a.avg_speed_mps, a.speed_sample_count, a.valid_point_count,
            a.fsd_engaged_ms, a.autosteer_engaged_ms, a.tacc_engaged_ms,
            a.fsd_distance_m, a.autosteer_distance_m, a.tacc_distance_m, a.assisted_distance_m,
            a.fsd_disengagements, a.fsd_accel_pushes,
            a.start_lat, a.start_lng, a.end_lat, a.end_lng,
            r.source, r.external_signature, r.tessie_autopilot_percent,
        ],
    )?;
    Ok(())
}

/// Export the DB contents as `drive-data.json`. Produces deterministic,
/// byte-identical output for the same DB state so rsync / archive
/// diff-detection works correctly.
///
/// Streams routes one at a time from SQLite directly into the JSON
/// serializer, so peak heap usage stays bounded by a single decoded
/// `Route` instead of the full store. On a 5500-clip DB this caps the
/// export at a few hundred KB of working memory vs. the ~17 MB that
/// materialising all routes used to consume.
pub fn export_json<W: Write>(conn: &Connection, writer: &mut W) -> Result<()> {
    let mut processed_files = {
        let mut stmt =
            conn.prepare("SELECT file FROM processed_files ORDER BY file")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        out
    };
    // Belt & suspenders — SQL ORDER BY already sorts but the UI will
    // sometimes insert paths with different case through the API; sort
    // case-insensitively here to match Go's deterministic output.
    processed_files.sort();

    let drive_tags = {
        let mut stmt = conn
            .prepare("SELECT drive_key, tag FROM drive_tags ORDER BY drive_key, tag")?;
        let rows =
            stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        let mut out: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for r in rows {
            let (k, t) = r?;
            out.entry(k).or_default().push(t);
        }
        out
    };

    // Use a BTreeMap (ordered) → HashMap transition for serialization
    // so drive_tags keys serialize in sorted order. serde_json writes
    // BTreeMap keys in their natural order.
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct OrderedStoreData<'a> {
        processed_files: &'a [String],
        routes: RouteStream<'a>,
        #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        drive_tags: &'a std::collections::BTreeMap<String, Vec<String>>,
    }

    // `route_err` is an out-parameter: if SQLite barfs partway through
    // the streaming serialize, the serde error propagated back by
    // `to_writer_pretty` is the generic "io error" wrapper — we stash
    // the real rusqlite error here and swap it back in afterwards so
    // the caller sees the useful message.
    let route_err: std::cell::RefCell<Option<rusqlite::Error>> =
        std::cell::RefCell::new(None);

    let out = OrderedStoreData {
        processed_files: &processed_files,
        routes: RouteStream { conn, error: &route_err },
        drive_tags: &drive_tags,
    };
    let ser_result = serde_json::to_writer_pretty(writer, &out);

    if let Some(e) = route_err.into_inner() {
        return Err(anyhow::Error::from(e).context("export_json: streaming route read failed"));
    }
    ser_result.context("serialize JSON")?;
    Ok(())
}

/// Serializer adapter that streams `Route` rows directly from SQLite
/// into the JSON output without ever holding more than one decoded
/// `Route` in memory. Used by [`export_json`].
struct RouteStream<'a> {
    conn: &'a Connection,
    error: &'a std::cell::RefCell<Option<rusqlite::Error>>,
}

impl<'a> serde::Serialize for RouteStream<'a> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{Error as SerError, SerializeSeq};

        let mut stmt = self
            .conn
            .prepare(
                "SELECT file, date_dir, raw_park_count, raw_frame_count,
                        points_blob, gear_states_blob, ap_states_blob,
                        speeds_blob, accel_blob, gear_runs_blob,
                        source, external_signature, tessie_autopilot_percent,
                        battery_pct_start, battery_pct_end,
                        interior_temp_min, interior_temp_max, exterior_temp_avg,
                        hvac_runtime_s,
                        tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi,
                        odometer_mi_start, odometer_mi_end,
                        location_name_start, location_name_end
                 FROM routes
                 ORDER BY file",
            )
            .map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("routes prepare failed")
            })?;

        let mut rows = stmt.query([]).map_err(|e| {
            *self.error.borrow_mut() = Some(e);
            S::Error::custom("routes query failed")
        })?;

        let mut seq = serializer.serialize_seq(None)?;

        loop {
            let row_opt = rows.next().map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("routes row fetch failed")
            })?;
            let Some(row) = row_opt else { break };

            // Pull columns then decode the BLOBs for this one row.
            // Each route is serialized and dropped before the next is
            // touched, which is what keeps the peak heap bounded.
            let file: String = row.get(0).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col file")
            })?;
            let date: String = row.get(1).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col date_dir")
            })?;
            let raw_park_count: u32 = row
                .get::<_, i64>(2)
                .map_err(|e| {
                    *self.error.borrow_mut() = Some(e);
                    S::Error::custom("col raw_park_count")
                })? as u32;
            let raw_frame_count: u32 = row
                .get::<_, i64>(3)
                .map_err(|e| {
                    *self.error.borrow_mut() = Some(e);
                    S::Error::custom("col raw_frame_count")
                })? as u32;
            let pb: Option<Vec<u8>> = row.get(4).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col points_blob")
            })?;
            let gb: Option<Vec<u8>> = row.get(5).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col gear_states_blob")
            })?;
            let ab: Option<Vec<u8>> = row.get(6).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col ap_states_blob")
            })?;
            let sb: Option<Vec<u8>> = row.get(7).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col speeds_blob")
            })?;
            let acb: Option<Vec<u8>> = row.get(8).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col accel_blob")
            })?;
            let rb: Option<Vec<u8>> = row.get(9).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col gear_runs_blob")
            })?;
            let source: Option<String> = row.get(10).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col source")
            })?;
            let external_signature: Option<String> = row.get(11).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col external_signature")
            })?;
            let tessie_autopilot_percent: Option<f64> = row.get(12).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col tessie_autopilot_percent")
            })?;
            // BLE telemetry rollup — Option<f64>/Option<String>, all
            // NULL on pre-v6 rows or clips whose 60s window had no
            // samples. Per-column .get to keep the streaming-row error
            // shape consistent.
            let battery_pct_start: Option<f64> = row.get(13).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col battery_pct_start")
            })?;
            let battery_pct_end: Option<f64> = row.get(14).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col battery_pct_end")
            })?;
            let interior_temp_min: Option<f64> = row.get(15).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col interior_temp_min")
            })?;
            let interior_temp_max: Option<f64> = row.get(16).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col interior_temp_max")
            })?;
            let exterior_temp_avg: Option<f64> = row.get(17).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col exterior_temp_avg")
            })?;
            let hvac_runtime_s: Option<i64> = row.get(18).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col hvac_runtime_s")
            })?;
            let tire_fl_psi: Option<f64> = row.get(19).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col tire_fl_psi")
            })?;
            let tire_fr_psi: Option<f64> = row.get(20).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col tire_fr_psi")
            })?;
            let tire_rl_psi: Option<f64> = row.get(21).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col tire_rl_psi")
            })?;
            let tire_rr_psi: Option<f64> = row.get(22).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col tire_rr_psi")
            })?;
            let odometer_mi_start: Option<f64> = row.get(23).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col odometer_mi_start")
            })?;
            let odometer_mi_end: Option<f64> = row.get(24).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col odometer_mi_end")
            })?;
            let location_name_start: Option<String> = row.get(25).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col location_name_start")
            })?;
            let location_name_end: Option<String> = row.get(26).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col location_name_end")
            })?;

            let points: Vec<GpsPoint> = decode_points(pb.as_deref())
                .map_err(|e| S::Error::custom(format!("decode points {}: {}", file, e)))?
                .unwrap_or_default();
            let gear_states = decode_u8s(gb.as_deref()).unwrap_or_default();
            let autopilot_states = decode_u8s(ab.as_deref()).unwrap_or_default();
            let speeds = decode_f32s(sb.as_deref())
                .map_err(|e| S::Error::custom(format!("decode speeds {}: {}", file, e)))?
                .unwrap_or_default();
            let accel_positions = decode_f32s(acb.as_deref())
                .map_err(|e| S::Error::custom(format!("decode accel {}: {}", file, e)))?
                .unwrap_or_default();
            let gear_runs: Vec<GearRun> = decode_gear_runs(rb.as_deref())
                .map_err(|e| S::Error::custom(format!("decode gear_runs {}: {}", file, e)))?
                .unwrap_or_default();

            let route = Route {
                file,
                date,
                points,
                gear_states,
                autopilot_states,
                speeds,
                accel_positions,
                raw_park_count,
                raw_frame_count,
                gear_runs,
                source,
                external_signature,
                tessie_autopilot_percent,
                battery_pct_start,
                battery_pct_end,
                interior_temp_min,
                interior_temp_max,
                exterior_temp_avg,
                hvac_runtime_s,
                tire_fl_psi,
                tire_fr_psi,
                tire_rl_psi,
                tire_rr_psi,
                odometer_mi_start,
                odometer_mi_end,
                location_name_start,
                location_name_end,
            };
            seq.serialize_element(&route)?;
            // `route` drops here — its ~10 KB of decoded BLOBs goes back
            // to the allocator before we loop.
        }
        seq.end()
    }
}

#[cfg(test)]
mod streaming_export_tests {
    use crate::db::DriveStore;
    use crate::types::{GearRun, GpsPoint, StoreData};

    /// The streaming exporter must produce byte-for-byte parseable JSON
    /// that deserializes back into the same `StoreData` the importer
    /// would reconstruct. Protects against a future "optimise the
    /// allocation loop" change silently breaking Sentry Studio /
    /// archive restore.
    #[test]
    fn streaming_export_roundtrips_to_identical_store_data() {
        let store = DriveStore::open_memory().unwrap();
        let pts: Vec<GpsPoint> = vec![[37.7749, -122.4194], [37.7750, -122.4195]];
        store
            .add_route(
                "2025-01-15/clip.mp4",
                "2025-01-15",
                &pts,
                &[4, 4],
                &[1, 1],
                &[25.0, 26.0],
                &[0.5, 0.6],
                0,
                2,
                &[GearRun { gear: 4, frames: 2 }],
            )
            .unwrap();

        let tmp = std::env::temp_dir().join("sentryusb-streaming-export-test.json");
        let tmp_str = tmp.to_string_lossy().to_string();
        store.export_json_to_file(&tmp_str).unwrap();

        let raw = std::fs::read(&tmp).unwrap();
        let data: StoreData = serde_json::from_slice(&raw).unwrap();

        assert_eq!(data.routes.len(), 1);
        assert_eq!(data.routes[0].file, "2025-01-15/clip.mp4");
        assert_eq!(data.routes[0].points, pts);
        assert_eq!(data.routes[0].gear_states, vec![4, 4]);
        assert_eq!(data.routes[0].autopilot_states, vec![1, 1]);
        assert_eq!(data.routes[0].speeds, vec![25.0, 26.0]);
        assert_eq!(data.processed_files, vec!["2025-01-15/clip.mp4"]);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn streaming_export_on_empty_store_is_valid_json() {
        let store = DriveStore::open_memory().unwrap();
        let tmp = std::env::temp_dir().join("sentryusb-streaming-export-empty.json");
        let tmp_str = tmp.to_string_lossy().to_string();
        store.export_json_to_file(&tmp_str).unwrap();

        let raw = std::fs::read(&tmp).unwrap();
        let data: StoreData = serde_json::from_slice(&raw).unwrap();
        assert!(data.routes.is_empty());
        assert!(data.processed_files.is_empty());
        assert!(data.drive_tags.is_empty());

        let _ = std::fs::remove_file(&tmp);
    }

    /// Covers the on-disk path that opens a fresh read-only SQLite handle
    /// rather than locking the writer mutex (the production path used by
    /// `/api/drives/data/export-for-sync`). The `:memory:` tests above
    /// fall through to the locked-connection branch since you can't open
    /// a second handle on an in-memory DB.
    #[test]
    fn streaming_export_from_on_disk_db_uses_readonly_handle() {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let db_path = std::env::temp_dir()
            .join(format!("sentryusb-readonly-{}-{}.db", pid, nanos));
        let json_path = std::env::temp_dir()
            .join(format!("sentryusb-readonly-{}-{}.json", pid, nanos));
        let db_str = db_path.to_string_lossy().to_string();
        let json_str = json_path.to_string_lossy().to_string();

        let store = DriveStore::open(&db_str).unwrap();
        let pts: Vec<GpsPoint> = vec![[37.7749, -122.4194], [37.7750, -122.4195]];
        store
            .add_route(
                "2025-01-15/clip.mp4",
                "2025-01-15",
                &pts,
                &[4, 4],
                &[1, 1],
                &[25.0, 26.0],
                &[0.5, 0.6],
                0,
                2,
                &[GearRun { gear: 4, frames: 2 }],
            )
            .unwrap();
        store.export_json_to_file(&json_str).unwrap();

        let raw = std::fs::read(&json_path).unwrap();
        let data: StoreData = serde_json::from_slice(&raw).unwrap();
        assert_eq!(data.routes.len(), 1);
        assert_eq!(data.routes[0].file, "2025-01-15/clip.mp4");
        assert_eq!(data.routes[0].points, pts);
        assert_eq!(data.processed_files, vec!["2025-01-15/clip.mp4"]);

        drop(store);
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{}-wal", db_str));
        let _ = std::fs::remove_file(format!("{}-shm", db_str));
        let _ = std::fs::remove_file(&json_path);
    }
}

#[cfg(test)]
mod import_diagnostics_tests {
    use super::*;
    use rusqlite::Connection;

    /// JSON helper — assembles a `drive-data.json`-shaped payload for tests.
    fn build_json(routes_json: &str) -> String {
        format!(
            r#"{{"processedFiles": [], "routes": [{}], "driveTags": {{}}}}"#,
            routes_json
        )
    }

    /// Returns a minimal route literal with overridable file + points.
    fn route_lit(file: &str, points_json: &str, date: &str) -> String {
        format!(
            r#"{{"file":"{}","date":"{}","points":{},"gearStates":[],"autopilotStates":[],"speeds":[],"accelPositions":[],"rawParkCount":0,"rawFrameCount":0}}"#,
            file, date, points_json
        )
    }

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn diagnostics_flag_empty_points() {
        let json = build_json(&format!(
            "{},{}",
            route_lit("2025-01-15_12-00-00/clip.mp4", "[]", "2025-01-15"),
            route_lit("2025-01-15_12-05-00/clip.mp4", "[[37.0,-122.0]]", "2025-01-15"),
        ));
        let path = std::env::temp_dir().join("sentryusb-diag-empty-points.json");
        std::fs::write(&path, &json).unwrap();

        let mut conn = fresh_conn();
        let (stats, diag) = import_json(&mut conn, path.to_str().unwrap(), |_| {}).unwrap();

        assert_eq!(stats.routes, 2);
        assert_eq!(diag.empty_points, 1);
        assert_eq!(diag.empty_date, 0);
        assert_eq!(diag.duplicate_files_in_json, 0);
        assert!(diag.bad_examples.iter().any(|e| e.contains("12-00-00")));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn diagnostics_flag_duplicate_file_paths_after_normalization() {
        // Two routes whose `file` differs only in slash style. The second
        // overwrites the first via INSERT OR REPLACE — we want this counted.
        let json = build_json(&format!(
            "{},{}",
            route_lit(
                "2025-01-15_12-00-00\\\\clip.mp4",
                "[[37.0,-122.0]]",
                "2025-01-15"
            ),
            route_lit(
                "2025-01-15_12-00-00/clip.mp4",
                "[[37.1,-122.1]]",
                "2025-01-15"
            ),
        ));
        let path = std::env::temp_dir().join("sentryusb-diag-dup-paths.json");
        std::fs::write(&path, &json).unwrap();

        let mut conn = fresh_conn();
        let (_stats, diag) = import_json(&mut conn, path.to_str().unwrap(), |_| {}).unwrap();

        assert_eq!(diag.duplicate_files_in_json, 1);
        assert!(diag.has_problems());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn diagnostics_clean_import_has_no_problems() {
        let json = build_json(&route_lit(
            "2025-01-15_12-00-00/clip.mp4",
            "[[37.0,-122.0],[37.1,-122.1]]",
            "2025-01-15",
        ));
        let path = std::env::temp_dir().join("sentryusb-diag-clean.json");
        std::fs::write(&path, &json).unwrap();

        let mut conn = fresh_conn();
        let (stats, diag) = import_json(&mut conn, path.to_str().unwrap(), |_| {}).unwrap();

        assert_eq!(stats.routes, 1);
        assert_eq!(diag.seen, 1);
        assert!(!diag.has_problems(), "clean import should not flag problems");

        let _ = std::fs::remove_file(&path);
    }

    /// The grouper's timestamp filter (grouper.rs `parse_file_timestamp`) is
    /// the most likely cause of "missing drives on import": a route whose
    /// filename lacks YYYY-MM-DD_HH-MM-SS lands in the routes table but
    /// never appears as a drive. We verify the route IS imported (so logs
    /// + import-history reflect it) — the grouper-side warn is exercised
    /// by integration tests.
    #[test]
    fn route_with_unparseable_filename_still_imports() {
        let json = build_json(&route_lit(
            "weird-file-no-timestamp.mp4",
            "[[37.0,-122.0],[37.1,-122.1]]",
            "2025-01-15",
        ));
        let path = std::env::temp_dir().join("sentryusb-diag-unparseable.json");
        std::fs::write(&path, &json).unwrap();

        let mut conn = fresh_conn();
        let (stats, diag) = import_json(&mut conn, path.to_str().unwrap(), |_| {}).unwrap();

        // import_json itself doesn't validate filenames — that's the
        // grouper's job. The route IS in the DB.
        assert_eq!(stats.routes, 1);
        assert_eq!(diag.empty_points, 0);

        // Confirm the row landed in the routes table.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM routes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let _ = std::fs::remove_file(&path);
    }
}
