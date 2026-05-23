//! Status, storage, config, and WiFi API handlers.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Serialize;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::router::AppState;

// ---------------------------------------------------------------------------
// Network throughput sampler
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct NetSample {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub taken_at: Instant,
}

pub type NetSampler = Arc<Mutex<HashMap<String, NetSample>>>;

// ---------------------------------------------------------------------------
// GET /api/status
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct PiStatus {
    cpu_temp: String,
    num_snapshots: String,
    snapshot_oldest: String,
    snapshot_newest: String,
    total_space: String,
    free_space: String,
    uptime: String,
    drives_active: String,
    wifi_ssid: String,
    wifi_freq: String,
    wifi_strength: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    wifi_signal_dbm: Option<i32>,
    wifi_ip: String,
    ether_ip: String,
    ether_speed: String,
    device_suffix: String,
    sbc_model: String,
    fan_speed: String,
    wifi_rx_bps: u64,
    wifi_tx_bps: u64,
    ether_rx_bps: u64,
    ether_tx_bps: u64,
}

pub async fn get_status(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut s = PiStatus {
        cpu_temp: String::new(),
        num_snapshots: "0".into(),
        snapshot_oldest: String::new(),
        snapshot_newest: String::new(),
        total_space: String::new(),
        free_space: String::new(),
        uptime: String::new(),
        drives_active: "no".into(),
        wifi_ssid: String::new(),
        wifi_freq: String::new(),
        wifi_strength: String::new(),
        wifi_signal_dbm: None,
        wifi_ip: String::new(),
        ether_ip: String::new(),
        ether_speed: String::new(),
        device_suffix: String::new(),
        sbc_model: String::new(),
        fan_speed: String::new(),
        wifi_rx_bps: 0,
        wifi_tx_bps: 0,
        ether_rx_bps: 0,
        ether_tx_bps: 0,
    };

    // Device suffix from machine-id
    if let Ok(mid) = std::fs::read_to_string("/etc/machine-id") {
        let mid = mid.trim();
        if mid.len() >= 4 {
            s.device_suffix = mid[mid.len() - 4..].to_uppercase();
        }
    }

    // SBC model
    s.sbc_model = get_sbc_model();

    // CPU temperature
    if let Ok(data) = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") {
        s.cpu_temp = data.trim().to_string();
    }

    // Fan speed (Raspberry Pi cooling fan RPM from hwmon device)
    s.fan_speed = read_fan_speed();

    // Uptime
    if let Ok(data) = std::fs::read_to_string("/proc/uptime") {
        if let Some(secs) = data.split_whitespace().next() {
            s.uptime = secs.to_string();
        }
    }

    // USB gadget status: report active only when UDC is bound AND lun.0 has a
    // backing file. A bare directory-exists check reports "yes" through a
    // partial teardown where the car has already lost the device — that drove
    // a UI bug where the dashboard stayed green after a failed toggle.
    if sentryusb_gadget::is_active() {
        s.drives_active = "yes".into();
    }

    // Snapshots
    let snapshots = find_snapshots();
    s.num_snapshots = snapshots.len().to_string();
    if !snapshots.is_empty() {
        if let Ok(meta) = std::fs::metadata(&snapshots[0]) {
            if let Ok(t) = meta.modified() {
                if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
                    s.snapshot_oldest = d.as_secs().to_string();
                }
            }
        }
        if let Ok(meta) = std::fs::metadata(snapshots.last().unwrap()) {
            if let Ok(t) = meta.modified() {
                if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
                    s.snapshot_newest = d.as_secs().to_string();
                }
            }
        }
    }

    // Disk space
    if let Ok(out) = sentryusb_shell::run("stat", &["--file-system", "--format=%b %S %f", "/backingfiles/."]).await {
        let parts: Vec<&str> = out.trim().split_whitespace().collect();
        if parts.len() >= 3 {
            if let (Ok(blocks), Ok(bs), Ok(free)) = (
                parts[0].parse::<u64>(),
                parts[1].parse::<u64>(),
                parts[2].parse::<u64>(),
            ) {
                s.total_space = (blocks * bs).to_string();
                s.free_space = (free * bs).to_string();
            }
        }
    }

    // WiFi info — skip shell queries when interface is down (saves 5-10s
    // on ethernet-only systems where wlan0 exists but is unconfigured).
    // The four shell-outs run concurrently — they're independent reads,
    // so serializing them is wasted wallclock on a slow Pi.
    let wifi_dev = find_net_device("wl*");
    if !wifi_dev.is_empty() && iface_is_up(&wifi_dev) {
        // Bind arg arrays to lets so the temporaries outlive the join!.
        let ssid_args = ["-r", wifi_dev.as_str()];
        let freq_args = ["-r", "-f", wifi_dev.as_str()];
        let iwc_args = [wifi_dev.as_str()];
        let ip_args = ["-4", "addr", "show", wifi_dev.as_str()];
        let (ssid_r, freq_r, iwc_r, ip_r) = tokio::join!(
            sentryusb_shell::run("iwgetid", &ssid_args),
            sentryusb_shell::run("iwgetid", &freq_args),
            sentryusb_shell::run("iwconfig", &iwc_args),
            sentryusb_shell::run("ip", &ip_args),
        );
        if let Ok(out) = ssid_r {
            s.wifi_ssid = out.trim().to_string();
        }
        if let Ok(out) = freq_r {
            s.wifi_freq = out.trim().to_string();
        }
        if let Ok(out) = iwc_r {
            for line in out.lines() {
                if let Some(after) = line.split("Link Quality=").nth(1) {
                    if let Some(qual) = after.split_whitespace().next() {
                        s.wifi_strength = qual.to_string();
                    }
                }
                // Same line typically contains "Signal level=-48 dBm". Parse
                // only the integer portion so we don't smuggle units through
                // the JSON and the frontend can format consistently.
                if let Some(after) = line.split("Signal level=").nth(1) {
                    if let Some(tok) = after.split_whitespace().next() {
                        if let Ok(dbm) = tok.parse::<i32>() {
                            s.wifi_signal_dbm = Some(dbm);
                        }
                    }
                }
            }
        }
        if let Ok(out) = ip_r {
            for line in out.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("inet ") {
                    if let Some(addr) = trimmed.split_whitespace().nth(1) {
                        s.wifi_ip = addr.split('/').next().unwrap_or("").to_string();
                    }
                }
            }
        }
        let (rx, tx) = compute_throughput(&state.net_sampler, &wifi_dev);
        s.wifi_rx_bps = rx;
        s.wifi_tx_bps = tx;
    }

    // Ethernet info — same operstate guard
    let mut eth_dev = find_net_device("eth*");
    if eth_dev.is_empty() {
        eth_dev = find_net_device("en*");
    }
    if !eth_dev.is_empty() && iface_is_up(&eth_dev) {
        // Same parallelization win as the WiFi block: independent reads.
        let eth_ip_args = ["-4", "addr", "show", eth_dev.as_str()];
        let eth_tool_args = [eth_dev.as_str()];
        let (ip_r, ethtool_r) = tokio::join!(
            sentryusb_shell::run("ip", &eth_ip_args),
            sentryusb_shell::run("ethtool", &eth_tool_args),
        );
        if let Ok(out) = ip_r {
            for line in out.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("inet ") {
                    if let Some(addr) = trimmed.split_whitespace().nth(1) {
                        s.ether_ip = addr.split('/').next().unwrap_or("").to_string();
                    }
                }
            }
        }
        if let Ok(out) = ethtool_r {
            for line in out.lines() {
                if line.contains("Speed:") {
                    if let Some(val) = line.split(':').nth(1) {
                        s.ether_speed = val.trim().to_string();
                    }
                }
            }
        }
        let (rx, tx) = compute_throughput(&state.net_sampler, &eth_dev);
        s.ether_rx_bps = rx;
        s.ether_tx_bps = tx;
    }

    (StatusCode::OK, Json(serde_json::to_value(s).unwrap_or_default()))
}

// ---------------------------------------------------------------------------
// GET /api/status/storage
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct StorageBreakdown {
    cam_size: i64,
    music_size: i64,
    lightshow_size: i64,
    boombox_size: i64,
    snapshots_size: i64,
    total_space: i64,
    free_space: i64,
}

pub async fn get_storage_breakdown(
    State(_state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut sb = StorageBreakdown {
        cam_size: disk_usage("/backingfiles/cam_disk.bin"),
        music_size: disk_usage("/backingfiles/music_disk.bin"),
        lightshow_size: disk_usage("/backingfiles/lightshow_disk.bin"),
        boombox_size: disk_usage("/backingfiles/boombox_disk.bin"),
        snapshots_size: 0,
        total_space: 0,
        free_space: 0,
    };

    if let Ok(out) = sentryusb_shell::run("stat", &["--file-system", "--format=%b %S %f", "/backingfiles/."]).await {
        let parts: Vec<&str> = out.trim().split_whitespace().collect();
        if parts.len() >= 3 {
            if let (Ok(blocks), Ok(bs), Ok(free)) = (
                parts[0].parse::<i64>(),
                parts[1].parse::<i64>(),
                parts[2].parse::<i64>(),
            ) {
                sb.total_space = blocks * bs;
                sb.free_space = free * bs;
            }
        }
    }

    // Derive snapshot usage by subtraction (reflink clones make du unreliable)
    let disk_images = sb.cam_size + sb.music_size + sb.lightshow_size + sb.boombox_size;
    let used = sb.total_space - sb.free_space;
    sb.snapshots_size = (used - disk_images).max(0);

    (StatusCode::OK, Json(serde_json::to_value(sb).unwrap_or_default()))
}

// ---------------------------------------------------------------------------
// GET /api/config
// ---------------------------------------------------------------------------

pub async fn get_config(
    State(_state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let has = |p: &str| -> String {
        if std::path::Path::new(p).exists() { "yes".into() } else { "no".into() }
    };

    let mut uses_ble = "no".to_string();
    let config_path = sentryusb_config::find_config_path();
    if let Ok((active, _)) = sentryusb_config::parse_file(config_path) {
        if active.contains_key("TESLA_BLE_VIN") {
            uses_ble = "yes".to_string();
        }
    }

    (StatusCode::OK, Json(serde_json::json!({
        "has_cam": has("/backingfiles/cam_disk.bin"),
        "has_music": has("/backingfiles/music_disk.bin"),
        "has_lightshow": has("/backingfiles/lightshow_disk.bin"),
        "has_boombox": has("/backingfiles/boombox_disk.bin"),
        "uses_ble": uses_ble,
    })))
}

// ---------------------------------------------------------------------------
// GET /api/wifi
// ---------------------------------------------------------------------------

pub async fn get_wifi_config(
    State(_state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut ssid = String::new();
    let mut connected = false;
    let mut source = String::new();

    // 1. Try nmcli
    if let Ok(out) = sentryusb_shell::run("nmcli", &["-t", "-f", "active,ssid", "dev", "wifi"]).await {
        for line in out.lines() {
            if line.starts_with("yes:") {
                ssid = line.strip_prefix("yes:").unwrap_or("").to_string();
                connected = true;
                source = "networkmanager".into();
                break;
            }
        }
    }

    // 2. Fallback: iwgetid
    if ssid.is_empty() {
        if let Ok(out) = sentryusb_shell::run("iwgetid", &["-r"]).await {
            let s = out.trim();
            if !s.is_empty() {
                ssid = s.to_string();
                connected = true;
                source = "iwgetid".into();
            }
        }
    }

    // 3. Fallback: wpa_supplicant.conf
    if ssid.is_empty() {
        for p in &[
            "/etc/wpa_supplicant/wpa_supplicant.conf",
            "/boot/firmware/wpa_supplicant.conf",
            "/boot/wpa_supplicant.conf",
        ] {
            if let Ok(data) = std::fs::read_to_string(p) {
                for line in data.lines() {
                    let trimmed = line.trim();
                    if let Some(val) = trimmed.strip_prefix("ssid=") {
                        let val = val.trim_matches('"');
                        if !val.is_empty() {
                            ssid = val.to_string();
                            source = "wpa_supplicant".into();
                            break;
                        }
                    }
                }
                if !ssid.is_empty() {
                    break;
                }
            }
        }
    }

    // 4. Config SSID
    let mut config_ssid = String::new();
    let config_path = sentryusb_config::find_config_path();
    if let Ok((active, _)) = sentryusb_config::parse_file(config_path) {
        if let Some(v) = active.get("SSID") {
            config_ssid = v.clone();
        }
    }
    // Filter placeholder values
    let lower = config_ssid.to_lowercase();
    if matches!(lower.as_str(), "your_ssid" | "yourssid" | "your_wifi" | "ssid" | "your_network" | "") {
        config_ssid.clear();
    }

    // WLAN country
    let mut wlan_country = String::new();
    if let Ok(out) = sentryusb_shell::run("iw", &["reg", "get"]).await {
        for line in out.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("country") {
                let parts: Vec<&str> = trimmed.splitn(3, ' ').collect();
                if parts.len() >= 2 {
                    wlan_country = parts[1].trim_end_matches(':').to_string();
                }
                break;
            }
        }
    }

    (StatusCode::OK, Json(serde_json::json!({
        "current": {
            "ssid": ssid,
            "connected": connected,
            "source": source,
        },
        "config_ssid": config_ssid,
        "wlan_country": wlan_country,
    })))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// List snapshot backing files at the top of `/backingfiles/snapshots/`.
///
/// The previous implementation called a generic recursive `walkdir` and
/// filtered for paths ending in `snap.bin`. That descended through every
/// snapshot's `mnt -> /tmp/snapshots/snap-NNN` symlink, which is backed by
/// an autofs mount (timeout=300s) that re-mounts the per-snapshot vfat loop
/// device on first access. Each fresh /api/status call after the autofs
/// timeout therefore triggered up to 130 vfat mounts *and* walked the
/// entire dashcam tree inside each one — observed 15,000+ openat syscalls
/// per request and 5-15s TTFB.
///
/// Snapshots always have `snap.bin` directly at the top level
/// (`/backingfiles/snapshots/snap-NNNNNN/snap.bin`). We only need to scan
/// that one directory level — no recursion, no symlink follow.
fn find_snapshots() -> Vec<String> {
    let mut snaps = Vec::new();
    let base = std::path::Path::new("/backingfiles/snapshots/");
    let Ok(entries) = std::fs::read_dir(base) else {
        return snaps;
    };
    for entry in entries.flatten() {
        // Only consider entries that are themselves directories on the
        // host filesystem. `file_type()` uses the dirent's d_type and
        // does NOT follow symlinks, so the `mnt` autofs symlink inside
        // each snapshot is never resolved here.
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let snap_bin = entry.path().join("snap.bin");
        // Use symlink_metadata to avoid traversing into anything weird;
        // snap.bin is always a regular file on the parent XFS.
        if std::fs::symlink_metadata(&snap_bin).is_ok() {
            if let Some(s) = snap_bin.to_str() {
                snaps.push(s.to_string());
            }
        }
    }
    snaps.sort();
    snaps
}

fn read_fan_speed() -> String {
    let base = std::path::Path::new("/sys/devices/platform/cooling_fan/hwmon");
    let Ok(entries) = std::fs::read_dir(base) else {
        return String::new();
    };
    for entry in entries.flatten() {
        let candidate = entry.path().join("fan1_input");
        if let Ok(data) = std::fs::read_to_string(&candidate) {
            return data.trim().to_string();
        }
    }
    String::new()
}

fn read_net_bytes(dev: &str, stat: &str) -> Option<u64> {
    let path = format!("/sys/class/net/{}/statistics/{}", dev, stat);
    std::fs::read_to_string(&path).ok()?.trim().parse::<u64>().ok()
}

fn compute_throughput(sampler: &NetSampler, dev: &str) -> (u64, u64) {
    let Some(rx_now) = read_net_bytes(dev, "rx_bytes") else { return (0, 0); };
    let Some(tx_now) = read_net_bytes(dev, "tx_bytes") else { return (0, 0); };
    let now = Instant::now();
    let mut map = sampler.lock().unwrap_or_else(|e| e.into_inner());
    let result = if let Some(prev) = map.get(dev) {
        let elapsed = now.duration_since(prev.taken_at).as_secs_f64();
        if elapsed < 0.1 {
            (0, 0)
        } else {
            let rx_bps = ((rx_now.saturating_sub(prev.rx_bytes) as f64 * 8.0) / elapsed) as u64;
            let tx_bps = ((tx_now.saturating_sub(prev.tx_bytes) as f64 * 8.0) / elapsed) as u64;
            (rx_bps, tx_bps)
        }
    } else {
        (0, 0)
    };
    map.insert(dev.to_string(), NetSample { rx_bytes: rx_now, tx_bytes: tx_now, taken_at: now });
    result
}

fn find_net_device(pattern: &str) -> String {
    let prefix = pattern.trim_end_matches('*');
    if let Ok(entries) = std::fs::read_dir("/sys/class/net/") {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.starts_with(prefix) {
                    return name.to_string();
                }
            }
        }
    }
    String::new()
}

/// Returns true when the kernel reports the interface in `operstate == "up"`.
///
/// We use this to gate the shell queries below: `iwgetid`/`iwconfig`/`ip` can
/// each block for several seconds when an interface is present-but-DOWN
/// (e.g. `wlan0` exists but no NetworkManager / no Skip-WiFi configured),
/// adding up to 5-15s on `GET /api/status`. Companion apps that probe this
/// endpoint with a short HTTP timeout then fall back to BLE-only mode even
/// though the Pi is reachable over ethernet.
fn iface_is_up(dev: &str) -> bool {
    let path = format!("/sys/class/net/{}/operstate", dev);
    std::fs::read_to_string(&path)
        .map(|s| s.trim() == "up")
        .unwrap_or(false)
}

fn disk_usage(path: &str) -> i64 {
    // On Linux, use stat to get st_blocks * 512 for actual disk usage
    // (handles sparse files and reflink copies correctly)
    if let Ok(out) = std::process::Command::new("stat")
        .args(["--format=%b", path])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Ok(blocks) = s.trim().parse::<i64>() {
                return blocks * 512;
            }
        }
    }
    0
}

/// Get SBC model from device tree.
pub fn get_sbc_model() -> String {
    for p in &["/proc/device-tree/model", "/sys/firmware/devicetree/base/model"] {
        if let Ok(data) = std::fs::read(p) {
            return String::from_utf8_lossy(&data)
                .trim_end_matches('\0')
                .trim()
                .to_string();
        }
    }
    "unknown".to_string()
}
