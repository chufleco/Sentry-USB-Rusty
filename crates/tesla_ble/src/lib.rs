//! Bluez-aware Tesla BLE client.

pub mod body_controller;
pub mod gatt;
pub mod local_name;
pub mod scan;
pub mod transport;

pub mod proto {
    pub mod universal_message {
        include!(concat!(env!("OUT_DIR"), "/universal_message.rs"));
    }
    pub mod vcsec {
        include!(concat!(env!("OUT_DIR"), "/vcsec.rs"));
    }
    pub mod signatures {
        include!(concat!(env!("OUT_DIR"), "/signatures.rs"));
    }
    pub mod keys {
        include!(concat!(env!("OUT_DIR"), "/keys.rs"));
    }
    pub mod errors {
        include!(concat!(env!("OUT_DIR"), "/errors.rs"));
    }
    pub mod car_server {
        include!(concat!(env!("OUT_DIR"), "/car_server.rs"));
    }
}

/// Tesla's BLE GATT service + characteristics UUIDs.
pub mod uuids {
    use uuid::Uuid;

    pub const VEHICLE_SERVICE: Uuid =
        Uuid::from_u128(0x00000211_b2d1_43f0_9b88_960cebf8b91e);
    pub const TO_VEHICLE: Uuid =
        Uuid::from_u128(0x00000212_b2d1_43f0_9b88_960cebf8b91e);
    pub const FROM_VEHICLE: Uuid =
        Uuid::from_u128(0x00000213_b2d1_43f0_9b88_960cebf8b91e);
}
