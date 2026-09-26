//! Routing car actions through the ESP32-C6 supervisor's local API. Shared by
//! the telemetry daemon (while the C6 owns the car) and sentryusb-ble-action
//! (daemon down while the C6 holds a live grant).

use std::time::Duration;

/// Default C6 supervisor API address (loopback; matches its `api_bind`).
pub const DEFAULT_SUPERVISOR_API: &str = "127.0.0.1:8787";

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
}
