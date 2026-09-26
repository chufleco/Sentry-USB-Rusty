//! Shared-key coordination between the Pi sampler and the ESP32-C6.
//!
//! With `TELEMETRY_SOURCE=c6_primary` the C6 usually holds the same Tesla key
//! as the Pi. Two devices signing with one key desync the car's VCSEC counter,
//! so exactly one of them may talk to the car at a time:
//!
//!   * C6 owns (default): the sampler holds NO Tesla BLE session at all.
//!   * Sampler owns: after the C6 has been dead for `FAILBACK_AFTER_MS`, or
//!     alive but unable to reach the car for `CAR_LINK_FAILBACK_MS`. It hands
//!     back once the C6 has recovered for `RETURN_SETTLE_MS` (or, after a
//!     car-link failback, on a backed-off trial grant).
//!
//! The sampler publishes ownership in a lease file the C6 supervisor checks
//! before every car command. The supervisor may only talk to the car under a
//! live `owner:c6` grant; a missing, expired or `owner:sampler` lease means
//! hold. `sentryusb-ble` (the phone-app peripheral) is a separate role and is
//! never touched here.

use std::time::Duration;

use crate::c6_source::{C6Snapshot, HEARTBEAT_DEAD_MS};

/// Sampler -> supervisor ownership lease (tmpfs, same box). Preserved across
/// supervisor restarts (RuntimeDirectoryPreserve=yes) so a just-expired grant
/// stays visible; a missing lease (fresh boot) means "hold" to the supervisor.
pub const LEASE_PATH: &str = "/run/sentryusb-c6/sampler_lease.json";

/// Serializes every lease read-decide-write across processes (this daemon and
/// sentryusb-ble-action). Must match sentryusb_ble_action.rs.
pub const LEASE_LOCK_PATH: &str = "/run/sentryusb-c6/lease.lock";

/// Held exclusive flock on LEASE_LOCK_PATH; released on drop.
pub struct LeaseLock(#[allow(dead_code)] std::fs::File);

/// Take the lease lock (blocking; holders only do tmpfs I/O). None = couldn't
/// open or lock: callers must then neither grant nor connect.
pub fn lease_lock() -> Option<LeaseLock> {
    use std::os::fd::AsRawFd;
    let path = std::path::Path::new(LEASE_LOCK_PATH);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path).ok()?;
    // SAFETY: valid fd owned by `f` for the duration of the call.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
    (rc == 0).then_some(LeaseLock(f))
}

/// Who wrote a sampler lease: this daemon, or `sentryusb-ble-action` holding
/// the car for a one-shot direct action while the daemon was down.
pub const HOLDER_TELEMETRY: &str = "telemetry";
pub const HOLDER_BLE_ACTION: &str = "ble-action";

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

/// After withdrawing a C6 grant, wait out the firmware's own 30s command
/// deadline before the sampler signs, so a C6 command already in flight ends.
pub const CLAIM_SETTLE_MS: u64 = 35_000;

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

/// Why the sampler currently owns the car.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The C6 device itself is gone (serial / heartbeat / supervisor).
    DeviceDead,
    /// The C6 is alive but has not reached the car for CAR_LINK_FAILBACK_MS.
    CarLink,
}

/// One liveness observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Health {
    pub device_alive: bool,
    /// The C6 is not reaching the car (and the car isn't known to be asleep).
    pub car_link_down: bool,
}

/// C6 device-alive but car link down this long => the sampler takes the car.
/// Worst-case keep-awake gap before the Pi can nudge: car-link path ~6.6 min
/// (60s stale + 5 min + 35s settle); device-death path ~2.4 min (16s heartbeat
/// + 90s FAILBACK_AFTER_MS + 35s settle).
pub const CAR_LINK_FAILBACK_MS: u64 = 5 * 60_000;

/// A car last seen ASLEEP that stops answering is not an outage, but only for
/// this long after that reading, and never while the car is known awake
/// (clips being written, or a keep-awake/archive in effect): the reading can't
/// refresh once the link itself is dead.
pub const ASLEEP_EXEMPT_MS: u64 = 30 * 60_000;

/// No car contact for this long counts as "car link down" (the C6's idle
/// keepalive answers every ~8s while it holds a session).
pub const CAR_LINK_STALE_MS: u64 = 60_000;

/// After a car-link failback the C6 is parked, so its link can't be observed
/// recovering; it gets a trial grant after the next step of this ladder
/// (one step per repeat failure, then stays at the last).
pub const CAR_LINK_TRIAL_LADDER_MS: [u64; 4] = [5 * 60_000, 15 * 60_000, 30 * 60_000, 60 * 60_000];

/// C6 fully healthy this long resets the trial backoff.
pub const BACKOFF_RESET_MS: u64 = 30 * 60_000;

/// Handoff state machine. Pure: time is passed in (CLOCK_BOOTTIME ms).
#[derive(Debug, Clone)]
pub struct Coordinator {
    owner: Owner,
    reason: Option<Reason>,
    dead_since: Option<u64>,
    link_down_since: Option<u64>,
    /// Sampler-owned: when the recovery signal (device alive / link ok) began.
    recovered_since: Option<u64>,
    /// C6-owned: when it last became fully healthy (backoff reset).
    healthy_since: Option<u64>,
    trial_at: Option<u64>,
    /// Index into CAR_LINK_TRIAL_LADDER_MS for the next car-link failback.
    trial_step: usize,
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
        Self {
            owner: Owner::C6,
            reason: None,
            dead_since: None,
            link_down_since: None,
            recovered_since: None,
            healthy_since: None,
            trial_at: None,
            trial_step: 0,
        }
    }

    pub fn owner(&self) -> Owner {
        self.owner
    }

    pub fn reason(&self) -> Option<Reason> {
        self.reason
    }

    fn take_car(&mut self, reason: Reason) {
        self.owner = Owner::Sampler;
        self.reason = Some(reason);
        self.dead_since = None;
        self.link_down_since = None;
        self.recovered_since = None;
        self.healthy_since = None;
    }

    fn hand_back(&mut self) {
        self.owner = Owner::C6;
        self.reason = None;
        self.recovered_since = None;
        self.trial_at = None;
    }

    /// The C6 is unplugged: the sampler owns now (stock behaviour). A replug
    /// then needs the normal RETURN_SETTLE_MS of device-alive to hand back.
    pub fn mark_absent(&mut self) {
        if self.owner == Owner::C6 {
            self.take_car(Reason::DeviceDead);
        }
        // Device rules from here, even mid car-link failback: a replugged C6
        // is handed back on device-alive, not on a link it can't show parked.
        self.reason = Some(Reason::DeviceDead);
        self.trial_at = None;
        self.recovered_since = None;
    }

    /// Device-only observation (car link assumed fine). Test convenience.
    #[cfg(test)]
    pub fn step(&mut self, c6_alive: bool, now_ms: u64) -> Owner {
        self.step_health(Health { device_alive: c6_alive, car_link_down: false }, now_ms)
    }

    /// Feed one observation; returns the owner to act on now.
    pub fn step_health(&mut self, h: Health, now_ms: u64) -> Owner {
        match self.owner {
            Owner::C6 => {
                if !h.device_alive {
                    self.link_down_since = None;
                    self.healthy_since = None;
                    let since = *self.dead_since.get_or_insert(now_ms);
                    if now_ms.saturating_sub(since) >= FAILBACK_AFTER_MS {
                        self.take_car(Reason::DeviceDead);
                    }
                } else if h.car_link_down {
                    self.dead_since = None;
                    self.healthy_since = None;
                    let since = *self.link_down_since.get_or_insert(now_ms);
                    if now_ms.saturating_sub(since) >= CAR_LINK_FAILBACK_MS {
                        self.take_car(Reason::CarLink);
                        let wait = CAR_LINK_TRIAL_LADDER_MS[self.trial_step];
                        self.trial_at = Some(now_ms.saturating_add(wait));
                        self.trial_step = (self.trial_step + 1).min(CAR_LINK_TRIAL_LADDER_MS.len() - 1);
                    }
                } else {
                    self.dead_since = None;
                    self.link_down_since = None;
                    let since = *self.healthy_since.get_or_insert(now_ms);
                    if now_ms.saturating_sub(since) >= BACKOFF_RESET_MS {
                        self.trial_step = 0;
                    }
                }
            }
            Owner::Sampler => {
                if self.reason == Some(Reason::CarLink) && !h.device_alive {
                    // The C6 died outright meanwhile: device rules from here.
                    self.reason = Some(Reason::DeviceDead);
                    self.recovered_since = None;
                    self.trial_at = None;
                }
                let recovered = match self.reason {
                    Some(Reason::CarLink) => !h.car_link_down,
                    _ => h.device_alive,
                };
                if recovered {
                    let since = *self.recovered_since.get_or_insert(now_ms);
                    if now_ms.saturating_sub(since) >= RETURN_SETTLE_MS {
                        self.hand_back();
                    }
                } else {
                    self.recovered_since = None;
                    if self.trial_at.is_some_and(|t| now_ms >= t) {
                        self.hand_back(); // trial grant; a dead link fails back again
                    }
                }
            }
        }
        self.owner
    }
}

/// Is the C6 reaching the car? False (= not down) when the supervisor doesn't
/// report car contact at all (older build), or the car was seen ASLEEP within
/// ASLEEP_EXEMPT_MS and nothing shows it awake now (`car_known_awake`: clips
/// being written, or a keep-awake/archive in effect).
pub fn car_link_down(snap: Option<&C6Snapshot>, car_known_awake: bool) -> bool {
    let Some(s) = snap else { return false };
    if !s.car_link_reported {
        return false;
    }
    let recently_asleep = s.car_sleep_status.as_deref().is_some_and(|v| v.eq_ignore_ascii_case("ASLEEP"))
        && s.car_sleep_age_ms().is_some_and(|a| a <= ASLEEP_EXEMPT_MS);
    if recently_asleep && !car_known_awake {
        return false;
    }
    match s.car_ok_age_ms() {
        Some(age) => age > CAR_LINK_STALE_MS,
        None => true, // never reached the car since the supervisor started
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

/// udev symlink for the C6's USB serial (99-sentryusb-c6.rules).
pub const C6_DEVICE: &str = "/dev/sentryusb-c6";

/// Is a C6 plugged in at all? Absent = stock sampler behaviour, no waiting.
pub fn c6_present() -> bool {
    std::path::Path::new(C6_DEVICE).exists()
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
        "holder": HOLDER_TELEMETRY,
        "boot_id": boot_id,
        "written_boottime_ms": now_ms,
        "valid_for_ms": valid_for_ms,
    })
    .to_string()
}

/// Pure: is `lease` a live C6 grant from this boot? (Mirrors the supervisor.)
pub fn grant_live_in(lease: &str, boot_id: &str, now_ms: u64) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(lease) else { return false };
    v.get("owner").and_then(|x| x.as_str()) == Some("c6")
        && v.get("boot_id").and_then(|x| x.as_str()) == Some(boot_id)
        && match (
            v.get("written_boottime_ms").and_then(|x| x.as_u64()),
            v.get("valid_for_ms").and_then(|x| x.as_u64()),
        ) {
            (Some(w), Some(valid)) => now_ms < w.saturating_add(valid),
            _ => false,
        }
}

/// A C6 grant on disk is live, or expired less than CLAIM_SETTLE_MS ago (e.g.
/// written by a previous run of this daemon): the C6 may be mid-command.
pub fn disk_grant_live() -> bool {
    let (Some(boot), Some(now)) = (boot_id(), boottime_ms()) else { return false };
    std::fs::read_to_string(LEASE_PATH)
        .is_ok_and(|l| grant_live_in(&l, &boot, now.saturating_sub(CLAIM_SETTLE_MS)))
}

/// Pure: is `lease` a live sampler lease held by `sentryusb-ble-action`?
pub fn ble_action_lease_live_in(lease: &str, boot_id: &str, now_ms: u64) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(lease) else { return false };
    v.get("owner").and_then(|x| x.as_str()) == Some("sampler")
        && v.get("holder").and_then(|x| x.as_str()) == Some(HOLDER_BLE_ACTION)
        && v.get("boot_id").and_then(|x| x.as_str()) == Some(boot_id)
        && match (
            v.get("written_boottime_ms").and_then(|x| x.as_u64()),
            v.get("valid_for_ms").and_then(|x| x.as_u64()),
        ) {
            (Some(w), Some(valid)) => now_ms < w.saturating_add(valid),
            _ => true, // malformed: assume it still holds the car
        }
}

/// `sentryusb-ble-action` is mid direct session (it claimed while this daemon
/// was down): the C6 must not be granted until that lease lapses.
pub fn ble_action_holds_car() -> bool {
    let (Some(boot), Some(now)) = (boot_id(), boottime_ms()) else { return true };
    std::fs::read_to_string(LEASE_PATH).is_ok_and(|l| ble_action_lease_live_in(&l, &boot, now))
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

    const UP: Health = Health { device_alive: true, car_link_down: false };
    const LINK_DOWN: Health = Health { device_alive: true, car_link_down: true };

    #[test]
    fn five_minute_car_link_outage_fails_back() {
        let mut c = Coordinator::new();
        assert_eq!(c.step_health(LINK_DOWN, 0), Owner::C6);
        assert_eq!(c.step_health(LINK_DOWN, CAR_LINK_FAILBACK_MS - 1), Owner::C6);
        assert_eq!(c.step_health(LINK_DOWN, CAR_LINK_FAILBACK_MS), Owner::Sampler);
        assert_eq!(c.reason(), Some(Reason::CarLink));
    }

    #[test]
    fn short_car_link_outage_does_not_flip() {
        let mut c = Coordinator::new();
        c.step_health(LINK_DOWN, 0);
        assert_eq!(c.step_health(LINK_DOWN, 240_000), Owner::C6);
        assert_eq!(c.step_health(UP, 250_000), Owner::C6);
        // Timer restarted: another 4 min outage still doesn't flip.
        assert_eq!(c.step_health(LINK_DOWN, 260_000), Owner::C6);
        assert_eq!(c.step_health(LINK_DOWN, 500_000), Owner::C6);
    }

    #[test]
    fn car_link_recovery_hands_back_after_settle() {
        let mut c = Coordinator::new();
        c.step_health(LINK_DOWN, 0);
        c.step_health(LINK_DOWN, CAR_LINK_FAILBACK_MS);
        let t = CAR_LINK_FAILBACK_MS + 10_000;
        assert_eq!(c.step_health(UP, t), Owner::Sampler);
        assert_eq!(c.step_health(UP, t + RETURN_SETTLE_MS - 1), Owner::Sampler);
        assert_eq!(c.step_health(UP, t + RETURN_SETTLE_MS), Owner::C6);
    }

    /// Fail back on a dead link starting at `t`; returns when the trial grant lands.
    fn fail_and_trial(c: &mut Coordinator, t: u64, expect_wait: u64) -> u64 {
        c.step_health(LINK_DOWN, t);
        let f = t + CAR_LINK_FAILBACK_MS;
        assert_eq!(c.step_health(LINK_DOWN, f), Owner::Sampler);
        assert_eq!(c.step_health(LINK_DOWN, f + expect_wait - 1), Owner::Sampler);
        assert_eq!(c.step_health(LINK_DOWN, f + expect_wait), Owner::C6, "trial after {expect_wait}ms");
        f + expect_wait
    }

    #[test]
    fn parked_c6_trial_ladder_is_5_15_30_60_then_60() {
        // After a car-link failback the C6 is parked, so the link stays "down":
        // each repeat failure waits the next ladder step before a trial grant.
        let mut c = Coordinator::new();
        let mut t = 0;
        for m in [5, 15, 30, 60, 60, 60] {
            t = fail_and_trial(&mut c, t + 1, m * 60_000);
        }
    }

    #[test]
    fn thirty_healthy_minutes_reset_the_ladder_to_5m() {
        let mut c = Coordinator::new();
        let mut t = fail_and_trial(&mut c, 0, 5 * 60_000);
        t = fail_and_trial(&mut c, t + 1, 15 * 60_000);
        // Trial works: healthy just under 30 min does NOT reset...
        c.step_health(UP, t + 1);
        c.step_health(UP, t + BACKOFF_RESET_MS);
        t = fail_and_trial(&mut c, t + BACKOFF_RESET_MS + 1, 30 * 60_000);
        // ...a full 30 healthy minutes does.
        c.step_health(UP, t + 1);
        c.step_health(UP, t + 1 + BACKOFF_RESET_MS);
        fail_and_trial(&mut c, t + 2 + BACKOFF_RESET_MS, 5 * 60_000);
    }

    #[test]
    fn device_death_during_car_link_failback_switches_to_device_rules() {
        let mut c = Coordinator::new();
        c.step_health(LINK_DOWN, 0);
        c.step_health(LINK_DOWN, CAR_LINK_FAILBACK_MS);
        let dead = Health { device_alive: false, car_link_down: true };
        c.step_health(dead, CAR_LINK_FAILBACK_MS + 1);
        assert_eq!(c.reason(), Some(Reason::DeviceDead));
        // No trial grant to a dead device, however long we wait.
        assert_eq!(c.step_health(dead, 10 * 60 * 60_000), Owner::Sampler);
    }

    fn link_snap(age: Option<u64>, sleep: Option<&str>) -> C6Snapshot {
        C6Snapshot {
            car_link_reported: true,
            car_ok_age_at_write_ms: age,
            car_sleep_status: sleep.map(str::to_string),
            car_sleep_age_at_write_ms: age,
            snapshot_age_ms: Some(500),
            ..alive_snap()
        }
    }

    #[test]
    fn car_asleep_is_not_a_car_link_outage() {
        let asleep_20m = link_snap(Some(20 * 60_000), Some("ASLEEP"));
        assert!(!car_link_down(Some(&asleep_20m), false));
        // And over the exemption window the coordinator never fails back on it.
        let mut c = Coordinator::new();
        let h = Health { device_alive: true, car_link_down: car_link_down(Some(&asleep_20m), false) };
        for t in (0..ASLEEP_EXEMPT_MS).step_by(15_000) {
            assert_eq!(c.step_health(h, t), Owner::C6);
        }
    }

    #[test]
    fn asleep_exemption_is_bounded_and_void_while_recording() {
        // Stale ASLEEP (link dead ever since): an outage after all.
        let asleep_old = link_snap(Some(ASLEEP_EXEMPT_MS + 1), Some("ASLEEP"));
        assert!(car_link_down(Some(&asleep_old), false));
        // Recent ASLEEP but clips are being written: the car is awake, so an outage.
        let asleep_recent = link_snap(Some(2 * 60_000), Some("ASLEEP"));
        assert!(car_link_down(Some(&asleep_recent), true));
        // ASLEEP with no age reported: no exemption.
        let no_age = C6Snapshot { car_sleep_age_at_write_ms: None, ..asleep_recent };
        assert!(car_link_down(Some(&no_age), false));
    }

    #[test]
    fn replug_during_car_link_failback_uses_device_rules() {
        let mut c = Coordinator::new();
        c.step_health(LINK_DOWN, 0);
        c.step_health(LINK_DOWN, CAR_LINK_FAILBACK_MS);
        assert_eq!(c.reason(), Some(Reason::CarLink));
        c.mark_absent();
        assert_eq!(c.reason(), Some(Reason::DeviceDead));
        // Replugged, alive but parked (link "down"): handed back on device-alive.
        let t = CAR_LINK_FAILBACK_MS + 1;
        assert_eq!(c.step_health(LINK_DOWN, t), Owner::Sampler);
        assert_eq!(c.step_health(LINK_DOWN, t + RETURN_SETTLE_MS), Owner::C6);
    }

    #[test]
    fn lease_lock_is_exclusive_across_handles() {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("l");
        let a = std::fs::File::create(&p).unwrap();
        let b = std::fs::File::create(&p).unwrap();
        unsafe {
            assert_eq!(libc::flock(a.as_raw_fd(), libc::LOCK_EX), 0);
            assert_ne!(libc::flock(b.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB), 0, "second holder must wait");
            drop(a);
            assert_eq!(libc::flock(b.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB), 0, "free after drop");
        }
    }

    #[test]
    fn ble_action_lease_blocks_the_grant_only_while_live() {
        let l = |holder: &str| {
            format!(r#"{{"owner":"sampler","holder":"{holder}","boot_id":"b","written_boottime_ms":0,"valid_for_ms":120000}}"#)
        };
        assert!(ble_action_lease_live_in(&l("ble-action"), "b", 60_000));
        assert!(!ble_action_lease_live_in(&l("ble-action"), "b", 120_000));
        assert!(!ble_action_lease_live_in(&l("telemetry"), "b", 60_000));
        assert!(!ble_action_lease_live_in(&l("ble-action"), "other", 60_000));
    }

    #[test]
    fn car_link_down_reads_the_snapshot() {
        assert!(!car_link_down(Some(&link_snap(Some(5_000), Some("AWAKE"))), false));
        assert!(car_link_down(Some(&link_snap(Some(CAR_LINK_STALE_MS + 1), Some("AWAKE"))), false));
        assert!(car_link_down(Some(&link_snap(None, None)), false), "never reached the car");
        // Older supervisor that doesn't report car contact: never "down".
        let legacy = C6Snapshot { car_link_reported: false, ..link_snap(None, None) };
        assert!(!car_link_down(Some(&legacy), false));
    }

    #[test]
    fn unplugged_c6_hands_the_car_to_the_sampler_at_once() {
        let mut c = Coordinator::new();
        c.mark_absent();
        assert_eq!(c.owner(), Owner::Sampler);
        // Replugged: needs the normal alive settle before handing back.
        assert_eq!(c.step_health(UP, 0), Owner::Sampler);
        assert_eq!(c.step_health(UP, RETURN_SETTLE_MS), Owner::C6);
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
    fn grant_check_matches_the_supervisor() {
        assert!(grant_live_in(&lease_json(Owner::C6, "b", 1_000, GRANT_VALID_MS), "b", 5_000));
        assert!(!grant_live_in(&lease_json(Owner::C6, "b", 1_000, GRANT_VALID_MS), "b", 1_000 + GRANT_VALID_MS));
        assert!(!grant_live_in(&lease_json(Owner::Sampler, "b", 1_000, LEASE_VALID_MS), "b", 5_000));
        assert!(!grant_live_in(&lease_json(Owner::C6, "other", 1_000, GRANT_VALID_MS), "b", 5_000));
        assert!(!grant_live_in("garbage", "b", 5_000));
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
