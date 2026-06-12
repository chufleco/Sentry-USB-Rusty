//! Diagnostic: import a drive-data.json export into a fresh store and
//! print the drive stats the real grouper/aggregate pipeline computes.
//!
//! Usage: cargo run -p sentryusb-drives --release --example import_stats -- <drive-data.json> [db-path]

use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let json = args
        .next()
        .expect("usage: import_stats <drive-data.json> [db-path]");
    let db = args.next().unwrap_or_else(|| {
        std::env::temp_dir()
            .join("import-stats-test.db")
            .to_string_lossy()
            .into_owned()
    });
    // Pass "-" as the JSON path to reuse an existing DB without
    // re-importing (the open()/load() migrations still run).
    let store = if json == "-" {
        sentryusb_drives::DriveStore::open(&db)?
    } else {
        let _ = std::fs::remove_file(&db);
        let _ = std::fs::remove_file(format!("{}-wal", db));
        let _ = std::fs::remove_file(format!("{}-shm", db));
        let store = sentryusb_drives::DriveStore::open(&db)?;
        let t = Instant::now();
        let stats = store.import_json_file(&json)?;
        eprintln!("imported {:?} in {:?}", stats, t.elapsed());
        store
    };

    let t = Instant::now();
    let s = store.get_cached_drive_stats_json()?;
    eprintln!("stats computed in {:?}", t.elapsed());
    println!("{}", s);

    // All-time FSD analytics — the Self-Driving-Analytics panel numbers.
    let analytics = store.with_route_summaries(|s| {
        sentryusb_drives::grouper::fsd_analytics_from_summaries_for_period(s, "all")
    })?;
    eprintln!(
        "fsd analytics (all): fsd%={} grade={} time={} avg_dis={} avg_pushes={} dis={} pushes={} fsd_mi={:.1} total_mi={:.1}",
        analytics.fsd_percent,
        analytics.fsd_grade,
        analytics.fsd_time_formatted,
        analytics.avg_disengagements_per_drive,
        analytics.avg_accel_pushes_per_drive,
        analytics.disengagements,
        analytics.accel_pushes,
        analytics.fsd_distance_mi,
        analytics.total_distance_mi,
    );

    // Drive-list breakdown by source, from the same cache the UI reads.
    let drives_json = store.get_cached_drives_json()?;
    if let Some(out) = args.next() {
        std::fs::write(&out, &drives_json)?;
        eprintln!("drives json written to {}", out);
    }
    let drives: serde_json::Value = serde_json::from_str(&drives_json)?;
    if let Some(arr) = drives.as_array() {
        let mut by_src: std::collections::BTreeMap<String, (usize, f64)> =
            std::collections::BTreeMap::new();
        for d in arr {
            let src = d
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("sei")
                .to_string();
            let mi = d.get("distanceMi").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let e = by_src.entry(src).or_insert((0, 0.0));
            e.0 += 1;
            e.1 += mi;
        }
        for (src, (n, mi)) in by_src {
            eprintln!("visible drives: source={} count={} mi={:.1}", src, n, mi);
        }
    }
    Ok(())
}
