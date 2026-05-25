//! Push 6a: in-process Tesla BLE sampler.
//!
//! Drop-in replacement for the shell-out paths in `sample.rs`. Same
//! result types (DriveResult, ClimateResult, etc.) — only the
//! transport changes. main.rs holds one `PersistentSession` for the
//! sampler's lifetime and threads it through every call here, so a
//! warm query lands in ~250-350 ms instead of the ~1.5-2 s the
//! shell-out path takes (per-call scan + connect + handshake +
//! tesla-control exec).
//!
//! The `body_controller` path stays unauthenticated (no SessionInfo
//! required) and works against a sleeping car — same as before.

use std::time::Instant;

use anyhow::Result;
use sentryusb_tesla_ble::{
    keys::KeyPair, manager::PersistentSession, proto::car_server,
};
use tracing::{info, warn};

use crate::sample::{
    BodyControllerSample, ChargeResult, ChargingState, ClimateResult, ClosuresResult,
    DriveResult, ResponseMeta, Sample, SentryMode, ShiftState, TiresResult, now_secs,
};

/// 1 bar = 14.5038 psi (NIST). Tesla reports TPMS in bar on the wire.
/// Rounded to 1 decimal so the DB doesn't carry FP noise we can't
/// observe at display time. Mirrors the helper in sample.rs.
fn bar_to_psi(bar: f64) -> f64 {
    ((bar * 14.5038) * 10.0).round() / 10.0
}

/// Build a ResponseMeta from a car_server `google.protobuf.Timestamp`
/// (every state response carries one in the relevant sub-message).
/// Mirrors `sample::build_meta` but for proto-typed inputs.
fn build_meta(ts: Option<&prost_types::Timestamp>, started: Instant) -> ResponseMeta {
    let vehicle_ts_ms = ts.map(|t| t.seconds * 1000 + (t.nanos / 1_000_000) as i64);
    ResponseMeta {
        vehicle_ts_ms,
        request_started_at: Some(started),
    }
}

/// Map car_server's nested ShiftState oneof to our flat enum. Tesla's
/// proto uses a oneof with Void leaves (P, R, N, D) which makes for
/// awkward pattern-matching; collapse to the enum the rest of the
/// crate already uses.
fn map_shift_state(ss: &car_server::ShiftState) -> ShiftState {
    use car_server::shift_state::Type;
    match &ss.r#type {
        Some(Type::P(_)) => ShiftState::Park,
        Some(Type::R(_)) => ShiftState::Reverse,
        Some(Type::N(_)) => ShiftState::Neutral,
        Some(Type::D(_)) => ShiftState::Drive,
        _ => ShiftState::Unknown,
    }
}

/// Map car_server's nested `ChargeState.ChargingState` oneof to our
/// flat `ChargingState`. Same pattern as `map_shift_state`. Returns
/// `Unknown` if the oneof is empty (Tesla didn't include the field
/// at all) — caller turns that into `None` so the phase machine's
/// conservative default kicks in.
fn map_charging_state(cs: &car_server::charge_state::ChargingState) -> ChargingState {
    use car_server::charge_state::charging_state::Type;
    match &cs.r#type {
        Some(Type::Unknown(_)) => ChargingState::Unknown,
        Some(Type::Disconnected(_)) => ChargingState::Disconnected,
        Some(Type::NoPower(_)) => ChargingState::NoPower,
        Some(Type::Starting(_)) => ChargingState::Starting,
        Some(Type::Charging(_)) => ChargingState::Charging,
        Some(Type::Complete(_)) => ChargingState::Complete,
        Some(Type::Stopped(_)) => ChargingState::Stopped,
        Some(Type::Calibrating(_)) => ChargingState::Calibrating,
        None => ChargingState::Unknown,
    }
}

/// Map `ClosuresState.SentryModeState` oneof to our flat `SentryMode`.
/// Returns `None` to the caller when the oneof is empty, so the phase
/// machine's conservative default (treat as "on" → stay Active) wins.
fn map_sentry_mode(sm: &car_server::closures_state::SentryModeState) -> SentryMode {
    use car_server::closures_state::sentry_mode_state::Type;
    match &sm.r#type {
        Some(Type::Off(_)) => SentryMode::Off,
        Some(Type::Idle(_)) => SentryMode::Idle,
        Some(Type::Armed(_)) => SentryMode::Armed,
        Some(Type::Aware(_)) => SentryMode::Aware,
        Some(Type::Panic(_)) => SentryMode::Panic,
        Some(Type::Quiet(_)) => SentryMode::Quiet,
        // Empty oneof — treat as Off, but callers should also be
        // checking whether the parent field was even present (it's
        // an optional sub-message). map_*_optional below handles both.
        None => SentryMode::Off,
    }
}

/// `state drive` over BLE. Carries the three signals that matter for
/// drive tracking: shift state (phase machine), odometer (mile
/// counter), and a placeholder for location_name (which actually
/// lives in `state location` — kept None here for parity with the
/// shell-out path, which also returned None for drive's location).
pub async fn sample_drive_ble(session: &PersistentSession) -> Result<DriveResult> {
    let started = Instant::now();
    let drive = session.get_drive().await?;
    let elapsed = started.elapsed().as_millis();
    info!("state-poll: drive=ok({}ms) via in-process BLE", elapsed);

    let shift_state = drive.shift_state.as_ref().map(map_shift_state);
    let odometer_mi = drive
        .optional_odometer_in_hundredths_of_a_mile
        .as_ref()
        .map(|o| {
            let car_server::drive_state::OptionalOdometerInHundredthsOfAMile::OdometerInHundredthsOfAMile(h) = o;
            (*h as f64) / 100.0
        });
    let meta = build_meta(drive.timestamp.as_ref(), started);

    Ok(DriveResult {
        location_name: None,
        odometer_mi,
        shift_state,
        meta,
    })
}

/// `state climate` over BLE. Interior/exterior temps + HVAC on/off.
pub async fn sample_climate_ble(session: &PersistentSession) -> Result<ClimateResult> {
    let started = Instant::now();
    let climate = session.get_climate().await?;
    let elapsed = started.elapsed().as_millis();
    info!("state-poll: climate=ok({}ms) via in-process BLE", elapsed);

    let interior_temp_c = climate
        .optional_inside_temp_celsius
        .as_ref()
        .map(|v| {
            let car_server::climate_state::OptionalInsideTempCelsius::InsideTempCelsius(t) = v;
            *t as f64
        });
    let exterior_temp_c = climate
        .optional_outside_temp_celsius
        .as_ref()
        .map(|v| {
            let car_server::climate_state::OptionalOutsideTempCelsius::OutsideTempCelsius(t) = v;
            *t as f64
        });
    let hvac_on = climate
        .optional_is_climate_on
        .as_ref()
        .map(|v| {
            let car_server::climate_state::OptionalIsClimateOn::IsClimateOn(b) = v;
            *b
        });
    let meta = build_meta(climate.timestamp.as_ref(), started);

    Ok(ClimateResult {
        interior_temp_c,
        exterior_temp_c,
        hvac_on,
        meta,
    })
}

/// `state charge` over BLE. Battery percent (usable preferred,
/// falling back to display battery_level).
pub async fn sample_charge_ble(session: &PersistentSession) -> Result<ChargeResult> {
    let started = Instant::now();
    let charge = session.get_charge().await?;
    let elapsed = started.elapsed().as_millis();
    info!("state-poll: charge=ok({}ms) via in-process BLE", elapsed);

    // Prefer usable_battery_level (matches Tesla app's headline %),
    // fall back to raw battery_level when usable isn't reported.
    let battery_pct = charge
        .optional_usable_battery_level
        .as_ref()
        .map(|v| {
            let car_server::charge_state::OptionalUsableBatteryLevel::UsableBatteryLevel(n) = v;
            *n as f64
        })
        .or_else(|| {
            charge.optional_battery_level.as_ref().map(|v| {
                let car_server::charge_state::OptionalBatteryLevel::BatteryLevel(n) = v;
                *n as f64
            })
        });
    // charging_state is a non-optional sub-message; the inner oneof
    // can still be empty (mapped to Unknown by map_charging_state).
    // We hand the phase machine an Option so it can tell "field
    // populated with Unknown" apart from "we never extracted one" —
    // both currently behave identically (conservative: stay Active),
    // but keeping the distinction makes future logic changes safer.
    let charging_state = charge.charging_state.as_ref().map(map_charging_state);
    let meta = build_meta(charge.timestamp.as_ref(), started);

    Ok(ChargeResult {
        battery_pct,
        charging_state,
        meta,
    })
}

/// `state closures` over BLE. Only field we currently care about is
/// `sentry_mode_state`, which the phase machine uses to decide
/// whether dropping to quiet polling is safe. Doors / windows /
/// charge-port states are available from the same response and could
/// be surfaced in the UI in the future without an additional poll.
pub async fn sample_closures_ble(session: &PersistentSession) -> Result<ClosuresResult> {
    let started = Instant::now();
    let closures = session.get_closures().await?;
    let elapsed = started.elapsed().as_millis();
    info!("state-poll: closures=ok({}ms) via in-process BLE", elapsed);

    // Tesla only populates `sentry_mode_state` on cars that support
    // sentry mode — older / fleet vehicles return the parent message
    // without this sub-field. Treating "absent" as None (rather than
    // Off) lets the phase machine's conservative default (stay
    // Active until we have proof) handle both "feature unsupported"
    // and "first poll hasn't landed yet" the same way.
    let sentry_mode = closures.sentry_mode_state.as_ref().map(map_sentry_mode);
    let meta = build_meta(closures.timestamp.as_ref(), started);

    Ok(ClosuresResult { sentry_mode, meta })
}

/// `state tire-pressure` over BLE. Converts Tesla's native bar →
/// PSI to match what's displayed in the UI (US convention).
pub async fn sample_tires_ble(session: &PersistentSession) -> Result<TiresResult> {
    let started = Instant::now();
    let tires = session.get_tire_pressure().await?;
    let elapsed = started.elapsed().as_millis();
    info!("state-poll: tires=ok({}ms) via in-process BLE", elapsed);

    let fl = tires.optional_tpms_pressure_fl.as_ref().map(|v| {
        let car_server::tire_pressure_state::OptionalTpmsPressureFl::TpmsPressureFl(b) = v;
        bar_to_psi(*b as f64)
    });
    let fr = tires.optional_tpms_pressure_fr.as_ref().map(|v| {
        let car_server::tire_pressure_state::OptionalTpmsPressureFr::TpmsPressureFr(b) = v;
        bar_to_psi(*b as f64)
    });
    let rl = tires.optional_tpms_pressure_rl.as_ref().map(|v| {
        let car_server::tire_pressure_state::OptionalTpmsPressureRl::TpmsPressureRl(b) = v;
        bar_to_psi(*b as f64)
    });
    let rr = tires.optional_tpms_pressure_rr.as_ref().map(|v| {
        let car_server::tire_pressure_state::OptionalTpmsPressureRr::TpmsPressureRr(b) = v;
        bar_to_psi(*b as f64)
    });
    let meta = build_meta(tires.timestamp.as_ref(), started);

    Ok(TiresResult {
        tire_fl_psi: fl,
        tire_fr_psi: fr,
        tire_rl_psi: rl,
        tire_rr_psi: rr,
        meta,
    })
}

/// `body-controller-state` over BLE. Stays unauthenticated — works
/// against a sleeping car without waking it. Now routed through the
/// PersistentSession's held GATT connection instead of opening its
/// own throwaway connection, which used to fight the persistent
/// session for bluez and caused framing-desync errors + multi-second
/// outliers on the body-controller poll itself.
pub async fn sample_body_controller_ble(
    session: &PersistentSession,
) -> Result<BodyControllerSample> {
    let start = Instant::now();
    let result = session.body_controller_state().await;
    let elapsed_ms = start.elapsed().as_millis() as u64;
    match &result {
        Ok(_) => info!("body-controller poll: ok({}ms) via in-process BLE", elapsed_ms),
        Err(e) => warn!("body-controller poll: err({}ms): {:#}", elapsed_ms, e),
    }
    let resp = result?;

    // UserPresence_E (vcsec.proto): 0=UNKNOWN, 1=NOT_PRESENT, 2=PRESENT.
    // Treat unknown as None so the phase machine doesn't take action
    // on a non-signal; only collapse the present/not-present cases to
    // a bool.
    let user_presence = match resp.user_presence {
        2 => Some(true),
        1 => Some(false),
        _ => None,
    };

    Ok(BodyControllerSample {
        sample: Sample {
            ts: now_secs(),
            source: "body_controller".into(),
            ..Sample::default()
        },
        user_presence,
    })
}

/// Bundles a `PersistentSession` with the VIN + adapter it was
/// opened for, so the sampler can detect a config change between
/// ticks and recreate the session cleanly. Stored as
/// `Option<SessionHandle>` in main.
pub struct SessionHandle {
    pub session: PersistentSession,
    pub vin: String,
    pub adapter: Option<String>,
}

/// Ensure `handle` is a `PersistentSession` for the given VIN +
/// adapter. Lazily spawns the session on first call, recreates it
/// if EITHER the VIN or the configured adapter changed. The
/// keypair is loaded from the standard /root/.ble path each time
/// the session is created.
pub fn ensure_session_for(
    handle: &mut Option<SessionHandle>,
    vin: &str,
    adapter: Option<&str>,
) -> Result<()> {
    let want_adapter = adapter
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if let Some(existing) = handle {
        if existing.vin == vin && existing.adapter == want_adapter {
            return Ok(());
        }
        info!(
            "PersistentSession config changed (vin {}->{}, adapter {:?}->{:?}); recreating",
            short_vin(&existing.vin),
            short_vin(vin),
            existing.adapter,
            want_adapter
        );
        // Dropping the existing handle closes the mpsc Sender; the
        // background task notices and shuts down naturally.
    }
    let keypair = KeyPair::load(std::path::Path::new("/root/.ble/key_private.pem"))?;
    *handle = Some(SessionHandle {
        session: PersistentSession::start(keypair, vin.to_string(), want_adapter.clone()),
        vin: vin.to_string(),
        adapter: want_adapter,
    });
    Ok(())
}

fn short_vin(vin: &str) -> String {
    if vin.len() >= 7 {
        format!("{}...{}", &vin[..3], &vin[vin.len() - 4..])
    } else {
        vin.to_string()
    }
}
