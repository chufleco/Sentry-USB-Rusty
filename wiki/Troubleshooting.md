# Troubleshooting

Most install problems fall into a few buckets. If your issue isn't here, ping **[Discord](https://discord.gg/9QZEzVwdnt)** — answers come fast and we'll add common ones to this page over time.

## Can't reach http://sentryusb.local

The Pi is up but its hostname isn't resolving on your network.

**Try:**
1. Open your router's admin page and confirm the Pi shows up as `sentryusb`. Use the IP directly in your browser instead (e.g., `http://192.168.1.47`).
2. On Windows, install **[Bonjour Print Services](https://support.apple.com/kb/DL999)** — Windows doesn't ship with mDNS by default.
3. Some corporate / mesh / guest WiFi networks block mDNS broadcasts. Try a different network or use the IP.

## Tesla dashcam icon never appears

The car isn't seeing the Pi as a USB drive.

**Try:**
1. **Check the cable** — make sure it's a **USB data cable**, not charge-only. The cheap ones bundled with most USB chargers are charge-only.
2. Plug the Pi into a different USB port on the Tesla. Newer cars: glovebox port. Older cars: front console ports.
3. Power-cycle the Pi (unplug, wait 5 seconds, plug back in).
4. SSH into the Pi and check the USB gadget service: `systemctl status sentryusb-gadget`.

## BLE Keep Awake stops working / can't pair

The Bluetooth keep-awake holds your Tesla awake by talking to the car over BLE. If pairing fails or the connection drops, **Logs → Bluetooth** has a live diagnostic dump.

**Try:**
1. **Logs → Bluetooth** — shows which adapter the daemon picked, connection state, sample-DB counts, and the recent sampler journal lines.
2. Click **Download Bluetooth Bundle** on that tab to grab a ZIP (config, journals, BlueZ state) for sharing on Discord — saves a lot of back-and-forth.
3. Re-pair from **Settings → Bluetooth Keep Awake → Pair Vehicle**. The pair handler power-cycles the adapter and verifies the link before declaring success.
4. Make sure your Tesla account in the car has BLE pairing enabled (Controls → Locks → PIN to Drive isn't required, but the car must accept new BLE peers).

## CIFS/SMB connection fails

**Try:**
1. Re-run the [Setup Wizard](Setup-Wizard-Guide) → Archive step → fill in **CIFS Version** as `2.0` or `1.0` (some older NAS devices reject SMB3 negotiation).
2. Check your password — special characters sometimes need escaping; try a simpler password as a test.
3. From the Pi, try mounting manually:
   ```bash
   sudo mount -t cifs //<server>/<share> /mnt -o username=<user>
   ```
   If that errors, the error message tells you exactly what's wrong.

## Still stuck?

- **[Discord](https://discord.gg/9QZEzVwdnt)** — fastest help, real humans
- **[Open an issue](https://github.com/Sentry-Six/Sentry-USB-Rusty/issues)** — for reproducible bugs
- Include: Pi model, what step you're stuck on, exact error message, and the output of `journalctl -u sentryusb -n 100` if relevant.
