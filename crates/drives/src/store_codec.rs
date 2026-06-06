//! Pluggable drive-data.json codec.
//!
//! Two implementations sit behind one trait:
//!
//! * [`JsonCompatCodec`] — the legacy path. Literal one-line delegations to
//!   the battle-tested [`crate::json_compat`] export/import. This is what a
//!   normal install runs: the Go-base64 `drive-data.json` shape, unchanged.
//!
//! * [`TypedCodec`] — the clean typed path. Its export is driven by
//!   [`crate::row::RouteRow`] (one place defines the column order) instead of
//!   an inline column list, but it serializes the SAME [`Route`] values
//!   through the SAME `go_byte_slice` wire encoding and the SAME
//!   `{processedFiles, routes, driveTags}` envelope. The only thing that
//!   differs from the legacy path is the SELECT/decode plumbing — so the two
//!   codecs MUST emit byte-identical output for identical DB state. That
//!   equality is the load-bearing safety gate (see `store_codec_tests` and
//!   the drives-crate byte-equality test), and it's what makes flipping
//!   `SENTRYUSB_EXPERIMENTAL` a no-op on the wire.
//!
//! [`select_codec`] picks the implementation from the injected `experimental`
//! bool. The bool is passed *in* from the api crate (which reads the config
//! flag) — this crate never reads the flag itself, keeping `drives`
//! independent of `config`.
//!
//! ## Streaming
//! `TypedCodec::export` does NOT buffer the route set. It walks a prepared
//! statement row-by-row, decoding and serializing one `Route` at a time and
//! dropping it before the next is fetched — exactly the bound-memory loop the
//! legacy `RouteStream` uses. This is what keeps the export viable on a
//! 512 MB Pi Zero with a multi-hundred-MB store. The trait method takes a
//! `&mut dyn Write` sink, so the bytes flow straight to the file/socket.

use std::io::Write;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::json_compat::{ImportDiagnostics, ImportStats};
use crate::row::RouteRow;

/// Export/import strategy for `drive-data.json`.
///
/// Object-safe: every method takes `&self`, concrete `Connection` /
/// `&mut dyn Write` / path args, and returns owned values — no generic type
/// parameters — so `Box<dyn StoreCodec>` works. (The streaming `Write` sink
/// is erased to `&mut dyn Write` for exactly this reason; callers that have a
/// concrete writer just pass `&mut writer`.)
pub trait StoreCodec {
    /// Stream the DB contents to `writer` as `drive-data.json`. Bound memory:
    /// one decoded `Route` at a time, never the whole set.
    fn export(&self, conn: &Connection, writer: &mut dyn Write) -> Result<()>;

    /// Import a `drive-data.json` file at `path` into `conn`.
    fn import(
        &self,
        conn: &mut Connection,
        path: &str,
        on_progress: &dyn Fn(usize),
    ) -> Result<(ImportStats, ImportDiagnostics)>;
}

/// Legacy codec — verbatim delegation to [`crate::json_compat`]. This is the
/// flag-off path and the definition of "correct bytes".
pub struct JsonCompatCodec;

impl StoreCodec for JsonCompatCodec {
    fn export(&self, conn: &Connection, writer: &mut dyn Write) -> Result<()> {
        crate::json_compat::export_json(conn, writer)
    }

    fn import(
        &self,
        conn: &mut Connection,
        path: &str,
        on_progress: &dyn Fn(usize),
    ) -> Result<(ImportStats, ImportDiagnostics)> {
        crate::json_compat::import_json(conn, path, on_progress)
    }
}

/// Typed codec — RouteRow-driven SELECT/decode, byte-identical wire output.
///
/// Import currently delegates to the legacy streaming importer: the import
/// side already parses the canonical Go envelope one Route at a time and
/// writes through the shared insert path, so there is no clean-DB win to be
/// had by re-plumbing it, and reusing it guarantees the round-trip stays
/// identical. The typed work that matters here is the export SELECT going
/// through the single `RouteRow::COLUMNS` definition.
pub struct TypedCodec;

impl StoreCodec for TypedCodec {
    fn export(&self, conn: &Connection, writer: &mut dyn Write) -> Result<()> {
        export_typed(conn, writer)
    }

    fn import(
        &self,
        conn: &mut Connection,
        path: &str,
        on_progress: &dyn Fn(usize),
    ) -> Result<(ImportStats, ImportDiagnostics)> {
        // Reuse the legacy streaming importer verbatim — see the struct doc.
        crate::json_compat::import_json(conn, path, on_progress)
    }
}

/// Pick a codec from the injected experimental flag. `true` => [`TypedCodec`],
/// `false` => [`JsonCompatCodec`] (legacy, the byte-for-byte current path).
pub fn select_codec(experimental: bool) -> Box<dyn StoreCodec> {
    if experimental {
        Box::new(TypedCodec)
    } else {
        Box::new(JsonCompatCodec)
    }
}

/// The typed export. Mirrors `json_compat::export_json` exactly — same
/// envelope, same ordering, same per-`Route` serialization — but the routes
/// stream is driven by [`RouteRow`] so the column list lives in one place.
fn export_typed<W: Write + ?Sized>(conn: &Connection, writer: &mut W) -> Result<()> {
    // processedFiles: same query + same belt-and-suspenders sort as legacy.
    let mut processed_files = {
        let mut stmt = conn.prepare("SELECT file FROM processed_files ORDER BY file")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        out
    };
    processed_files.sort();

    // driveTags: BTreeMap so keys serialize in sorted order, matching legacy.
    let drive_tags = {
        let mut stmt =
            conn.prepare("SELECT drive_key, tag FROM drive_tags ORDER BY drive_key, tag")?;
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

    // Envelope shape MUST match json_compat::export_json's OrderedStoreData
    // field-for-field (names, order, skip-if-empty) so serde emits the same
    // bytes. Only `routes` differs: a TypedRouteStream instead of RouteStream.
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct OrderedStoreData<'a> {
        processed_files: &'a [String],
        routes: TypedRouteStream<'a>,
        #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        drive_tags: &'a std::collections::BTreeMap<String, Vec<String>>,
    }

    // Out-parameter for the real rusqlite error behind serde's generic
    // "io error" wrapper — same trick the legacy exporter uses.
    let route_err: std::cell::RefCell<Option<anyhow::Error>> = std::cell::RefCell::new(None);

    let out = OrderedStoreData {
        processed_files: &processed_files,
        routes: TypedRouteStream {
            conn,
            error: &route_err,
        },
        drive_tags: &drive_tags,
    };
    let ser_result = serde_json::to_writer_pretty(writer, &out);

    if let Some(e) = route_err.into_inner() {
        return Err(e.context("export_typed: streaming route read failed"));
    }
    ser_result.context("serialize JSON")?;
    Ok(())
}

/// Serializer adapter that streams `routes` from SQLite through [`RouteRow`]
/// one row at a time. Holds at most one decoded `Route` in memory — the
/// `RouteRow` and the `Route` it decodes into both drop at the end of each
/// loop iteration, before the next row is fetched.
struct TypedRouteStream<'a> {
    conn: &'a Connection,
    error: &'a std::cell::RefCell<Option<anyhow::Error>>,
}

impl<'a> serde::Serialize for TypedRouteStream<'a> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{Error as SerError, SerializeSeq};

        let mut stmt = self.conn.prepare(&RouteRow::select_sql()).map_err(|e| {
            *self.error.borrow_mut() = Some(anyhow::Error::from(e));
            S::Error::custom("routes prepare failed")
        })?;

        let mut rows = stmt.query([]).map_err(|e| {
            *self.error.borrow_mut() = Some(anyhow::Error::from(e));
            S::Error::custom("routes query failed")
        })?;

        // `serialize_seq(None)` with serde_json's pretty serializer produces
        // exactly the same array framing the legacy RouteStream does.
        let mut seq = serializer.serialize_seq(None)?;

        loop {
            let row_opt = rows.next().map_err(|e| {
                *self.error.borrow_mut() = Some(anyhow::Error::from(e));
                S::Error::custom("routes row fetch failed")
            })?;
            let Some(row) = row_opt else { break };

            let route_row = RouteRow::from_row(row).map_err(|e| {
                *self.error.borrow_mut() = Some(anyhow::Error::from(e));
                S::Error::custom("routes row decode failed")
            })?;
            // Decode BLOBs into a Route; serialize it through the identical
            // Route Serialize impl (go_byte_slice, skip_serializing_if, …)
            // the legacy exporter uses — that's what makes the bytes match.
            let route = route_row.to_route().map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("route decode failed")
            })?;
            seq.serialize_element(&route)?;
            // `route` and `route_row` drop here before the next fetch.
        }
        seq.end()
    }
}

#[cfg(test)]
mod store_codec_tests {
    use super::*;
    use crate::db::DriveStore;
    use crate::types::{GearRun, GpsPoint, StoreData};

    /// Seed a multi-row store with non-empty BLOBs and some NULL aggregate
    /// columns, returning the open store.
    fn seed_store() -> DriveStore {
        let store = DriveStore::open_memory().unwrap();
        // Route 1: full payload.
        let p1: Vec<GpsPoint> = vec![[37.7749, -122.4194], [37.7750, -122.4195]];
        store
            .add_route(
                "2025-01-15_10-00-00/clip-front.mp4",
                "2025-01-15",
                &p1,
                &[4, 4],
                &[1, 0],
                &[25.0, 26.0],
                &[0.5, 0.6],
                0,
                2,
                &[GearRun { gear: 4, frames: 2 }],
            )
            .unwrap();
        // Route 2: different gears/speeds.
        let p2: Vec<GpsPoint> = vec![[40.0, -74.0], [40.1, -74.1], [40.2, -74.2]];
        store
            .add_route(
                "2025-02-20_08-30-00/clip-front.mp4",
                "2025-02-20",
                &p2,
                &[4, 3, 4],
                &[0, 0, 2],
                &[10.0, 11.0, 12.0],
                &[0.0, 0.1, 0.2],
                1,
                3,
                &[GearRun { gear: 4, frames: 3 }],
            )
            .unwrap();
        // Route 3: another shape — all of these leave the v6+ telemetry
        // columns (battery, temps, TPMS, odometer) NULL since add_route
        // never populates them, so the byte-equality test exercises rows
        // with NULL aggregate/telemetry columns by construction.
        let p3: Vec<GpsPoint> = vec![[51.5, -0.12], [51.6, -0.13]];
        store
            .add_route(
                "2025-03-10_18-45-00/clip-front.mp4",
                "2025-03-10",
                &p3,
                &[4, 4],
                &[1, 1],
                &[30.0, 31.0],
                &[0.3, 0.4],
                0,
                2,
                &[GearRun { gear: 4, frames: 2 }],
            )
            .unwrap();
        store.mark_processed("2025-01-15_10-00-00/clip-front.mp4").unwrap();
        store
            .set_drive_tags("2025-01-15T10:00:00", &["Commute".into(), "Work".into()])
            .unwrap();
        store
    }

    /// Populate v6+ telemetry/aggregate columns on route 1. Used by the
    /// byte-equality gate (not the round-trip test): it makes BOTH export
    /// paths serialize non-NULL battery/temp/TPMS/odometer/location values
    /// so the gate can catch a field dropped from one export path but not
    /// the other. The round-trip test can't use this — the legacy *import*
    /// path doesn't yet persist v6+ columns (typed import is a later slice),
    /// so a round-trip through it would legitimately drop them.
    fn populate_v6_columns(store: &DriveStore) {
        store
            .with_locked_conn(|conn| {
                conn.execute(
                    "UPDATE routes SET \
                       battery_pct_start = 80.0, battery_pct_end = 64.0, \
                       interior_temp_min = 18.5, interior_temp_max = 22.0, \
                       tire_fl_psi = 41.0, tire_fr_psi = 41.5, \
                       odometer_mi_start = 12453.5, odometer_mi_end = 12461.2, \
                       location_name_start = 'Start St', location_name_end = 'End Ave' \
                     WHERE file = ?1",
                    rusqlite::params!["2025-01-15_10-00-00/clip-front.mp4"],
                )
            })
            .unwrap();
    }

    /// THE load-bearing gate: TypedCodec and JsonCompatCodec must produce
    /// byte-identical `drive-data.json` for the same DB state. If this fails,
    /// TypedCodec is wrong — not the test.
    #[test]
    fn typed_and_jsoncompat_export_are_byte_identical() {
        let store = seed_store();
        populate_v6_columns(&store);
        store
            .with_locked_conn(|conn| {
                let mut typed_buf = Vec::new();
                let mut legacy_buf = Vec::new();
                select_codec(true).export(conn, &mut typed_buf).unwrap();
                select_codec(false).export(conn, &mut legacy_buf).unwrap();
                assert_eq!(
                    typed_buf, legacy_buf,
                    "TypedCodec export must be byte-identical to JsonCompatCodec",
                );
                // Sanity: it's real, non-empty JSON with all three routes.
                let parsed: StoreData = serde_json::from_slice(&typed_buf).unwrap();
                assert_eq!(parsed.routes.len(), 3);
                // The v6+ telemetry columns we populated on route 1 must
                // actually appear on the wire — proves the gate exercised a
                // non-NULL aggregate column through both codecs, not just NULLs.
                let r1 = parsed
                    .routes
                    .iter()
                    .find(|r| r.file == "2025-01-15_10-00-00/clip-front.mp4")
                    .expect("route 1 present");
                assert_eq!(r1.battery_pct_start, Some(80.0));
                assert_eq!(r1.location_name_start.as_deref(), Some("Start St"));
            });
    }

    /// Cross-codec round-trip: importing the TypedCodec export must yield the
    /// same store contents, and re-exporting must again byte-match.
    #[test]
    fn typed_export_import_roundtrips() {
        let store = seed_store();
        let export_bytes = store
            .with_locked_conn(|conn| {
                let mut buf = Vec::new();
                select_codec(true).export(conn, &mut buf).unwrap();
                buf
            });

        // Write to a temp file and import into a fresh migrated connection
        // via TypedCodec, then re-export and compare bytes.
        let path = std::env::temp_dir().join(format!(
            "sentryusb-typed-roundtrip-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, &export_bytes).unwrap();

        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::schema::migrate(&conn).unwrap();
        select_codec(true)
            .import(&mut conn, path.to_str().unwrap(), &|_| {})
            .unwrap();
        let mut typed_again = Vec::new();
        select_codec(true).export(&conn, &mut typed_again).unwrap();
        let mut legacy_again = Vec::new();
        select_codec(false).export(&conn, &mut legacy_again).unwrap();

        assert_eq!(
            typed_again, legacy_again,
            "after import, TypedCodec and JsonCompatCodec exports must match",
        );
        // And the re-export equals the original export (lossless round-trip).
        assert_eq!(
            typed_again, export_bytes,
            "import(export) must round-trip to identical bytes",
        );

        let _ = std::fs::remove_file(&path);
    }
}
