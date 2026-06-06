//! Master experimental opt-in gate.
//!
//! A single place that answers "is the experimental command surface
//! turned on?" so every consumer agrees on what "on" means. The value
//! lives in the on-disk config (`SENTRYUSB_EXPERIMENTAL`) and is read
//! fresh on each call — toggling the flag takes effect without a daemon
//! restart, and these checks are low-traffic enough that re-parsing the
//! small config file per request is free. When the file is missing or
//! the key is unset the answer is `false`, so a normal install behaves
//! exactly as it did before any experimental code existed.

/// Whether `SENTRYUSB_EXPERIMENTAL` is set to an affirmative value
/// (`yes` / `true` / `1`, case-insensitive). Read fresh per call.
pub(crate) fn experimental_enabled() -> bool {
    let path = sentryusb_config::find_config_path();
    match sentryusb_config::parse_file(path) {
        Ok((active, commented)) => {
            match sentryusb_config::get_config_value(&active, &commented, "SENTRYUSB_EXPERIMENTAL") {
                Some(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "yes" | "true" | "1"),
                None => false,
            }
        }
        Err(_) => false,
    }
}
