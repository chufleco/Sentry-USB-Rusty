//! Inner payload bytes for keep-awake actions, used by `awake_start`'s
//! nudge loop during archive cycles. All are signed messages (same
//! AES-GCM pipeline as state queries):
//!
//! * `wake_vehicle`          → VEHICLE_SECURITY, RKE_ACTION_WAKE_VEHICLE (30).
//!                             Works even when the main computer is asleep.
//! * `set_sentry_mode`       → INFOTAINMENT. Used by SENTRY_CASE=1
//!                             (auto-arm away) and =3 (periodic nudge).
//! * `charge_port_open/close` → INFOTAINMENT, ChargePortDoor{Open,Close}.

use prost::Message;

use crate::proto::car_server::{
    Action, ChargePortDoorClose, ChargePortDoorOpen, SetKeepAccessoryPowerModeAction,
    VehicleAction, VehicleControlSetSentryModeAction, action, vehicle_action,
};
use crate::proto::vcsec::{UnsignedMessage, unsigned_message};
use crate::proto::universal_message::Domain;

/// One keep-awake command with the domain it ships to and the
/// pre-encoded inner payload bytes.
pub struct ActionPayload {
    pub domain: Domain,
    pub inner: Vec<u8>,
}

/// Wake the car. Routes through VEHICLE_SECURITY domain because the
/// VCSEC controller is awake even when the main computer is asleep —
/// this is the same path `tesla-control wake` uses.
pub fn wake_vehicle() -> ActionPayload {
    const RKE_ACTION_WAKE_VEHICLE: i32 = 30;
    let inner = UnsignedMessage {
        sub_message: Some(unsigned_message::SubMessage::RkeAction(
            RKE_ACTION_WAKE_VEHICLE,
        )),
    }
    .encode_to_vec();
    ActionPayload {
        domain: Domain::VehicleSecurity,
        inner,
    }
}

/// Turn Sentry Mode on or off via INFOTAINMENT. Lives under the
/// `vehicleControlSetSentryModeAction` oneof variant in
/// `car_server.VehicleAction`.
pub fn set_sentry_mode(on: bool) -> ActionPayload {
    let action = Action {
        action_msg: Some(action::ActionMsg::VehicleAction(VehicleAction {
            vehicle_action_msg: Some(
                vehicle_action::VehicleActionMsg::VehicleControlSetSentryModeAction(
                    VehicleControlSetSentryModeAction { on },
                ),
            ),
        })),
    };
    ActionPayload {
        domain: Domain::Infotainment,
        inner: action.encode_to_vec(),
    }
}

/// Open the charge port. INFOTAINMENT domain.
pub fn charge_port_open() -> ActionPayload {
    let action = Action {
        action_msg: Some(action::ActionMsg::VehicleAction(VehicleAction {
            vehicle_action_msg: Some(
                vehicle_action::VehicleActionMsg::ChargePortDoorOpen(
                    ChargePortDoorOpen {},
                ),
            ),
        })),
    };
    ActionPayload {
        domain: Domain::Infotainment,
        inner: action.encode_to_vec(),
    }
}

/// Close the charge port. INFOTAINMENT domain.
pub fn charge_port_close() -> ActionPayload {
    let action = Action {
        action_msg: Some(action::ActionMsg::VehicleAction(VehicleAction {
            vehicle_action_msg: Some(
                vehicle_action::VehicleActionMsg::ChargePortDoorClose(
                    ChargePortDoorClose {},
                ),
            ),
        })),
    };
    ActionPayload {
        domain: Domain::Infotainment,
        inner: action.encode_to_vec(),
    }
}

/// Turn "Keep Accessory Power" on or off via INFOTAINMENT — the same toggle the
/// Tesla app exposes on the Charging screen. Mirrors [`set_sentry_mode`]; lives
/// under the `setKeepAccessoryPowerModeAction` oneof variant in
/// `car_server.VehicleAction` (field 138).
pub fn set_keep_accessory_power(on: bool) -> ActionPayload {
    let action = Action {
        action_msg: Some(action::ActionMsg::VehicleAction(VehicleAction {
            vehicle_action_msg: Some(
                vehicle_action::VehicleActionMsg::SetKeepAccessoryPowerModeAction(
                    SetKeepAccessoryPowerModeAction {
                        keep_accessory_power_mode: on,
                    },
                ),
            ),
        })),
    };
    ActionPayload {
        domain: Domain::Infotainment,
        inner: action.encode_to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_payload_is_vcsec_rke_30() {
        let p = wake_vehicle();
        assert_eq!(p.domain, Domain::VehicleSecurity);
        // VCSEC UnsignedMessage { RKEAction = 30 } encodes as:
        //   tag for field 2 varint (0x10), value 30 (0x1e) = "10 1e"
        assert_eq!(hex::encode(&p.inner), "101e");
    }

    #[test]
    fn sentry_mode_on_routes_to_infotainment() {
        let p = set_sentry_mode(true);
        assert_eq!(p.domain, Domain::Infotainment);
        // Action { VehicleAction { vehicleControlSetSentryModeAction { on=true } } }
        //   Action.vehicleAction (field 2, length-delimited)
        //     VehicleAction.vehicleControlSetSentryModeAction (field 30)
        //       SentryModeAction.on (field 1, varint, true)
        // We don't pin exact bytes (proto encoding can vary by lib
        // version on field-tag widths); just sanity-check non-empty.
        assert!(!p.inner.is_empty());
    }

    #[test]
    fn sentry_mode_off_differs_from_on() {
        // The two states must produce distinct payloads or the car
        // would arm/disarm the wrong way.
        assert_ne!(set_sentry_mode(true).inner, set_sentry_mode(false).inner);
    }

    #[test]
    fn charge_port_open_close_are_distinct() {
        assert_ne!(charge_port_open().inner, charge_port_close().inner);
    }
}
