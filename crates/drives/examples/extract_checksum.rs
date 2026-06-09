//! Equivalence/perf harness for extract_gps_from_file.
//!
//! Walks a directory of .mp4 clips, extracts GPS from each, and prints a
//! deterministic per-file summary plus a rolling checksum over every
//! extracted value. Used to prove a refactor of the extractor is
//! byte-identical against real clips, and to time it.
//!
//! Usage: extract_checksum <dir> [max_files]

use std::path::Path;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: extract_checksum <dir> [max]");
    let max: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);

    let mut files: Vec<String> = Vec::new();
    collect(Path::new(&dir), &mut files);
    files.sort();
    files.truncate(max);

    // FNV-1a over a canonical byte encoding of every extracted field.
    let mut h: u64 = 0xcbf29ce484222325;
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    };

    let start = std::time::Instant::now();
    let mut total_points = 0usize;
    let mut ok_files = 0usize;
    for f in &files {
        match sentryusb_drives::extract::extract_gps_from_file(f) {
            Ok(g) => {
                ok_files += 1;
                total_points += g.points.len();
                feed(&(g.points.len() as u32).to_le_bytes());
                feed(&g.raw_frame_count.to_le_bytes());
                feed(&g.raw_park_count.to_le_bytes());
                for p in &g.points {
                    feed(&p[0].to_le_bytes());
                    feed(&p[1].to_le_bytes());
                }
                feed(&g.gear_states);
                feed(&g.autopilot_states);
                for s in &g.speeds { feed(&s.to_le_bytes()); }
                for a in &g.accel_positions { feed(&a.to_le_bytes()); }
                for r in &g.gear_runs {
                    feed(&[r.gear]);
                    feed(&r.frames.to_le_bytes());
                }
            }
            Err(_) => feed(b"ERR"),
        }
    }
    let elapsed = start.elapsed();
    println!(
        "files={} ok={} total_points={} checksum={:016x} elapsed_ms={}",
        files.len(),
        ok_files,
        total_points,
        h,
        elapsed.as_millis()
    );
}

fn collect(dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("mp4") {
            // Resolve symlinks so we read the real snapshot file.
            if let Ok(real) = std::fs::canonicalize(&p) {
                out.push(real.to_string_lossy().into_owned());
            }
        }
    }
}
