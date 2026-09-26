//! Shared-key coordination between the Pi sampler and the ESP32-C6.
//!
//! With `TELEMETRY_SOURCE=c6_primary` the C6 usually holds the same Tesla key
//! as the Pi. Two devices signing with one key desync the car's VCSEC counter,
//! so exactly one of them may talk to the car at a time:
//!
//!   * C6 owns (default): the sampler holds NO Tesla BLE session at all.
//!   * Sampler owns: only after the C6 has been dead for `FAILBACK_AFTER_MS`.
//!     It hands back once the C6 has been alive again for `RETURN_SETTLE_MS`.
//!
//! The sampler publishes ownership in a lease file the C6 supervisor checks
//! before every car command. The supervisor may only talk to the car under a
//! live `owner:c6` grant; a missing, expired or `owner:sampler` lease means
//! hold. `sentryusb-ble` (the phone-app peripheral) is a separate role and is
//! never touched here.

use std::time::Duration;

use crate::c6_source::{C6Snapshot, HEARTBEAT_DEAD_MS};

/// Sampler -> supervisor ownership lease (tmpfs, same box).
pub const LEASE_PATH: &str = "/run/sentryusb-c6/sampler_lease.json";

/// The C6 must look dead this long before the sampler takes the car. Several
/// heartbeat windows wide so a supervisor restart or USB blip never fails back.
pub const FAILBACK_AFTER_MS: u64 = 90_000;

/// The C6 must look alive this long before the sampler hands the car back.
/// Together with FAILBACK_AFTER_MS this bounds a flapping C6 to one handoff
/// pair per ~2 minutes.
pub const RETURN_SETTLE_MS: u64 = 30_000;

/// The supervisor rewrites the snapshot every second; older means it stopped.
pub const SNAPSHOT_MAX_AGE_MS: u64 = 10_000;

/// How long a sampler-owned lease stays valid without a refresh. Longer than
/// any single tick (scan + polls) so it can't lapse mid-tick.
pub const LEASE_VALID_MS: u64 = 10 * 60_000;

/// How long a C6 grant stays valid without a refresh (C6-owned ticks refresh
/// it every 15s). If this daemon dies, the C6 stops within this window.
pub const GRANT_VALID_MS: u64 = 2 * 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    C6,
    Sampler,
}

impl Owner {
    pub fn as_str(self) -> &'static str {
        match self {
            Owner::C6 => "c6",
            Owner::Sampler => "sampler",
        }
    }
}

/// Handoff state machine. Pure: time is passed in (CLOCK_BOOTTIME ms).
#[derive(Debug, Clone)]
pub struct Coordinator {
    owner: Owner,
    dead_since: Option<u64>,
    alive_since: Option<u64>,
}

impl Default for Coordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl Coordinator {
    /// Start as C6-owned: on daemon start the sampler must prove the C6 dead
    /// for the full window before touching the car, closing the startup race.
    pub fn new() -> Self {
        Self { owner: Owner::C6, dead_since: None, alive_since: None }
    }

    pub fn owner(&self) -> Owner {
        self.owner
    }

    /// Feed one liveness observation; returns the owner to act on now.
    pub fn step(&mut self, c6_alive: bool, now_ms: u64) -> Owner {
        if c6_alive {
            self.dead_since = None;
            if self.owner == Owner::Sampler {
                let since = *self.alive_since.get_or_insert(now_ms);
                if now_ms.saturating_sub(since) >= RETURN_SETTLE_MS {
                    self.owner = Owner::C6;
                    self.alive_since = None;
                }
            }
        } else {
            self.alive_since = None;
            if self.owner == Owner::C6 {
                let since = *self.dead_since.get_or_insert(now_ms);
                if now_ms.saturating_sub(since) >= FAILBACK_AFTER_MS {
                    self.owner = Owner::Sampler;
                    self.dead_since = None;
                }
            }
        }
        self.owner
    }
}

/// Is the C6 actually able to own the car right now? Requires a snapshot that
/// is still being rewritten, a live serial link with recent heartbeats, a
/// provisioned chip, and the supervisor's poll loop switched on. Anything
/// unknown counts as not-alive only through the (slow) failback window.
pub fn c6_alive(snap: Option<&C6Snapshot>) -> bool {
    let Some(s) = snap else { return false };
    let fresh_file = s.snapshot_age_ms.is_some_and(|a| a <= SNAPSHOT_MAX_AGE_MS);
    let beating = s.heartbeat_age_ms().is_some_and(|a| a <= HEARTBEAT_DEAD_MS);
    fresh_file
        && beating
        && s.healthy
        && s.serial_connected != Some(false)
        && s.provisioned != Some(false)
        && s.polling_enabled != Some(false)
}

/// CLOCK_BOOTTIME in ms: monotonic, counts suspend, never steps with NTP.
pub fn boottime_ms() -> Option<u64> {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid out-pointer to a stack timespec.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    (rc == 0).then(|| ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000)
}

/// This boot's kernel id, so a snapshot or lease from another boot never counts.
pub fn boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_string())
}

/// Lease body. `valid_for_ms` is from `written_boottime_ms`.
pub fn lease_json(owner: Owner, boot_id: &str, now_ms: u64, valid_for_ms: u64) -> String {
    serde_json::json!({
        "owner": owner.as_str(),
        "boot_id": boot_id,
        "written_boottime_ms": now_ms,
        "valid_for_ms": valid_for_ms,
    })
    .to_string()
}

/// Publish the lease atomically (tmp + rename). Returns false if it could not
/// be written: the caller must then NOT connect to the car (a stale grant may
/// still be live for the supervisor).
pub fn write_lease(owner: Owner) -> bool {
    let (Some(boot), Some(now)) = (boot_id(), boottime_ms()) else { return false };
    let valid = match owner {
        Owner::Sampler => LEASE_VALID_MS,
        Owner::C6 => GRANT_VALID_MS,
    };
    let path = std::path::Path::new(LEASE_PATH);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    let ok = std::fs::write(&tmp, lease_json(owner, &boot, now, valid))
        .and_then(|()| std::fs::rename(&tmp, path))
        .is_ok();
    if !ok {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!("C6 coordination: could not write {} lease", owner.as_str());
    }
    ok
}


/// Map an action-socket verb to the C6 firmware command that performs it.
/// None = no C6 equivalent (refused while the C6 owns the link).
pub fn verb_to_c6_command(verb: &str) -> Option<serde_json::Value> {
    use serde_json::json;
    // `await_result`: the firmware tracks these and reports the car's answer
    // (cmd_result); the rest only ack receipt.
    let v = match verb {
        "wake" => json!({"cmd": "wake", "await_result": true}),
        "charge-port-close" => json!({"cmd": "nudge_charge_port_close", "await_result": true}),
        "charge-port-open" => json!({"cmd": "charge_port_open"}),
        "keep-accessory-on" => json!({"cmd": "keep_accessory", "on": true, "await_result": true}),
        "keep-accessory-off" => json!({"cmd": "keep_accessory", "on": false, "await_result": true}),
        "sentry-on" => json!({"cmd": "set_sentry_mode", "on": true}),
        "sentry-off" => json!({"cmd": "set_sentry_mode", "on": false}),
        "charge-start" => json!({"cmd": "charging_start_stop", "start": true}),
        "charge-stop" => json!({"cmd": "charging_start_stop", "start": false}),
        other => {
            if let Some(n) = other.strip_prefix("set-charging-amps:") {
                json!({"cmd": "set_charging_amps", "amps": n.trim().parse::<i64>().ok()?})
            } else if let Some(n) = other.strip_prefix("set-charge-limit:") {
                json!({"cmd": "set_charge_limit", "percent": n.trim().parse::<i64>().ok()?})
            } else {
                return None;
            }
        }
    };
    Some(v)
}

/// POST one command to the supervisor (`/api/coprocessor/command`). Ok only on
/// HTTP 200: the car's own result for `await_result` commands, else the C6's
/// receipt ack. Plain HTTP/1.1 over loopback, bounded.
pub async fn supervisor_command(addr: &str, body: &serde_json::Value) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let payload = body.to_string();
    let req = format!(
        "POST /api/coprocessor/command HTTP/1.1\r\nHost: {addr}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{payload}",
        payload.len()
    );
    let io = async {
        let mut s = tokio::net::TcpStream::connect(addr).await?;
        s.write_all(req.as_bytes()).await?;
        let mut resp = Vec::with_capacity(512);
        s.take(64 * 1024).read_to_end(&mut resp).await?;
        anyhow::Ok(resp)
    };
    // Supervisor: 5s for the ack, up to 30s more for an awaited car result.
    let limit = if body.get("await_result").and_then(|v| v.as_bool()) == Some(true) { 40 } else { 8 };
    let resp = tokio::time::timeout(Duration::from_secs(limit), io)
        .await
        .map_err(|_| anyhow::anyhow!("C6 supervisor timed out"))??;
    let text = String::from_utf8_lossy(&resp);
    let status_ok = text
        .lines()
        .next()
        .is_some_and(|l| l.split_whitespace().nth(1) == Some("200"));
    if status_ok {
        Ok(())
    } else {
        let body = text.split("\r\n\r\n").nth(1).unwrap_or("").trim();
        anyhow::bail!("C6 command failed: {}", if body.is_empty() { "no response" } else { body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::c6_source::C6Snapshot;

    fn alive_snap() -> C6Snapshot {
        C6Snapshot {
            healthy: true,
            last_heartbeat_age_ms: Some(1_000),
            serial_connected: Some(true),
            provisioned: Some(true),
            polling_enabled: Some(true),
            boot_id: Some("b".into()),
            written_boottime_ms: Some(1),
            snapshot_age_ms: Some(500),
            ..Default::default()
        }
    }

    #[test]
    fn healthy_c6_keeps_sampler_off() {
        let mut c = Coordinator::new();
        for t in (0..600_000).step_by(15_000) {
            assert_eq!(c.step(true, t), Owner::C6);
        }
    }

    #[test]
    fn blip_does_not_fail_back() {
        let mut c = Coordinator::new();
        c.step(true, 0);
        // Dead for 60s (< 90s), then back: never handed to the sampler.
        for t in (15_000..=75_000).step_by(15_000) {
            assert_eq!(c.step(false, t), Owner::C6);
        }
        assert_eq!(c.step(true, 90_000), Owner::C6);
        // The dead timer restarted; another short outage still doesn't flip.
        assert_eq!(c.step(false, 100_000), Owner::C6);
        assert_eq!(c.step(false, 180_000), Owner::C6);
    }

    #[test]
    fn sustained_death_fails_back() {
        let mut c = Coordinator::new();
        assert_eq!(c.step(false, 0), Owner::C6);
        assert_eq!(c.step(false, 89_999), Owner::C6);
        assert_eq!(c.step(false, 90_000), Owner::Sampler);
    }

    #[test]
    fn return_settles_before_handing_back() {
        let mut c = Coordinator::new();
        c.step(false, 0);
        assert_eq!(c.step(false, 90_000), Owner::Sampler);
        // C6 back: sampler holds for the settle window, then releases.
        assert_eq!(c.step(true, 100_000), Owner::Sampler);
        assert_eq!(c.step(true, 129_999), Owner::Sampler);
        assert_eq!(c.step(true, 130_000), Owner::C6);
    }

    #[test]
    fn flapping_return_restarts_settle() {
        let mut c = Coordinator::new();
        c.step(false, 0);
        c.step(false, 90_000);
        assert_eq!(c.step(true, 100_000), Owner::Sampler);
        assert_eq!(c.step(false, 110_000), Owner::Sampler); // flap resets settle
        assert_eq!(c.step(true, 120_000), Owner::Sampler);
        assert_eq!(c.step(true, 149_999), Owner::Sampler);
        assert_eq!(c.step(true, 150_000), Owner::C6);
    }

    #[test]
    fn startup_never_takes_the_car_immediately() {
        // Daemon (re)start with no snapshot yet: must wait the full window.
        let mut c = Coordinator::new();
        assert_eq!(c.owner(), Owner::C6);
        assert_eq!(c.step(false, 5_000), Owner::C6);
    }

    #[test]
    fn liveness_requires_every_signal() {
        assert!(c6_alive(Some(&alive_snap())));
        assert!(!c6_alive(None));
        let mut s = alive_snap();
        s.snapshot_age_ms = Some(SNAPSHOT_MAX_AGE_MS + 1); // supervisor stopped writing
        assert!(!c6_alive(Some(&s)));
        let mut s = alive_snap();
        s.snapshot_age_ms = None; // unknown age (other boot)
        assert!(!c6_alive(Some(&s)));
        let mut s = alive_snap();
        s.last_heartbeat_age_ms = Some(HEARTBEAT_DEAD_MS); // + 500ms file age
        assert!(!c6_alive(Some(&s)));
        let mut s = alive_snap();
        s.serial_connected = Some(false);
        assert!(!c6_alive(Some(&s)));
        let mut s = alive_snap();
        s.provisioned = Some(false);
        assert!(!c6_alive(Some(&s)));
        let mut s = alive_snap();
        s.polling_enabled = Some(false);
        assert!(!c6_alive(Some(&s)));
        let mut s = alive_snap();
        s.healthy = false;
        assert!(!c6_alive(Some(&s)));
    }

    #[test]
    fn verbs_map_to_c6_commands() {
        assert_eq!(verb_to_c6_command("charge-port-close").unwrap()["cmd"], "nudge_charge_port_close");
        assert_eq!(verb_to_c6_command("keep-accessory-off").unwrap()["on"], false);
        assert_eq!(verb_to_c6_command("set-charge-limit:80").unwrap()["percent"], 80);
        assert_eq!(verb_to_c6_command("set-charging-amps:16").unwrap()["amps"], 16);
        assert!(verb_to_c6_command("set-charging-amps:x").is_none());
        assert!(verb_to_c6_command("pair").is_none());
        assert!(verb_to_c6_command("unlock").is_none());
    }

    #[test]
    fn lease_json_shape() {
        let v: serde_json::Value = serde_json::from_str(&lease_json(Owner::Sampler, "b", 7, 9)).unwrap();
        assert_eq!(v["owner"], "sampler");
        assert_eq!(v["boot_id"], "b");
        assert_eq!(v["written_boottime_ms"], 7);
        assert_eq!(v["valid_for_ms"], 9);
    }
}
