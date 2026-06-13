# SentryUSB on the Orange Pi Zero 3W (Allwinner A733 / sun60iw2)

This board needs a **patched binary**. The stock upstream build assumes an
XFS-capable kernel and a Pi-style boot layout; the A733 BSP kernel
(`6.6.98-sun60iw2`) has **no XFS at all** and a different root/initramfs setup,
so stock setup dies on the XFS check and then loops forever in root-shrink.

The fork build fixes:

- **Backing FS:** probes what the kernel can actually mount and picks
  Btrfs (CoW) or ext4 instead of assuming XFS.
- **Root-shrink loop:** rebuilds the initramfs on non-Pi boards so the shrink
  step terminates instead of repeating every reboot.
- **USB gadget:** `otg_role` token is `usb_device` on this SoC (not
  `peripheral`); the gadget now binds and Tesla enumerates it (verified
  in-car: UDC state `configured`, LUN `cam_disk.bin`).
- **WiFi:** the driver leaves `/proc/net/wireless` empty, so SSID, signal
  bars, and exact **dBm** are read via `iw` / `nmcli` fallbacks.
- **Board name:** reports "Orange Pi Zero 3W" instead of `sun60iw2`.

## Fresh install

On a freshly-flashed Orange Pi Zero 3W (1.0.0 Bookworm image), with the
patched binary either hosted at a URL or sitting next to the script:

```bash
# binary from a fork release:
A733_BINARY_URL=https://github.com/chufleco/Sentry-USB-Rusty/releases/download/<tag>/sentryusb-a733 \
  bash install-a733.sh

# …or with ./sentryusb-a733-patched next to the script:
bash install-a733.sh
```

It runs the normal upstream installer, then swaps the patched binary in
**before** you run setup. Open the web UI and run the setup wizard.

**Storage:** SD cards die under constant dashcam writes — use an external SSD
(e.g. Samsung T5). Plug the SSD into the **middle Type-C (USB 3.0 host)**, keep
the Tesla cable on the **corner Type-C**, and set `DATA_DRIVE=/dev/sda` in
setup. That also makes setup skip root-shrink entirely.

## After a SentryUSB OTA / "Update"

An OTA re-fetches the stock binary and breaks the board again. Restore in
~5 seconds with no rebuild (uses the binary stashed at
`/opt/sentryusb/sentryusb-a733-patched`):

```bash
sudo /opt/sentryusb/repatch-a733.sh
```
