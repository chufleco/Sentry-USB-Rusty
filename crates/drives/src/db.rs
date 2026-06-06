//! Drive data store backed by SQLite.
//!
//! Thread-safety: WAL handles SQLite's internal locking, but a
//! `Mutex<Connection>` still serializes writes for the Pi's single-writer
//! setup. Atomic counters give `/api/drives/status` polling a lock-free
//! fast path.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, ToSql};
use tracing::{info, warn};

use crate::aggregate::compute_route_aggregates;
use crate::backfill::backfill_route_aggregates;
use crate::blob::{
    decode_f32s, decode_gear_runs, decode_points, decode_u8s, encode_f32s, encode_gear_runs,
    encode_points, encode_u8s,
};
use crate::schema::{self, meta_get, meta_set};
use crate::syncguard::{self, check_sync_size_guard, read_sync_cache, write_sync_cache};
use crate::types::{GearRun, GpsPoint, Route, RouteAggregates, RouteSummary, StoreData};

/// Default SQLite DB path on the Pi.
pub const DEFAULT_DATA_PATH: &str = "/backingfiles/drive-data.db";

/// JSON staging mirror — regenerated on demand by `ExportJSONForSync` so
/// `post-archive-process.sh` can ship it to the archive server. Lives on
/// `/backingfiles` (same partition as the DB) because the export can
/// reach hundreds of MB on a long-used Pi and the 2 GB `/mutable` partition
/// can't hold two copies during atomic write. The local copy is retained
/// after upload so rsync's delta-transfer protocol only sends changed bytes.
pub const DEFAULT_JSON_MIRROR_PATH: &str = "/backingfiles/drive-data.json";

/// Pre-SQLite data file on the read-only root. The JSON importer reads
/// this on first boot if the primary mirror is missing.
pub const LEGACY_JSON_PATH: &str = "/root/drive-data.json";

/// Archive-side JSON copy for CIFS/NFS mounts.
pub const ARCHIVE_DATA_PATH: &str = "/mnt/archive/drive-data.json";
// Bump on every drive-list-shape change so existing on-disk caches
// rebuild on first boot after upgrade.
//
// v3 (2026-04-28): switched cache from BLOB grouper (group_clips) to
// summary grouper (group_summary_clips), added hide_tessie_overlapping_sei
// filter, and changed `DriveSummary.date` to derive from start_time
// (was the raw date_dir column). Aligns the list endpoint with what
// the single-drive endpoint already does, so clicking a drive in the
// list returns the matching points.
//
// v4 (2026-05-19): drive grouping output changed since v3 without a bump —
// SavedClips/SentryClips event folders are now skipped, and the grouper was
// reworked to match Sentry-Drive/Sentry-Cloud distance/AP math. Same routes
// now yield a different drive list, so stale v3 caches must rebuild.
const DRIVE_LIST_CACHE_ALGO_VERSION: &str = "4";

/// Ordered list of paths the one-shot importer checks on first boot.
/// The first that exists wins. `/mutable/drive-data.json` is kept as a
/// fallback so upgraders whose DB is still empty (mid-migration) still
/// get their legacy export imported — once the marker is set, this list
/// never runs again, and `cleanup_legacy_mutable_files` clears the
/// orphaned file in the steady-state.
pub const IMPORT_SOURCE_CANDIDATES: &[&str] = &[
    DEFAULT_JSON_MIRROR_PATH,        // /backingfiles/drive-data.json (new canonical)
    "/mutable/drive-data.json",      // legacy pre-2026-05 location (upgrade fallback)
    LEGACY_JSON_PATH,                // /root/drive-data.json (pre-SQLite)
];

/// Files this binary used to write under `/mutable` that are now obsolete and
/// are safe to delete at startup. Limited to the `drive-data.json` family
/// because the importer in `DriveStore::open` consumes the legacy
/// `/mutable/drive-data.json` (if any) *before* this cleanup runs, so the
/// data is already migrated into the DB. Other legacy paths (notification
/// history, preferences) are deliberately left alone — they're tiny and
/// some Rust code still reads them as a lazy fallback for upgraders whose
/// data only lives at the legacy location.
const LEGACY_MUTABLE_ORPHANS: &[&str] = &[
    "/mutable/drive-data.json",     // moved to /backingfiles/drive-data.json
    "/mutable/drive-data.json.tmp", // half-written atomic-rename leftover
];

/// Remove orphaned `/mutable` files left behind by older binaries that wrote
/// state to paths since moved to `/backingfiles`. Best-effort and idempotent:
/// missing files are silently skipped, removal failures are logged but never
/// abort startup. Safe to call on every boot. Sized so calling once at startup
/// is enough to keep the 2 GB `/mutable` partition stable across upgrades.
pub fn cleanup_legacy_mutable_files() {
    use std::path::Path;
    for path in LEGACY_MUTABLE_ORPHANS {
        if !Path::new(path).exists() {
            continue;
        }
        match std::fs::remove_file(path) {
            Ok(()) => tracing::info!(
                "cleanup_legacy_mutable_files: removed orphaned {}",
                path
            ),
            Err(e) => tracing::warn!(
                "cleanup_legacy_mutable_files: failed to remove {}: {}",
                path,
                e
            ),
        }
    }
}

/// Drive data store.
pub struct DriveStore {
    path: String,
    conn: Mutex<Connection>,
    /// Cached row counts so `/api/drives/status` doesn't hit SQLite for
    /// every poll.
    route_count: AtomicI64,
    processed_count: AtomicI64,
    /// Set whenever routes or tags change. `get_cached_drives_json` rebuilds
    /// and clears this flag before serving. Using a flag rather than
    /// rebuilding on every `add_route` call avoids O(n²) work when the
    /// processor adds hundreds of clips in a batch.
    drive_cache_dirty: AtomicBool,
}

impl DriveStore {
    /// Open (or create) the DB at `path`, apply migrations, run the
    /// one-shot JSON→DB import if needed, backfill v2 aggregate columns,
    /// and prime the row-count caches. Equivalent to Go `NewStore(p)
    /// + Load()`.
    pub fn open(path: &str) -> Result<Self> {
        let path = if path.is_empty() {
            DEFAULT_DATA_PATH.to_string()
        } else {
            path.to_string()
        };

        if let Some(parent) = Path::new(&path).parent() {
            if !parent.as_os_str().is_empty() && parent != Path::new("/") {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("open: mkdir {}", parent.display()))?;
            }
        }

        let conn = open_connection(&path)
            .with_context(|| format!("open: sql.Open {}", path))?;

        let store = DriveStore {
            path,
            conn: Mutex::new(conn),
            route_count: AtomicI64::new(0),
            processed_count: AtomicI64::new(0),
            drive_cache_dirty: AtomicBool::new(true),
        };

        store.load_locked(IMPORT_SOURCE_CANDIDATES)?;

        info!("Drive store opened at {}", store.path);
        Ok(store)
    }

    /// Opens an in-memory DB (for testing). Skips the one-shot JSON
    /// import since there's nothing on disk to import from.
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        apply_pragmas(&conn)?;
        let store = DriveStore {
            path: ":memory:".to_string(),
            conn: Mutex::new(conn),
            route_count: AtomicI64::new(0),
            processed_count: AtomicI64::new(0),
            drive_cache_dirty: AtomicBool::new(false),
        };
        // Still run migrate + backfill so tests exercise the real schema.
        let guard = store.conn.lock().unwrap();
        schema::migrate(&guard)?;
        drop(guard);
        store.refresh_counts()?;
        Ok(store)
    }

    /// Path the store was opened at.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Escape hatch for crates that need targeted SQL access without
    /// extending the public DriveStore API. Used by the cloud-uploader
    /// crate to read pending-upload rows and stamp `cloud_uploaded_at`
    /// without polluting this crate with cloud-specific methods.
    ///
    /// Holds the same connection mutex everything else uses, so callers
    /// share WAL serialization with `add_route` / `save` / etc. Keep the
    /// closure short — long-running work blocks all other DB I/O.
    pub fn with_locked_conn<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        let guard = self.conn.lock().unwrap();
        f(&guard)
    }

    /// Re-load (re-migrate + re-import). Safe to call multiple times.
    pub fn load(&self) -> Result<()> {
        self.load_locked(IMPORT_SOURCE_CANDIDATES)
    }

    fn load_locked(&self, import_candidates: &[&str]) -> Result<()> {
        {
            let mut guard = self.conn.lock().unwrap();
            schema::migrate(&guard).context("load: migrate")?;
            run_one_shot_import(&mut guard, import_candidates)
                .context("load: one-shot import")?;
            let mut mg = guard;
            let stats =
                backfill_route_aggregates(&mut mg, |done, total| {
                    info!("[drives] Backfilling summary aggregates: {}/{} routes", done, total);
                })
                .context("load: aggregate backfill")?;
            if stats.updated > 0 {
                info!(
                    "[drives] Summary backfill complete: {} routes updated",
                    stats.updated
                );
                meta_set(
                    &mg,
                    "summary_backfilled_at",
                    &chrono::Utc::now().to_rfc3339(),
                )?;
            }

            // Checkpoint the WAL after any import/backfill writes so the
            // subsequent grouper query runs against the main DB file with
            // no large WAL to walk through.
            let _ = mg.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");

            // Rebuild the drive list cache only when the DB contents differ
            // from what the cache was built from. On a typical restart where
            // nothing changed, this skips the expensive grouper run entirely
            // (two COUNT(*) queries instead of a full 5k-row table scan).
            if is_drive_cache_valid(&mg)? {
                info!("[drives] Drive list cache is current; skipping rebuild on startup");
            } else {
                rebuild_drive_list_cache(&mg).context("load: build drive cache")?;
            }
        }
        self.drive_cache_dirty.store(false, Ordering::Release);
        self.refresh_counts()?;
        Ok(())
    }

    /// Passive WAL checkpoint — called periodically by the processor so
    /// the `-wal` file doesn't grow unbounded during long runs. Errors
    /// are non-fatal (the data is already durable in the WAL).
    pub fn save(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
        Ok(())
    }

    /// Return the set of all processed file paths (normalized to forward
    /// slashes). Called once per ProcessDirectory run.
    pub fn processed_set(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached("SELECT file FROM processed_files")?;
        let files = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .map(|f| normalize_path(&f))
            .collect();
        Ok(files)
    }

    /// Mark a file processed without adding route data. Idempotent.
    pub fn mark_processed(&self, relative_path: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = now_unix();
        conn.execute(
            "INSERT OR IGNORE INTO processed_files(file, added_at) VALUES(?1, ?2)",
            params![normalize_path(relative_path), now],
        )?;
        drop(conn);
        self.refresh_counts()?;
        Ok(())
    }

    /// True if `file` has been processed.
    pub fn is_processed(&self, file: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let exists: i64 = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM processed_files WHERE file = ?1)",
                params![normalize_path(file)],
                |row| row.get(0),
            )
            .unwrap_or(0);
        Ok(exists != 0)
    }

    /// Add a processed file AND its route data. If `points` is empty the
    /// route row is skipped (the clip is still marked processed). If a
    /// route for `file` already exists it is upserted in place.
    ///
    /// Computes aggregate columns inline so the summary endpoints can
    /// serve BLOB-free rows
    /// exactly (single source of truth in `aggregate.rs`).
    #[allow(clippy::too_many_arguments)]
    pub fn add_route(
        &self,
        relative_path: &str,
        date_dir: &str,
        points: &[GpsPoint],
        gears: &[u8],
        ap_states: &[u8],
        speeds: &[f32],
        accel_positions: &[f32],
        raw_park_count: u32,
        raw_frame_count: u32,
        gear_runs: &[GearRun],
    ) -> Result<()> {
        let norm = normalize_path(relative_path);
        let now = now_unix();

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        tx.execute(
            "INSERT OR IGNORE INTO processed_files(file, added_at) VALUES(?1, ?2)",
            params![norm, now],
        )?;

        if !points.is_empty() {
            let route = Route {
                file: relative_path.to_string(),
                date: date_dir.to_string(),
                points: points.to_vec(),
                gear_states: gears.to_vec(),
                autopilot_states: ap_states.to_vec(),
                speeds: speeds.to_vec(),
                accel_positions: accel_positions.to_vec(),
                raw_park_count,
                raw_frame_count,
                gear_runs: gear_runs.to_vec(),
                source: None,
                external_signature: None,
                tessie_autopilot_percent: None,
                // BLE telemetry rollup is written separately by
                // `write_route_telemetry` after this insert lands; leave
                // None here.
                ..Default::default()
            };
            let agg = compute_route_aggregates(&route);
            insert_or_update_route(&tx, &norm, &route, &agg, now)?;

            // v6 telemetry rollup: join the just-inserted clip's
            // 60s window against any telemetry_samples that landed
            // in it. Best-effort — telemetry failure must not block
            // the route insert (drive grouping is the critical path
            // here, telemetry is a value-add).
            match crate::aggregate_telemetry::compute_telemetry_for_route(&tx, &norm) {
                Ok(tele) => {
                    if let Err(e) = crate::aggregate_telemetry::write_route_telemetry(
                        &tx, &norm, &tele,
                    ) {
                        warn!("telemetry write failed for {}: {}", norm, e);
                    }
                }
                Err(e) => warn!("telemetry compute failed for {}: {}", norm, e),
            }
        }

        tx.commit()?;
        drop(conn);
        self.drive_cache_dirty.store(true, Ordering::Release);
        self.refresh_counts()?;
        Ok(())
    }

    /// Cached route count (O(1)).
    pub fn route_count(&self) -> i64 {
        self.route_count.load(Ordering::Relaxed)
    }

    /// Cached processed-files count (O(1)).
    pub fn processed_count(&self) -> i64 {
        self.processed_count.load(Ordering::Relaxed)
    }

    /// Fresh `Vec<Route>` decoded from the DB. Hot-path readers should
    /// use [`with_routes`] instead to avoid the allocation.
    pub fn get_routes(&self) -> Result<Vec<Route>> {
        let conn = self.conn.lock().unwrap();
        select_all_routes(&conn)
    }

    /// Materialize all routes and invoke `f` with the resulting slice.
    /// Slice and elements must not be retained beyond `f`'s return.
    pub fn with_routes<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&[Route]) -> R,
    {
        let routes = self.get_routes()?;
        Ok(f(&routes))
    }

    /// BLOB-free analogue of `with_routes`: materializes per-route
    /// metadata + pre-computed aggregate columns, excluding all point-data
    /// BLOBs. On a 5500-route DB this costs ~5 MB of heap instead of
    /// ~300 MB for the full `WithRoutes` materialization.
    pub fn with_route_summaries<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&[RouteSummary]) -> R,
    {
        let conn = self.conn.lock().unwrap();
        let summaries = select_all_route_summaries(&conn)?;
        Ok(f(&summaries))
    }

    /// Fetch full `Route` rows (with all BLOB columns decoded) for the
    /// named files. Intended for the single-drive detail view: after
    /// [`with_route_summaries`] has identified which ~1-20 clips make up
    /// a drive, this avoids materialising the ~5500-row full store just
    /// to answer a single-drive request. Missing files are silently
    /// skipped — the caller can compare input vs. output lengths if it
    /// needs to detect a tag/race gap.
    pub fn with_routes_by_files<F, R>(&self, files: &[&str], f: F) -> Result<R>
    where
        F: FnOnce(&[Route]) -> R,
    {
        let conn = self.conn.lock().unwrap();
        let routes = select_routes_by_files(&conn, files)?;
        Ok(f(&routes))
    }

    /// Wipe routes + processed_files + drive_tags and bulk-insert `data`.
    /// Used by `POST /api/drives/data/upload` to restore a previously-
    /// downloaded `drive-data.json`.
    pub fn replace_data(&self, data: &StoreData) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for stmt in &[
            "DELETE FROM routes",
            "DELETE FROM processed_files",
            "DELETE FROM drive_tags",
        ] {
            tx.execute(stmt, [])?;
        }
        let now = now_unix();
        let mut seen = std::collections::HashSet::new();
        {
            let mut pf = tx.prepare(
                "INSERT OR IGNORE INTO processed_files(file, added_at) VALUES(?1, ?2)",
            )?;
            for f in &data.processed_files {
                let n = normalize_path(f);
                if !seen.insert(n.clone()) {
                    continue;
                }
                pf.execute(params![n, now])?;
            }

            for r in &data.routes {
                let n = normalize_path(&r.file);
                let agg = compute_route_aggregates(r);
                insert_or_update_route(&tx, &n, r, &agg, now)?;
                if seen.insert(n.clone()) {
                    pf.execute(params![n, now])?;
                }
            }
        }
        {
            let mut ts = tx.prepare(
                "INSERT OR IGNORE INTO drive_tags(drive_key, tag) VALUES(?1, ?2)",
            )?;
            for (key, tags) in &data.drive_tags {
                for t in tags {
                    ts.execute(params![key, t])?;
                }
            }
        }
        tx.commit()?;
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
        drop(conn);
        self.drive_cache_dirty.store(true, Ordering::Release);
        self.refresh_counts()?;
        Ok(())
    }

    /// Full store snapshot — all routes + processed files + tags. Used
    /// by `GET /api/drives/data/download`. Allocates the whole payload.
    pub fn get_data(&self) -> Result<StoreData> {
        let conn = self.conn.lock().unwrap();

        let routes = select_all_routes(&conn)?;

        let mut processed_files = Vec::new();
        {
            let mut stmt = conn
                .prepare("SELECT file FROM processed_files ORDER BY file")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            for r in rows {
                processed_files.push(r?);
            }
        }

        let mut drive_tags = std::collections::HashMap::<String, Vec<String>>::new();
        {
            let mut stmt = conn.prepare_cached("SELECT drive_key, tag FROM drive_tags")?;
            let rows = stmt
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
            for r in rows {
                let (key, tag) = r?;
                drive_tags.entry(key).or_default().push(tag);
            }
        }

        Ok(StoreData {
            processed_files,
            routes,
            drive_tags,
        })
    }

    /// Replace the tags for `drive_key`. Empty/zero-length `tags` drops
    /// the entry entirely.
    pub fn set_drive_tags(&self, drive_key: &str, tags: &[String]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM drive_tags WHERE drive_key = ?1",
            params![drive_key],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO drive_tags(drive_key, tag) VALUES(?1, ?2)",
            )?;
            for t in tags {
                if t.is_empty() {
                    continue;
                }
                stmt.execute(params![drive_key, t])?;
            }
        }
        tx.commit()?;
        self.drive_cache_dirty.store(true, Ordering::Release);
        Ok(())
    }

    /// Tags for a drive, or an empty vec.
    pub fn get_drive_tags(&self, drive_key: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT tag FROM drive_tags WHERE drive_key = ?1 ORDER BY tag",
        )?;
        let out = stmt
            .query_map(params![drive_key], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(out)
    }

    /// Full drive_key → tags map.
    pub fn get_all_drive_tags(&self) -> Result<std::collections::HashMap<String, Vec<String>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT drive_key, tag FROM drive_tags ORDER BY drive_key, tag",
        )?;
        let rows =
            stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        let mut out = std::collections::HashMap::<String, Vec<String>>::new();
        for r in rows {
            let (k, t) = r?;
            out.entry(k).or_default().push(t);
        }
        Ok(out)
    }

    /// Every tag name in use, sorted and deduplicated.
    pub fn get_all_tag_names(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached("SELECT DISTINCT tag FROM drive_tags ORDER BY tag")?;
        let tags = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(tags)
    }

    /// Empty `processed_files` so every clip becomes eligible for
    /// re-extraction. Routes and drive_tags are preserved.
    pub fn clear_processed_for_reprocess(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM processed_files", [])?;
        drop(conn);
        self.refresh_counts()?;
        Ok(())
    }

    /// Wipe routes, processed_files, and drive_tags — clean slate.
    pub fn clear_all_drives(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        for stmt in &[
            "DELETE FROM routes",
            "DELETE FROM processed_files",
            "DELETE FROM drive_tags",
        ] {
            conn.execute(stmt, [])?;
        }
        drop(conn);
        self.drive_cache_dirty.store(true, Ordering::Release);
        self.refresh_counts()?;
        Ok(())
    }

    /// Bulk-delete the specified clip files from `routes` and
    /// `processed_files`, and prune any matching `drive_tags` rows.
    /// `drive_keys` lists the start_time strings used as `drive_tags`
    /// keys so the tag rows tied to the removed drives are cleaned up
    /// at the same time.
    ///
    /// All deletes run in a single transaction so a partial failure
    /// can't leave routes deleted but processed_files stale (which
    /// would silently prevent reprocessing). Returns the number of
    /// routes actually removed — useful for surfacing "deleted 42
    /// drives (61 clips)" in the UI.
    pub fn delete_routes_by_files(
        &self,
        files: &[String],
        drive_keys: &[String],
    ) -> Result<usize> {
        if files.is_empty() && drive_keys.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut deleted: usize = 0;
        if !files.is_empty() {
            let mut del_routes =
                tx.prepare("DELETE FROM routes WHERE file = ?1")?;
            let mut del_processed =
                tx.prepare("DELETE FROM processed_files WHERE file = ?1")?;
            for f in files {
                let n = normalize_path(f);
                deleted += del_routes.execute(params![n])?;
                del_processed.execute(params![n])?;
            }
        }
        if !drive_keys.is_empty() {
            let mut del_tags =
                tx.prepare("DELETE FROM drive_tags WHERE drive_key = ?1")?;
            for k in drive_keys {
                del_tags.execute(params![k])?;
            }
        }
        tx.commit()?;
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
        drop(conn);
        self.drive_cache_dirty.store(true, Ordering::Release);
        self.refresh_counts()?;
        Ok(deleted)
    }

    /// Regenerate the canonical `/backingfiles/drive-data.json` mirror for
    /// `post-archive-process.sh`. Idempotent; safe alongside reads.
    ///
    /// Legacy path: hard-wires `experimental = false` (the `JsonCompatCodec`)
    /// so this zero-arg method's bytes never change. Callers that want the
    /// flag honoured use [`export_json_for_sync_with`].
    pub fn export_json_for_sync(&self) -> Result<()> {
        self.export_json_for_sync_with(false)
    }

    /// Codec-aware variant of [`export_json_for_sync`]. `experimental`
    /// selects the codec via [`crate::store_codec::select_codec`]; `false`
    /// is the legacy `JsonCompatCodec` and is byte-for-byte identical to the
    /// zero-arg method. The api crate reads `SENTRYUSB_EXPERIMENTAL` and
    /// passes the bool in — this crate never reads the flag itself.
    pub fn export_json_for_sync_with(&self, experimental: bool) -> Result<()> {
        self.export_json_to_file_with(DEFAULT_JSON_MIRROR_PATH, experimental)
    }

    /// Import a drive-data.json file into the store. Thin wrapper around
    /// [`json_compat::import_json`](crate::json_compat::import_json) that
    /// takes care of locking the shared connection.
    pub fn import_json_file(
        &self,
        path: &str,
    ) -> Result<crate::json_compat::ImportStats> {
        self.import_json_file_with_progress(path, |_| {})
    }

    /// Codec-aware variant of [`import_json_file`]. `experimental` selects
    /// the codec; `false` is the legacy path.
    pub fn import_json_file_with(
        &self,
        path: &str,
        experimental: bool,
    ) -> Result<crate::json_compat::ImportStats> {
        self.import_json_file_with_progress_with(path, |_| {}, experimental)
    }

    /// Like [`import_json_file`] but invokes `on_progress(routes_seen)`
    /// once the decoder knows the total route count. Used by the
    /// upload handler to forward `drive_import` WebSocket broadcasts
    /// so the web UI can show the user progress instead of a stale
    /// spinner during a large restore.
    pub fn import_json_file_with_progress<F: Fn(usize)>(
        &self,
        path: &str,
        on_progress: F,
    ) -> Result<crate::json_compat::ImportStats> {
        // Legacy path: hard-wire experimental=false (JsonCompatCodec).
        self.import_json_file_with_progress_codec(
            path,
            on_progress,
            crate::store_codec::select_codec(false).as_ref(),
        )
    }

    /// Codec-aware variant of [`import_json_file_with_progress`].
    /// `experimental` selects the codec; `false` is the legacy
    /// `JsonCompatCodec` and behaves identically to the non-`_with` method.
    pub fn import_json_file_with_progress_with<F: Fn(usize)>(
        &self,
        path: &str,
        on_progress: F,
        experimental: bool,
    ) -> Result<crate::json_compat::ImportStats> {
        self.import_json_file_with_progress_codec(
            path,
            on_progress,
            crate::store_codec::select_codec(experimental).as_ref(),
        )
    }

    /// Lowest-level import: drive the import through an explicit
    /// [`StoreCodec`]. Locks the shared connection, persists import history,
    /// refreshes counts — identical bookkeeping regardless of codec.
    pub fn import_json_file_with_progress_codec<F: Fn(usize)>(
        &self,
        path: &str,
        on_progress: F,
        codec: &dyn crate::store_codec::StoreCodec,
    ) -> Result<crate::json_compat::ImportStats> {
        let existing_before = self.route_count.load(Ordering::Relaxed);
        let (stats, diag) = {
            let mut conn = self.conn.lock().unwrap();
            let s = codec.import(&mut conn, path, &on_progress)?;
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
            // Persist the diagnostics record while we still hold the writer
            // lock. Best-effort — a failure here is logged but not fatal,
            // since the import itself already committed.
            if let Err(e) = persist_import_history(&conn, &s.0, &s.1) {
                warn!("import_json_file_with_progress: failed to persist import history: {}", e);
            }
            s
        };
        self.drive_cache_dirty.store(true, Ordering::Release);
        self.refresh_counts()?;
        let after = self.route_count.load(Ordering::Relaxed);
        info!(
            "import_json_file: existing_before={} stats_routes={} after={} (delta={})",
            existing_before,
            stats.routes,
            after,
            after - existing_before
        );
        if diag.has_problems() {
            warn!(
                "import_json_file: diagnostics flagged problems — see import_json warnings above; \
                 query GET /api/drives/data/import-history for the persisted record"
            );
        }
        Ok(stats)
    }

    /// Read the ring-buffered import history (last [`IMPORT_HISTORY_MAX`]
    /// entries). Each entry contains the `ImportStats` and `ImportDiagnostics`
    /// captured at import time, plus a Unix epoch timestamp. Used by
    /// `GET /api/drives/data/import-history` so operators can see why drives
    /// went missing without scraping logs.
    pub fn import_history(&self) -> Result<Vec<ImportHistoryEntry>> {
        let conn = self.conn.lock().unwrap();
        read_import_history_inner(&conn)
    }

    /// Export current DB contents as `drive-data.json` at `path`.
    /// Atomic via tmp + rename.
    ///
    /// Opens a fresh read-only connection rather than locking the shared
    /// writer connection, so the 3+ minute mirror regeneration on a
    /// well-used Pi doesn't block `/api/drives`, `/api/drives/routes`,
    /// or any other DB-touching endpoint. WAL mode (set in
    /// `apply_pragmas`) lets this reader stream a consistent snapshot
    /// concurrently with writes from the main connection. Falls back
    /// to the in-memory `:memory:` path by reusing the shared
    /// connection, since you can't open a second handle to an
    /// in-memory DB.
    pub fn export_json_to_file(&self, path: &str) -> Result<()> {
        // Legacy path: hard-wire experimental=false (JsonCompatCodec).
        self.export_json_to_file_with(path, false)
    }

    /// Codec-aware variant of [`export_json_to_file`]. `experimental`
    /// selects the codec; `false` is the legacy `JsonCompatCodec` and
    /// produces byte-for-byte identical output to the zero-arg method.
    /// All the read-only-handle / atomic-rename machinery is shared.
    pub fn export_json_to_file_with(&self, path: &str, experimental: bool) -> Result<()> {
        self.export_json_to_file_codec(path, crate::store_codec::select_codec(experimental).as_ref())
    }

    /// Lowest-level export: write through an explicit [`StoreCodec`].
    /// Used by the `_with` overload; exposed so a caller that already holds
    /// a codec can avoid re-selecting one.
    pub fn export_json_to_file_codec(
        &self,
        path: &str,
        codec: &dyn crate::store_codec::StoreCodec,
    ) -> Result<()> {
        if let Some(dir) = Path::new(path).parent() {
            if !dir.as_os_str().is_empty() && dir != Path::new("/") {
                std::fs::create_dir_all(dir)?;
            }
        }
        let tmp = format!("{}.tmp", path);
        if self.path == ":memory:" {
            let conn = self.conn.lock().unwrap();
            write_export_json(&conn, &tmp, codec)?;
        } else {
            let conn = open_readonly_connection(&self.path)
                .with_context(|| format!("export_json_to_file: open read-only {}", self.path))?;
            write_export_json(&conn, &tmp, codec)?;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }

    /// Regenerate the JSON mirror and copy it to `/mnt/archive/drive-data.json`
    /// with the size-guard applied. No-op if `/mnt/archive` is not a
    /// mounted filesystem.
    pub fn sync_to_archive(&self) -> Result<()> {
        if !Path::new("/mnt/archive").exists() {
            return Ok(());
        }
        if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
            if !mounts.contains("/mnt/archive") {
                return Ok(());
            }
        }

        self.export_json_for_sync()?;
        sync_to_path(
            DEFAULT_JSON_MIRROR_PATH,
            ARCHIVE_DATA_PATH,
            syncguard::DEFAULT_CACHE_PATH,
        )
    }

    /// Copy `/mnt/archive/drive-data.json` to `/backingfiles/drive-data.json`
    /// so the next `Load()` picks it up via the one-shot importer.
    /// Useful after reflashing a Pi that still has an archive backup.
    pub fn restore_from_archive(&self) -> Result<()> {
        if !Path::new(ARCHIVE_DATA_PATH).exists() {
            return Ok(());
        }
        // Don't restore if we already have local data — the importer
        // would skip it anyway, and we'd rather not churn disk.
        if Path::new(DEFAULT_JSON_MIRROR_PATH).exists() {
            return Ok(());
        }
        let src = std::fs::read(ARCHIVE_DATA_PATH).unwrap_or_default();
        if let Some(dir) = Path::new(DEFAULT_JSON_MIRROR_PATH).parent() {
            if !dir.as_os_str().is_empty() && dir != Path::new("/") {
                std::fs::create_dir_all(dir)?;
            }
        }
        std::fs::write(DEFAULT_JSON_MIRROR_PATH, &src)?;
        info!(
            "[drives] Restored drive-data.json from archive ({} bytes); next Load() will import it",
            src.len()
        );
        Ok(())
    }

    /// Return the pre-computed drives list as a JSON string. On a cache hit
    /// (the common case after startup) this is a single-row meta-table
    /// lookup — no grouper work, no BLOB decoding, no sorter allocation.
    ///
    /// On a cache miss (first request after startup or after routes/tags
    /// change), builds the cache from route summaries + tags and stores it
    /// in the `meta` table for subsequent requests.
    pub fn get_cached_drives_json(&self) -> Result<String> {
        let conn = self.conn.lock().unwrap();

        if !self.drive_cache_dirty.load(Ordering::Acquire) {
            if let Some(json) = schema::meta_get(&conn, "drive_list_cache")? {
                if !json.is_empty() {
                    return Ok(json);
                }
            }
        }

        rebuild_drive_list_cache(&conn)?;
        self.drive_cache_dirty.store(false, Ordering::Release);
        Ok(schema::meta_get(&conn, "drive_list_cache")?.unwrap_or_else(|| "[]".to_string()))
    }

    /// Return the pre-computed drive stats as a JSON string. `processed_count`
    /// is stored as 0 in the cache; callers must inject the live value.
    pub fn get_cached_drive_stats_json(&self) -> Result<String> {
        let conn = self.conn.lock().unwrap();
        if !self.drive_cache_dirty.load(Ordering::Acquire) {
            if let Some(json) = schema::meta_get(&conn, "drive_stats_cache")? {
                if !json.is_empty() {
                    return Ok(json);
                }
            }
        }
        rebuild_drive_list_cache(&conn)?;
        self.drive_cache_dirty.store(false, Ordering::Release);
        Ok(schema::meta_get(&conn, "drive_stats_cache")?.unwrap_or_else(|| "{}".to_string()))
    }

    /// Return the pre-computed FSD analytics as a JSON string.
    pub fn get_cached_fsd_analytics_json(&self) -> Result<String> {
        let conn = self.conn.lock().unwrap();
        if !self.drive_cache_dirty.load(Ordering::Acquire) {
            if let Some(json) = schema::meta_get(&conn, "fsd_analytics_cache")? {
                // Treat "{}" as a cache miss: older builds could persist an
                // empty-object placeholder which then masks real data forever.
                if !json.is_empty() && json.trim() != "{}" {
                    return Ok(json);
                }
            }
        }
        rebuild_drive_list_cache(&conn)?;
        self.drive_cache_dirty.store(false, Ordering::Release);
        Ok(schema::meta_get(&conn, "fsd_analytics_cache")?.unwrap_or_else(|| "{}".to_string()))
    }

    /// Refresh the cached row counts. Called after every mutation.
    fn refresh_counts(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let rc: i64 = conn.query_row("SELECT COUNT(*) FROM routes", [], |r| r.get(0))?;
        let pc: i64 =
            conn.query_row("SELECT COUNT(*) FROM processed_files", [], |r| r.get(0))?;
        self.route_count.store(rc, Ordering::Relaxed);
        self.processed_count.store(pc, Ordering::Relaxed);
        Ok(())
    }
}

// -----------------------------------------------------------------------------
// SQL helpers (private)
// -----------------------------------------------------------------------------

fn open_connection(path: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    apply_pragmas(&conn)?;
    Ok(conn)
}

/// Open a second SQLite handle on the same DB file, in read-only mode,
/// for long-running reads (the JSON export mirror) that would otherwise
/// hold the shared writer connection's mutex for minutes. WAL mode on
/// the writer lets this handle see a consistent snapshot.
fn open_readonly_connection(path: &str) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags)?;
    conn.execute_batch(
        "PRAGMA query_only = ON;
         PRAGMA busy_timeout = 5000;",
    )?;
    Ok(conn)
}

fn apply_pragmas(conn: &Connection) -> Result<()> {
    // mmap_size = 64 MB: SQLite mmaps the DB file up to this size, which
    // eliminates the pager-buffer copy on BLOB-heavy reads (e.g.
    // select_all_route_summaries scanning thousands of routes). 64 MB
    // fits comfortably in 32-bit ARMv7's ~3 GB user-space VA, so it's
    // safe across every SBC the project supports.
    //
    // cache_size = -8000: 8 MB page cache (negative value = KB). Default
    // is 2 MB which is too small to keep the rebuild_drive_list_cache
    // working set hot on a populated DB. Bumped to 8 MB; still trivial
    // relative to Pi RAM budgets.
    //
    // temp_store = MEMORY: keep ORDER BY / GROUP BY temp tables in RAM
    // instead of /tmp. On read-only-root Pi setups /tmp is tmpfs anyway,
    // so this is equivalent in effect, but it's explicit and safe.
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA mmap_size = 67108864;
         PRAGMA cache_size = -8000;
         PRAGMA temp_store = MEMORY;",
    )?;
    Ok(())
}

fn write_export_json(
    conn: &Connection,
    tmp_path: &str,
    codec: &dyn crate::store_codec::StoreCodec,
) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(tmp_path)?;
    codec.export(conn, &mut f).context("export_json")?;
    f.flush()?;
    f.sync_all()?;
    Ok(())
}

/// Maximum number of import-history records kept in the `meta` table.
const IMPORT_HISTORY_MAX: usize = 20;

/// Wire-format record for one entry in the persisted import history.
/// Stored as a JSON array under `meta` key `import_history` (newest last).
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ImportHistoryEntry {
    /// Unix epoch seconds when the import completed.
    pub timestamp: i64,
    pub stats: crate::json_compat::ImportStats,
    pub diagnostics: crate::json_compat::ImportDiagnostics,
}

/// Append a single import's stats + diagnostics to the ring-buffered
/// `import_history` JSON array in the `meta` table. Keeps the most recent
/// `IMPORT_HISTORY_MAX` entries; older ones are dropped from the front.
fn persist_import_history(
    conn: &Connection,
    stats: &crate::json_compat::ImportStats,
    diag: &crate::json_compat::ImportDiagnostics,
) -> Result<()> {
    let entry = ImportHistoryEntry {
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        stats: *stats,
        diagnostics: diag.clone(),
    };
    let mut history: Vec<ImportHistoryEntry> = match schema::meta_get(conn, "import_history")? {
        Some(s) if !s.is_empty() => serde_json::from_str(&s).unwrap_or_default(),
        _ => Vec::new(),
    };
    history.push(entry);
    if history.len() > IMPORT_HISTORY_MAX {
        let drop = history.len() - IMPORT_HISTORY_MAX;
        history.drain(0..drop);
    }
    let json = serde_json::to_string(&history)
        .map_err(|e| anyhow::anyhow!("serialize import_history: {}", e))?;
    schema::meta_set(conn, "import_history", &json)?;
    Ok(())
}

/// Read the persisted import history from the `meta` table. Returns an
/// empty Vec if no imports have been recorded.
fn read_import_history_inner(conn: &Connection) -> Result<Vec<ImportHistoryEntry>> {
    match schema::meta_get(conn, "import_history")? {
        Some(s) if !s.is_empty() => serde_json::from_str(&s)
            .map_err(|e| anyhow::anyhow!("parse import_history: {}", e)),
        _ => Ok(Vec::new()),
    }
}

/// Build the grouped drive list and store it as JSON in the `meta` table,
/// along with the route count and tag row count used to validate the cache
/// on the next startup.
fn rebuild_drive_list_cache(conn: &Connection) -> Result<()> {
    // Use the BLOB-free summary grouper so this cache and the
    // `single_drive` endpoint (which also resolves drive IDs through
    // the summary grouper) agree on drive count, boundaries, and IDs.
    // The previous BLOB-grouper cache could split a clip mid-park-gap
    // while the summary grouper kept the whole clip in one drive,
    // producing different drive lists for /api/drives vs
    // /api/drives/{id} and causing clicked drives to load wrong points.
    //
    // Heap win: ~5 MB instead of ~300 MB on a 5500-route DB (no BLOB
    // decode here). Numerical drift on noisy GPS is fractions of a
    // percent, invisible after the UI's 0.1-mi / whole-percent rounding.
    let summaries = select_all_route_summaries(conn)?;
    let route_count = summaries.len() as i64;

    let mut tags = std::collections::HashMap::<String, Vec<String>>::new();
    let mut tags_count: i64 = 0;
    {
        let mut stmt = conn.prepare_cached("SELECT drive_key, tag FROM drive_tags")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for r in rows {
            let (k, t) = r?;
            tags.entry(k).or_default().push(t);
            tags_count += 1;
        }
    }

    // Group with original (un-hidden) IDs first — these are what
    // `find_drive_files` looks up against, so the cached list must
    // hold the same IDs even after the Tessie-overlap filter strips
    // duplicates.
    let drives = crate::grouper::group_summaries_fast(&summaries, &tags);
    let visible = crate::grouper::hide_tessie_overlapping_sei(drives.clone());
    info!(
        "drive cache: route_count={} drives={} visible={} tags={}",
        route_count,
        drives.len(),
        visible.len(),
        tags_count
    );
    let json = serde_json::to_string(&visible)
        .map_err(|e| anyhow::anyhow!("drive cache serialize: {}", e))?;
    // MAX(updated_at) lets the validity check detect in-place route updates
    // (same row count, different aggregates — e.g. archiveloop reprocess,
    // or a grouper change shipped via OTA without bumping
    // DRIVE_LIST_CACHE_ALGO_VERSION). Without this marker, the cache stays
    // "valid" while the live grouper would produce a different drive list.
    let max_updated_at: i64 = conn
        .query_row("SELECT COALESCE(MAX(updated_at), 0) FROM routes", [], |r| r.get(0))
        .unwrap_or(0);
    schema::meta_set(conn, "drive_list_cache", &json)?;
    schema::meta_set(conn, "drive_list_cache_route_count", &route_count.to_string())?;
    schema::meta_set(conn, "drive_list_cache_tags_count", &tags_count.to_string())?;
    schema::meta_set(conn, "drive_list_cache_algo", DRIVE_LIST_CACHE_ALGO_VERSION)?;
    schema::meta_set(conn, "drive_list_cache_max_updated_at", &max_updated_at.to_string())?;

    // Cache drive stats from grouped drives so `/api/drives` and
    // `/api/drives/stats` are consistent on drive count and mileage.
    // Totals include all drives; FSD-specific analytics are SEI-only.
    let r = |v: f64| -> f64 { (v * 100.0).round() / 100.0 };
    let drives_count = drives.len() as i64;
    let total_distance_km: f64 = drives.iter().map(|d| d.distance_km).sum();
    let total_distance_mi: f64 = drives.iter().map(|d| d.distance_mi).sum();
    let total_duration_ms: i64 = drives.iter().map(|d| d.duration_ms).sum();

    let sei_drives: Vec<_> = drives
        .iter()
        .filter(|d| d.source.as_deref() != Some("tessie"))
        .collect();
    let sei_total_km: f64 = sei_drives.iter().map(|d| d.distance_km).sum();
    let fsd_distance_km: f64 = sei_drives.iter().map(|d| d.fsd_distance_km).sum();
    let fsd_distance_mi: f64 = sei_drives.iter().map(|d| d.fsd_distance_mi).sum();
    let autosteer_distance_km: f64 = sei_drives.iter().map(|d| d.autosteer_distance_km).sum();
    let autosteer_distance_mi: f64 = sei_drives.iter().map(|d| d.autosteer_distance_mi).sum();
    let tacc_distance_km: f64 = sei_drives.iter().map(|d| d.tacc_distance_km).sum();
    let tacc_distance_mi: f64 = sei_drives.iter().map(|d| d.tacc_distance_mi).sum();
    let fsd_engaged_ms: i64 = sei_drives.iter().map(|d| d.fsd_engaged_ms).sum();
    let autosteer_engaged_ms: i64 = sei_drives.iter().map(|d| d.autosteer_engaged_ms).sum();
    let tacc_engaged_ms: i64 = sei_drives.iter().map(|d| d.tacc_engaged_ms).sum();
    let fsd_disengagements: i32 = sei_drives.iter().map(|d| d.fsd_disengagements).sum();
    let fsd_accel_pushes: i32 = sei_drives.iter().map(|d| d.fsd_accel_pushes).sum();
    let fsd_percent = if sei_total_km > 0.0 {
        (fsd_distance_km / sei_total_km * 100.0 * 10.0).round() / 10.0
    } else {
        0.0
    };
    let assisted_percent = if sei_total_km > 0.0 {
        ((fsd_distance_km + autosteer_distance_km + tacc_distance_km) / sei_total_km * 100.0 * 10.0)
            .round()
            / 10.0
    } else {
        0.0
    };
    let stats_json = serde_json::to_string(&serde_json::json!({
        "drives_count":          drives_count,
        "routes_count":          route_count,
        "processed_count":       0,
        "total_distance_km":     r(total_distance_km),
        "total_distance_mi":     r(total_distance_mi),
        "total_duration_ms":     total_duration_ms,
        "fsd_engaged_ms":        fsd_engaged_ms,
        "fsd_distance_km":       r(fsd_distance_km),
        "fsd_distance_mi":       r(fsd_distance_mi),
        "fsd_percent":           fsd_percent,
        "fsd_disengagements":    fsd_disengagements,
        "fsd_accel_pushes":      fsd_accel_pushes,
        "autosteer_engaged_ms":  autosteer_engaged_ms,
        "autosteer_distance_km": r(autosteer_distance_km),
        "autosteer_distance_mi": r(autosteer_distance_mi),
        "tacc_engaged_ms":       tacc_engaged_ms,
        "tacc_distance_km":      r(tacc_distance_km),
        "tacc_distance_mi":      r(tacc_distance_mi),
        "assisted_percent":      assisted_percent,
    })).map_err(|e| anyhow::anyhow!("stats cache serialize: {}", e))?;
    schema::meta_set(conn, "drive_stats_cache", &stats_json)?;

    // Cache FSD analytics — reuses the already-grouped drives list.
    let fsd = crate::grouper::fsd_analytics_from_drives(&drives);
    let fsd_json = serde_json::to_string(&fsd)
        .map_err(|e| anyhow::anyhow!("fsd analytics cache serialize: {}", e))?;
    schema::meta_set(conn, "fsd_analytics_cache", &fsd_json)?;

    info!(
        "[drives] Drive list cache rebuilt ({} drives, {} visible after Tessie/SEI hide, from {} routes)",
        drives.len(),
        visible.len(),
        route_count
    );
    Ok(())
}

/// True when the persisted drive list cache matches the current DB contents.
/// Compares route count and drive_tags row count stored at cache-build time
/// against live COUNT(*) values. Two cheap queries per startup skip the
/// expensive grouper run on restarts where nothing changed.
fn is_drive_cache_valid(conn: &Connection) -> Result<bool> {
    let cache = schema::meta_get(conn, "drive_list_cache")?;
    if cache.map_or(true, |s| s.is_empty()) {
        return Ok(false);
    }

    let stored_rc = schema::meta_get(conn, "drive_list_cache_route_count")?
        .and_then(|s| s.parse::<i64>().ok());
    let current_rc: i64 =
        conn.query_row("SELECT COUNT(*) FROM routes", [], |r| r.get(0))?;
    if stored_rc != Some(current_rc) {
        return Ok(false);
    }

    let stored_tc = schema::meta_get(conn, "drive_list_cache_tags_count")?
        .and_then(|s| s.parse::<i64>().ok());
    let current_tc: i64 =
        conn.query_row("SELECT COUNT(*) FROM drive_tags", [], |r| r.get(0))?;
    if stored_tc != Some(current_tc) {
        return Ok(false);
    }

    let algo = schema::meta_get(conn, "drive_list_cache_algo")?;
    if algo.as_deref() != Some(DRIVE_LIST_CACHE_ALGO_VERSION) {
        return Ok(false);
    }

    // Detect in-place route updates (same row count, changed aggregates).
    // Without this, an archiveloop reprocess pass — or an OTA where the
    // grouper changes but DRIVE_LIST_CACHE_ALGO_VERSION isn't bumped —
    // leaves the cache serving stale drive boundaries while the live
    // grouper would produce a different list. Field-reproduced 2026-05-19
    // on v2.7.5: cache held 213 drives from a prior state; fresh grouper
    // computed 144; `/api/drives/{id}` 404'd for IDs 144-212 because they
    // existed in the cache but not in the live grouping.
    //
    // Treat a missing stored value as invalid so caches written by
    // pre-fix builds get rebuilt on first read after the upgrade.
    let stored_max_ua = schema::meta_get(conn, "drive_list_cache_max_updated_at")?
        .and_then(|s| s.parse::<i64>().ok());
    let current_max_ua: i64 = conn
        .query_row("SELECT COALESCE(MAX(updated_at), 0) FROM routes", [], |r| r.get(0))
        .unwrap_or(-1);
    if stored_max_ua != Some(current_max_ua) {
        return Ok(false);
    }

    Ok(true)
}

/// Insert-or-update a single route row with all v2 aggregate columns.
/// Caller is inside a transaction.
fn insert_or_update_route(
    tx: &rusqlite::Transaction,
    norm_file: &str,
    r: &Route,
    a: &RouteAggregates,
    now: i64,
) -> Result<()> {
    let pb = encode_points(Some(&r.points));
    let gb = encode_u8s(Some(&r.gear_states));
    let ab = encode_u8s(Some(&r.autopilot_states));
    let sb = encode_f32s(Some(&r.speeds));
    let acb = encode_f32s(Some(&r.accel_positions));
    let rb = encode_gear_runs(Some(&r.gear_runs));

    let first_lat: Option<f64> = r.points.first().map(|p| p[0]);
    let first_lon: Option<f64> = r.points.first().map(|p| p[1]);

    let point_count = r.points.len() as i64;

    // `params![]` builds a stack-allocated `[&dyn ToSql; N]`, replacing
    // the prior `Vec<Box<dyn ToSql>>` + `Vec<&dyn ToSql>` pattern which
    // heap-allocated 35 small boxes plus two Vecs per insert. Called once
    // per ingested clip (50+/min during Tesla recording).
    tx.execute(
        "INSERT INTO routes(
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
            ?33, ?34, ?35)
         ON CONFLICT(file) DO UPDATE SET
            date_dir            = excluded.date_dir,
            point_count         = excluded.point_count,
            raw_park_count      = excluded.raw_park_count,
            raw_frame_count     = excluded.raw_frame_count,
            distance_m          = excluded.distance_m,
            first_lat           = excluded.first_lat,
            first_lon           = excluded.first_lon,
            points_blob         = excluded.points_blob,
            gear_states_blob    = excluded.gear_states_blob,
            ap_states_blob      = excluded.ap_states_blob,
            speeds_blob         = excluded.speeds_blob,
            accel_blob          = excluded.accel_blob,
            gear_runs_blob      = excluded.gear_runs_blob,
            updated_at          = excluded.updated_at,
            max_speed_mps       = excluded.max_speed_mps,
            avg_speed_mps       = excluded.avg_speed_mps,
            speed_sample_count  = excluded.speed_sample_count,
            valid_point_count   = excluded.valid_point_count,
            fsd_engaged_ms      = excluded.fsd_engaged_ms,
            autosteer_engaged_ms= excluded.autosteer_engaged_ms,
            tacc_engaged_ms     = excluded.tacc_engaged_ms,
            fsd_distance_m      = excluded.fsd_distance_m,
            autosteer_distance_m= excluded.autosteer_distance_m,
            tacc_distance_m     = excluded.tacc_distance_m,
            assisted_distance_m = excluded.assisted_distance_m,
            fsd_disengagements  = excluded.fsd_disengagements,
            fsd_accel_pushes    = excluded.fsd_accel_pushes,
            start_lat           = excluded.start_lat,
            start_lon           = excluded.start_lon,
            end_lat             = excluded.end_lat,
            end_lon             = excluded.end_lon,
            source              = excluded.source,
            external_signature  = excluded.external_signature,
            tessie_autopilot_percent = excluded.tessie_autopilot_percent",
        params![
            norm_file,
            &r.date,
            point_count,
            r.raw_park_count as i64,
            r.raw_frame_count as i64,
            a.distance_m,
            first_lat,
            first_lon,
            pb,
            gb,
            ab,
            sb,
            acb,
            rb,
            now,
            a.max_speed_mps,
            a.avg_speed_mps,
            a.speed_sample_count,
            a.valid_point_count,
            a.fsd_engaged_ms,
            a.autosteer_engaged_ms,
            a.tacc_engaged_ms,
            a.fsd_distance_m,
            a.autosteer_distance_m,
            a.tacc_distance_m,
            a.assisted_distance_m,
            a.fsd_disengagements,
            a.fsd_accel_pushes,
            a.start_lat,
            a.start_lng,
            a.end_lat,
            a.end_lng,
            &r.source,
            &r.external_signature,
            r.tessie_autopilot_percent,
        ],
    )?;
    Ok(())
}

/// Select all routes into `Vec<Route>` — fully decoded BLOB columns.
fn select_all_routes(conn: &Connection) -> Result<Vec<Route>> {
    let mut stmt = conn.prepare_cached(
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
    )?;
    let rows = stmt.query_map([], route_row_mapper)?;

    let mut out = Vec::new();
    for r in rows {
        out.push(build_route_from_row(r?)?);
    }
    Ok(out)
}

/// Select full routes for a specific set of files. Uses an IN (...) clause
/// bound with positional parameters so the query planner can still use the
/// `file` primary-key index. Falls back to empty when `files` is empty
/// (SQLite disallows `IN ()`).
fn select_routes_by_files(conn: &Connection, files: &[&str]) -> Result<Vec<Route>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = std::iter::repeat("?").take(files.len()).collect::<Vec<_>>().join(",");
    let sql = format!(
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
         WHERE file IN ({})
         ORDER BY file",
        placeholders
    );
    // The normalized path is what's stored in the routes table; callers
    // pass already-normalized strings (from RouteSummary.file, which came
    // out of the same column).
    let normalized: Vec<String> = files.iter().map(|f| normalize_path(f)).collect();
    let params: Vec<&dyn ToSql> = normalized.iter().map(|s| s as &dyn ToSql).collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params.as_slice(), route_row_mapper)?;

    let mut out = Vec::with_capacity(files.len());
    for r in rows {
        out.push(build_route_from_row(r?)?);
    }
    Ok(out)
}

/// Tuple shape that [`route_row_mapper`] returns. Carries raw column
/// values straight from the row; [`build_route_from_row`] decodes the
/// blob columns and assembles the `Route`. Kept as a free-standing
/// tuple to avoid a one-off struct used only by these two selects.
type RouteRow = (
    String, String, u32, u32,
    Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>,
    Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>,
    Option<String>, Option<String>, Option<f64>,
    // BLE telemetry rollup — populated by
    // `aggregate_telemetry::write_route_telemetry`. NULL on rows whose
    // clip-window had no telemetry samples, or pre-v6 rows.
    Option<f64>, Option<f64>,
    Option<f64>, Option<f64>, Option<f64>,
    Option<i64>,
    Option<f64>, Option<f64>, Option<f64>, Option<f64>,
    Option<f64>, Option<f64>,
    Option<String>, Option<String>,
);

/// Shared row mapper for the two route SELECTs above. The column order
/// in both SQL strings must match this exactly.
fn route_row_mapper(row: &rusqlite::Row<'_>) -> rusqlite::Result<RouteRow> {
    Ok((
        row.get::<_, String>(0)?,
        row.get::<_, String>(1)?,
        row.get::<_, i64>(2)? as u32,
        row.get::<_, i64>(3)? as u32,
        row.get::<_, Option<Vec<u8>>>(4)?,
        row.get::<_, Option<Vec<u8>>>(5)?,
        row.get::<_, Option<Vec<u8>>>(6)?,
        row.get::<_, Option<Vec<u8>>>(7)?,
        row.get::<_, Option<Vec<u8>>>(8)?,
        row.get::<_, Option<Vec<u8>>>(9)?,
        row.get::<_, Option<String>>(10)?,
        row.get::<_, Option<String>>(11)?,
        row.get::<_, Option<f64>>(12)?,
        row.get::<_, Option<f64>>(13)?,
        row.get::<_, Option<f64>>(14)?,
        row.get::<_, Option<f64>>(15)?,
        row.get::<_, Option<f64>>(16)?,
        row.get::<_, Option<f64>>(17)?,
        row.get::<_, Option<i64>>(18)?,
        row.get::<_, Option<f64>>(19)?,
        row.get::<_, Option<f64>>(20)?,
        row.get::<_, Option<f64>>(21)?,
        row.get::<_, Option<f64>>(22)?,
        row.get::<_, Option<f64>>(23)?,
        row.get::<_, Option<f64>>(24)?,
        row.get::<_, Option<String>>(25)?,
        row.get::<_, Option<String>>(26)?,
    ))
}

/// Decode blob columns + assemble a `Route` from a [`RouteRow`] tuple.
fn build_route_from_row(r: RouteRow) -> Result<Route> {
    let (
        file, date, raw_park_count, raw_frame_count,
        pb, gb, ab, sb, acb, rb,
        source, external_signature, tessie_autopilot_percent,
        battery_pct_start, battery_pct_end,
        interior_temp_min, interior_temp_max, exterior_temp_avg,
        hvac_runtime_s,
        tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi,
        odometer_mi_start, odometer_mi_end,
        location_name_start, location_name_end,
    ) = r;
    let points = decode_points(pb.as_deref())
        .with_context(|| format!("decode points {}", file))?
        .unwrap_or_default();
    let gear_states = decode_u8s(gb.as_deref()).unwrap_or_default();
    let autopilot_states = decode_u8s(ab.as_deref()).unwrap_or_default();
    let speeds = decode_f32s(sb.as_deref())
        .with_context(|| format!("decode speeds {}", file))?
        .unwrap_or_default();
    let accel_positions = decode_f32s(acb.as_deref())
        .with_context(|| format!("decode accel {}", file))?
        .unwrap_or_default();
    let gear_runs = decode_gear_runs(rb.as_deref())
        .with_context(|| format!("decode gear_runs {}", file))?
        .unwrap_or_default();
    Ok(Route {
        file, date, points, gear_states, autopilot_states,
        speeds, accel_positions, raw_park_count, raw_frame_count, gear_runs,
        source, external_signature, tessie_autopilot_percent,
        battery_pct_start, battery_pct_end,
        interior_temp_min, interior_temp_max, exterior_temp_avg,
        hvac_runtime_s,
        tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi,
        odometer_mi_start, odometer_mi_end,
        location_name_start, location_name_end,
    })
}

/// Select BLOB-free summary rows — metadata + v2 aggregate columns +
/// v6 telemetry rollups. The telemetry columns may be NULL on pre-v6
/// rows or routes whose 60s window had no samples; the consumer
/// handles that via the `Option` shape inside `RouteTelemetryAggregates`.
fn select_all_route_summaries(conn: &Connection) -> Result<Vec<RouteSummary>> {
    let mut stmt = conn.prepare_cached(
        "SELECT file, date_dir, raw_park_count, raw_frame_count, gear_runs_blob,
                distance_m, max_speed_mps, avg_speed_mps, speed_sample_count,
                valid_point_count, fsd_engaged_ms, autosteer_engaged_ms,
                tacc_engaged_ms, fsd_distance_m, autosteer_distance_m,
                tacc_distance_m, assisted_distance_m,
                fsd_disengagements, fsd_accel_pushes,
                start_lat, start_lon, end_lat, end_lon,
                source, external_signature,
                battery_pct_start, battery_pct_end,
                interior_temp_min, interior_temp_max, exterior_temp_avg,
                hvac_runtime_s,
                tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi,
                odometer_mi_start, odometer_mi_end,
                location_name_start, location_name_end
         FROM routes
         ORDER BY file",
    )?;
    let rows = stmt.query_map([], |row| {
        let rb: Option<Vec<u8>> = row.get(4)?;
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)? as u32,
            row.get::<_, i64>(3)? as u32,
            rb,
            row.get::<_, Option<f64>>(5)?,
            row.get::<_, Option<f64>>(6)?,
            row.get::<_, Option<f64>>(7)?,
            row.get::<_, Option<i64>>(8)?,
            row.get::<_, Option<i64>>(9)?,
            row.get::<_, Option<i64>>(10)?,
            row.get::<_, Option<i64>>(11)?,
            row.get::<_, Option<i64>>(12)?,
            row.get::<_, Option<f64>>(13)?,
            row.get::<_, Option<f64>>(14)?,
            row.get::<_, Option<f64>>(15)?,
            row.get::<_, Option<f64>>(16)?,
            row.get::<_, Option<i64>>(17)?,
            row.get::<_, Option<i64>>(18)?,
            row.get::<_, Option<f64>>(19)?,
            row.get::<_, Option<f64>>(20)?,
            row.get::<_, Option<f64>>(21)?,
            row.get::<_, Option<f64>>(22)?,
            row.get::<_, Option<String>>(23)?,
            row.get::<_, Option<String>>(24)?,
            // v6 telemetry columns (battery_temp_avg intentionally
            // not fetched — Tesla doesn't expose battery cell temp)
            row.get::<_, Option<f64>>(25)?,
            row.get::<_, Option<f64>>(26)?,
            row.get::<_, Option<f64>>(27)?,
            row.get::<_, Option<f64>>(28)?,
            row.get::<_, Option<f64>>(29)?,
            row.get::<_, Option<i64>>(30)?,
            // v7 TPMS columns
            (
                row.get::<_, Option<f64>>(31)?,
                row.get::<_, Option<f64>>(32)?,
                row.get::<_, Option<f64>>(33)?,
                row.get::<_, Option<f64>>(34)?,
            ),
            // v9 odometer. software_version intentionally not
            // fetched — Tesla doesn't expose car_version over BLE.
            (
                row.get::<_, Option<f64>>(35)?,
                row.get::<_, Option<f64>>(36)?,
            ),
            // v10 location names (start / end)
            (
                row.get::<_, Option<String>>(37)?,
                row.get::<_, Option<String>>(38)?,
            ),
        ))
    })?;

    let mut out = Vec::new();
    for r in rows {
        let (
            file,
            date,
            raw_park_count,
            raw_frame_count,
            rb,
            distance_m,
            max_speed_mps,
            avg_speed_mps,
            speed_sample_count,
            valid_point_count,
            fsd_engaged_ms,
            autosteer_engaged_ms,
            tacc_engaged_ms,
            fsd_distance_m,
            autosteer_distance_m,
            tacc_distance_m,
            assisted_distance_m,
            fsd_disengagements,
            fsd_accel_pushes,
            start_lat,
            start_lon,
            end_lat,
            end_lon,
            source,
            external_signature,
            battery_pct_start,
            battery_pct_end,
            interior_temp_min,
            interior_temp_max,
            exterior_temp_avg,
            hvac_runtime_s,
            (tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi),
            (odometer_mi_start, odometer_mi_end),
            (location_name_start, location_name_end),
        ) = r?;

        let gear_runs = decode_gear_runs(rb.as_deref())
            .with_context(|| format!("decode gear_runs {}", file))?
            .unwrap_or_default();

        out.push(RouteSummary {
            file,
            date,
            raw_park_count,
            raw_frame_count,
            gear_runs,
            aggregates: RouteAggregates {
                distance_m: distance_m.unwrap_or(0.0),
                max_speed_mps: max_speed_mps.unwrap_or(0.0),
                avg_speed_mps: avg_speed_mps.unwrap_or(0.0),
                speed_sample_count: speed_sample_count.unwrap_or(0),
                valid_point_count: valid_point_count.unwrap_or(0),
                fsd_engaged_ms: fsd_engaged_ms.unwrap_or(0),
                autosteer_engaged_ms: autosteer_engaged_ms.unwrap_or(0),
                tacc_engaged_ms: tacc_engaged_ms.unwrap_or(0),
                fsd_distance_m: fsd_distance_m.unwrap_or(0.0),
                autosteer_distance_m: autosteer_distance_m.unwrap_or(0.0),
                tacc_distance_m: tacc_distance_m.unwrap_or(0.0),
                assisted_distance_m: assisted_distance_m.unwrap_or(0.0),
                fsd_disengagements: fsd_disengagements.unwrap_or(0) as i32,
                fsd_accel_pushes: fsd_accel_pushes.unwrap_or(0) as i32,
                start_lat,
                start_lng: start_lon,
                end_lat,
                end_lng: end_lon,
            },
            source,
            external_signature,
            telemetry: crate::types::RouteTelemetryAggregates {
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
            },
        });
    }
    Ok(out)
}

// -----------------------------------------------------------------------------
// Import dance + path helpers
// -----------------------------------------------------------------------------

fn run_one_shot_import(conn: &mut Connection, candidates: &[&str]) -> Result<()> {
    if let Some(v) = meta_get(conn, "imported_from_json_at")? {
        if !v.is_empty() {
            return Ok(()); // already imported
        }
    }

    let mut source: Option<&str> = None;
    let mut also_present: Vec<&str> = Vec::new();
    for p in candidates {
        if let Ok(info) = std::fs::metadata(p) {
            if !info.is_dir() {
                if source.is_none() {
                    source = Some(p);
                } else {
                    also_present.push(p);
                }
            }
        }
    }
    if !also_present.is_empty() {
        warn!(
            "[drives] Multiple drive-data.json candidates exist; importing {} and ignoring {:?}. Delete the unused file(s) to silence this warning.",
            source.unwrap(),
            also_present
        );
    }

    let Some(source_path) = source else {
        // True fresh install — mark so we don't keep checking.
        info!("[drives] No legacy drive-data.json found; treating as fresh install");
        meta_set(conn, "imported_from_json_at", &chrono::Utc::now().to_rfc3339())?;
        return Ok(());
    };

    info!("[drives] Importing legacy JSON from {}", source_path);
    let (stats, diag) =
        crate::json_compat::import_json(conn, source_path, |routes_imported| {
            info!("[drives] Import progress: {} routes", routes_imported);
        })
        .with_context(|| format!("import_json {}", source_path))?;
    info!(
        "[drives] Import complete: {} routes, {} processed files, {} tags",
        stats.routes, stats.processed_files, stats.drive_tags
    );
    if let Err(e) = persist_import_history(conn, &stats, &diag) {
        warn!("[drives] failed to persist import history: {}", e);
    }

    // Set the marker BEFORE renaming. If we die between these two steps,
    // the worst outcome on next boot is an orphan JSON left alone (the
    // marker is set → no double-import).
    meta_set(conn, "imported_from_json_at", &chrono::Utc::now().to_rfc3339())?;

    let bak_path = {
        let ts = chrono::Utc::now().timestamp();
        format!("{}.bak-{}-{:04x}", source_path, ts, rand_suffix4())
    };
    if let Err(e) = rename_or_copy(source_path, &bak_path) {
        warn!(
            "[drives] Import succeeded but failed to archive {} -> {}: {}",
            source_path, bak_path, e
        );
    } else {
        info!(
            "[drives] Renamed source JSON to {} (backup; safe to delete after verifying drives page)",
            bak_path
        );
    }
    Ok(())
}

fn rand_suffix4() -> u16 {
    // Simple xorshift on a nanosecond clock — not security-sensitive.
    let mut t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    t ^= t >> 13;
    t ^= t << 7;
    t ^= t >> 17;
    (t & 0xffff) as u16
}

fn rename_or_copy(src: &str, dst: &str) -> Result<()> {
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    // Cross-filesystem fallback.
    let data = std::fs::read(src)?;
    std::fs::write(dst, &data)?;
    // Best-effort fsync the destination so a crash doesn't lose data.
    if let Ok(f) = std::fs::File::open(dst) {
        let _ = f.sync_all();
    }
    std::fs::remove_file(src)?;
    Ok(())
}

/// Atomic sync of `src` → `dst` with size-guard gated by `cache_path`.
fn sync_to_path(src: &str, dst: &str, cache_path: &str) -> Result<()> {
    let src_meta = std::fs::metadata(src)?;
    let new_size = src_meta.len() as i64;

    let last_size = read_sync_cache(cache_path);
    if let Err(e) = check_sync_size_guard(new_size, last_size) {
        warn!("[drives] {}", e);
        return Err(e.into());
    }

    if let Some(dir) = Path::new(dst).parent() {
        if !dir.as_os_str().is_empty() && dir != Path::new("/") {
            std::fs::create_dir_all(dir)?;
        }
    }
    let tmp = format!("{}.tmp", dst);
    let copied = std::fs::copy(src, &tmp)?;

    // Short-copy detection: if src shrank between stat and copy, do NOT
    // poison the size-guard cache with the truncated length.
    if copied as i64 != new_size {
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!(
            "sync_to_path: short copy ({} of {} bytes); refusing to poison size-guard cache",
            copied,
            new_size
        );
    }

    if let Err(e) = rename_or_copy(&tmp, dst) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    if let Err(e) = write_sync_cache(cache_path, new_size) {
        warn!(
            "[drives] Failed to update sync-size cache at {}: {}",
            cache_path, e
        );
    }
    info!("[drives] Synced drive data to archive ({} bytes)", new_size);
    Ok(())
}

/// Convert backslashes to forward slashes so Windows-shaped paths
/// collide with their POSIX equivalents in `processed_files` and
/// `routes`.
pub fn normalize_path(p: &str) -> String {
    p.replace('\\', "/")
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_route_is_bit_identical() {
        let store = DriveStore::open_memory().unwrap();
        let pts: Vec<GpsPoint> = vec![[37.7749, -122.4194], [37.7750, -122.4195]];
        store
            .add_route(
                "2025-01-15/clip-front.mp4",
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
        let routes = store.get_routes().unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].points, pts);
        assert_eq!(routes[0].gear_states, vec![4, 4]);
        assert_eq!(routes[0].autopilot_states, vec![1, 1]);
        assert_eq!(routes[0].speeds, vec![25.0, 26.0]);
        assert_eq!(routes[0].accel_positions, vec![0.5, 0.6]);
        assert_eq!(routes[0].raw_frame_count, 2);
        assert_eq!(routes[0].gear_runs.len(), 1);
    }

    #[test]
    fn route_summary_reads_precomputed_aggregates() {
        let store = DriveStore::open_memory().unwrap();
        let pts: Vec<GpsPoint> = vec![[37.7749, -122.4194], [37.7750, -122.4194]];
        store
            .add_route(
                "a.mp4", "2025-01-01", &pts, &[], &[], &[], &[], 0, 2, &[],
            )
            .unwrap();
        let out = store.with_route_summaries(|s| s.to_vec()).unwrap();
        assert_eq!(out.len(), 1);
        // Distance should be > 0 since AddRoute populated the aggregate
        // column from the BLOB via ComputeRouteAggregates.
        assert!(out[0].aggregates.distance_m > 0.0);
    }

    #[test]
    fn path_normalization_dedups_windows_and_posix() {
        let store = DriveStore::open_memory().unwrap();
        store.mark_processed("2025\\01\\clip.mp4").unwrap();
        store.mark_processed("2025/01/clip.mp4").unwrap();
        assert_eq!(store.processed_count(), 1);
        assert!(store.is_processed("2025\\01\\clip.mp4").unwrap());
        assert!(store.is_processed("2025/01/clip.mp4").unwrap());
    }

    #[test]
    fn tags_set_and_get() {
        let store = DriveStore::open_memory().unwrap();
        store
            .set_drive_tags(
                "drive1",
                &["Work".to_string(), "Commute".to_string()],
            )
            .unwrap();
        let tags = store.get_drive_tags("drive1").unwrap();
        assert_eq!(tags, vec!["Commute".to_string(), "Work".to_string()]);
    }

    /// End-to-end contract for the `drive_tags` join key.
    ///
    /// Regression test for the v2.9.x "Add tag" bug: the PUT handler used
    /// to pass the raw URL id (typically the numeric DriveSummary.id, e.g.
    /// `"0"`) straight into `set_drive_tags`, but the grouper joins tags
    /// onto drives using the `%Y-%m-%dT%H:%M:%S` start_time string. The
    /// row was written but never matched on read, so the tag silently
    /// vanished from the UI even though the request returned 200.
    ///
    /// This test:
    ///   1. Pins down the contract: rows keyed by start_time DO surface.
    ///   2. Pins down the failure mode: rows keyed by the numeric idx do
    ///      NOT surface — so future refactors of the handler can't
    ///      regress this without breaking the test.
    ///   3. Exercises `find_drive_start_time` as the bridge the handler
    ///      uses to translate numeric ids into the canonical key.
    #[test]
    fn drive_tags_join_on_start_time_string_not_numeric_id() {
        use crate::grouper;
        let store = DriveStore::open_memory().unwrap();
        let pts: Vec<GpsPoint> = vec![[37.7749, -122.4194], [37.7750, -122.4195]];
        store
            .add_route(
                "2025-01-15_12-30-45-front.mp4",
                "2025-01-15",
                &pts,
                &[4, 4],
                &[0, 0],
                &[10.0, 10.0],
                &[0.0, 0.0],
                0,
                2,
                &[GearRun { gear: 4, frames: 2 }],
            )
            .unwrap();

        // Sanity: grouper sees one drive whose start_time is the
        // parsed-from-filename `%Y-%m-%dT%H:%M:%S` form.
        let (drive_id, drive_start_time) = store
            .with_route_summaries(|s| {
                let drives = grouper::group_summaries_fast(s, &std::collections::HashMap::new());
                (drives[0].id, drives[0].start_time.clone())
            })
            .unwrap();
        assert_eq!(drive_id, 0);
        assert_eq!(drive_start_time, "2025-01-15T12:30:45");

        // The resolver must translate the numeric URL id into the
        // start_time the grouper joins on.
        let resolved = store
            .with_route_summaries(|s| grouper::find_drive_start_time(s, "0"))
            .unwrap();
        assert_eq!(resolved.as_deref(), Some("2025-01-15T12:30:45"));
        // And accept the start_time form too (single_drive does).
        let resolved_st = store
            .with_route_summaries(|s| grouper::find_drive_start_time(s, "2025-01-15T12:30:45"))
            .unwrap();
        assert_eq!(resolved_st.as_deref(), Some("2025-01-15T12:30:45"));
        // Bogus id resolves to None — handler returns 404.
        let bogus = store
            .with_route_summaries(|s| grouper::find_drive_start_time(s, "999"))
            .unwrap();
        assert!(bogus.is_none());

        // Negative control: storing under the raw numeric id (the old
        // broken path) MUST NOT surface the tag on the drive.
        store
            .set_drive_tags("0", &["BugCanary".to_string()])
            .unwrap();
        let tags_after_bad = store.get_all_drive_tags().unwrap();
        let drive_after_bad = store
            .with_route_summaries(|s| {
                grouper::group_summaries_fast(s, &tags_after_bad)[0]
                    .tags
                    .clone()
            })
            .unwrap();
        assert!(
            drive_after_bad.is_empty(),
            "numeric-keyed tag rows must not surface — got {:?}",
            drive_after_bad,
        );

        // Positive: storing under the resolved start_time key DOES surface.
        store
            .set_drive_tags(&resolved.unwrap(), &["Work".to_string()])
            .unwrap();
        let tags_after_good = store.get_all_drive_tags().unwrap();
        let drive_after_good = store
            .with_route_summaries(|s| {
                grouper::group_summaries_fast(s, &tags_after_good)[0]
                    .tags
                    .clone()
            })
            .unwrap();
        assert_eq!(drive_after_good, vec!["Work".to_string()]);
    }

    #[test]
    fn replace_data_wipes_and_reinserts() {
        let store = DriveStore::open_memory().unwrap();
        store.mark_processed("old.mp4").unwrap();
        assert_eq!(store.processed_count(), 1);

        let data = StoreData {
            processed_files: vec!["new.mp4".to_string()],
            routes: vec![],
            drive_tags: std::collections::HashMap::new(),
        };
        store.replace_data(&data).unwrap();
        assert_eq!(store.processed_count(), 1);
        assert!(!store.is_processed("old.mp4").unwrap());
        assert!(store.is_processed("new.mp4").unwrap());
    }
}
