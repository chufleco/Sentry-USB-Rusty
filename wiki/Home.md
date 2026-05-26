# Sentry USB Wiki

Turn a Raspberry Pi into a smart USB drive for your Tesla's dashcam.
Clips archive themselves. A web UI lets you review, search, and back
them up — no SSH, no config files.

## Install in one line

> **No prebuilt SD card image yet.** The install path is: flash stock **Raspberry Pi OS Lite (64-bit)**, SSH in, and run the one-liner below. A prebuilt image will come later — for now this is the only supported install route. Full step-by-step in [Getting Started](Getting-Started).

On a Pi already running Pi OS:

```bash
sudo -i
curl -fsSL https://raw.githubusercontent.com/Sentry-Six/Sentry-USB-Rusty/main/install-pi.sh | bash
```

Then open `http://sentryusb.local` and the [Setup Wizard](Setup-Wizard-Guide) takes you the rest of the way.

## What you need

| Tier | Boards | Notes |
|------|--------|-------|
| **Recommended** | Raspberry Pi 4B, Raspberry Pi 5 | USB 2.0 OTG — fastest archiving, best web UI responsiveness |
| **Tested** | Raspberry Pi Zero 2 W, Raspberry Pi 3 (A+/B/B+) | USB 2.0 OTG — works fine, slower archive speeds |
| **Community** | Radxa Rock Pi 4C+, Radxa Zero 3W | USB 3.0 OTG Reported working by users, not officially supported |

Plus:
- MicroSD card, **256 GB or larger** recommended
- A **USB 3.0 data cable** (not charge-only) — Pi to your Tesla.
  Use a 3.0 cable even on boards that only support USB 2.0 OTG: 3.0 cables
  deliver more power, which keeps lower-end boards stable.
- WiFi network with internet (for first-time setup and updates)

## Quick Links

| Page | What's on it |
|------|--------------|
| [Getting Started](Getting-Started) | Install in 10 minutes |
| [Setup Wizard Guide](Setup-Wizard-Guide) | Every wizard step explained |
| [Archive Methods](Archive-Methods) | CIFS, rsync, rclone, NFS |
| [Notifications](Notifications) | Push notifications to your phone |
| [Privacy](Privacy) | What we send, when, and why — and how to opt out |
| [Sentry Cloud](Sentry-Cloud) | Encrypted cloud backup (in beta) |
| [Troubleshooting](Troubleshooting) | Things that go wrong |
| [FAQ](FAQ) | Common questions |

## Links

- **Site**: [sentryusb.com](https://sentryusb.com)
- **GitHub**: [Sentry-Six/Sentry-USB-Rusty](https://github.com/Sentry-Six/Sentry-USB-Rusty)
- **Releases**: [Latest](https://github.com/Sentry-Six/Sentry-USB-Rusty/releases/latest)
- **Discord**: [Community chat](https://discord.gg/9QZEzVwdnt)
- **License**: MIT
