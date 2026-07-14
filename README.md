# loudeq — one-click Loudness Equalization toggle for Windows

[![CI](https://github.com/ardenden/loudeq/actions/workflows/ci.yml/badge.svg)](https://github.com/ardenden/loudeq/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/ardenden/loudeq)](LICENSE)

**Toggle Windows Loudness Equalization instantly** — from a system tray icon, a pinned taskbar button, or the command line — instead of digging through the Sound control panel every time.

Loudness Equalization (a.k.a. volume leveling / loudness equalizer) is the built-in Windows audio enhancement that evens out volume differences: quiet dialogue gets louder, loud peaks get tamed. It's great for movies at night, quiet speech, or weak USB speakers — but Windows buries the checkbox in *Sound Control Panel → device Properties → Enhancements*, and it won't stay put on some devices. loudeq turns it into one click.

- Works on **Windows 10 and 11**, on any playback device with audio enhancements — including USB speakers/headphones that the **Realtek Audio Console** and other vendor tools ignore
- Applies **live** to playing audio — no audio-service restart, no admin prompt
- Tiny (~400 KB), **no runtime required**, fully **offline** (no network, no data collection), open source
- Available on the **Microsoft Store** (search "Loudness Equalization Toggle") for a signed one-click install, or build from source below

## Usage

```
loudeq              # toggle Loudness EQ on the default playback device
loudeq on           # enable
loudeq off          # disable
loudeq status       # show current state
loudeq list         # list active playback devices (+ state)
loudeq setup        # only needed if the fallback path asks for UAC (see below)

Options:
  -d, --device <NAME>   target a device by name substring instead of the default
  --no-restart          fallback path only: write the setting but don't restart
                        the audio service
```

Changes are applied **live** through the audio policy service — same as clicking Apply in the Sound control panel: no admin rights, no UAC, no audio interruption.

If that path ever fails (`loudeq` tells you when it does), it falls back to writing the registry directly and restarting the Windows Audio service, which needs administrator rights — it requests UAC elevation by itself. For a UAC-free fallback too, run `loudeq setup` once: it grants interactive users permission to start/stop the *Windows Audio* service. `loudeq setup --remove` restores the original permissions (a backup of the original security descriptor is kept under `HKCU\Software\loudeq`).

## How it works

The Loudness Equalization checkbox is the endpoint FX property `{fc52a749-4be9-4510-896e-966ba6525980},3` (a `VT_BOOL` PROPVARIANT). It lives in **two** places under
`HKLM\...\MMDevices\Audio\Render\{endpoint-guid}\FxProperties`:

1. the legacy flat value directly in `FxProperties`, and
2. on Windows 11, a per-effect-instance user store at `FxProperties\{instance-guid}\User` — **this is the one the effects engine and the Enhancements dialog actually honor**. Writing only the flat value changes nothing audible.

`loudeq` writes both: the flat value through `IPolicyConfig::SetPropertyValue` (the undocumented-but-stable audio policy interface, unchanged since Windows 7), and each instance user store through the documented Windows 11 `IAudioSystemEffectsPropertyStore` API. The engine applies the change live — mid-stream, no elevation, no service restart (the effect's own gain then ramps over a few seconds, by design).

If the live path fails, `loudeq` falls back to writing the registry directly and restarting the *Windows Audio* service.

`loudeq meter` samples the endpoint's peak output level for 5 seconds — useful to verify the effect objectively: play quiet/dynamic audio and compare readings with the effect off vs. on.

The default playback device is resolved with the MMDevice COM API (`IMMDeviceEnumerator::GetDefaultAudioEndpoint`); device enumeration and state reads are plain registry access.

## Build

```
cargo build --release
```

The exe lands in `target\release\loudeq.exe`. Copy it anywhere and/or make a shortcut.

> **Smart App Control:** if SAC is enabled, Windows blocks locally-built unsigned executables (including Cargo build scripts), so both building and running this tool require SAC to be off.

## Tray app (loudeq-tray.exe)

`loudeq-tray.exe` (also started by `loudeq tray`) puts an icon in the notification area that always shows the current state of the default playback device:

- **green dot** = Loudness EQ ON, **gray ring** = OFF
- **left-click** the icon = toggle (with a toast notification)
- **right-click** = menu: status, Toggle, Start with Windows, Exit
- launching the exe again while it's running relays a **toggle** to the running instance — so a shortcut **pinned to the taskbar** acts as a toggle button: every click flips the setting and the tray icon updates.

To pin it: find **LoudEQ** in the Start Menu, right-click → Pin to taskbar. Drag the tray icon out of the overflow flyout onto the taskbar clock area to keep it always visible. Enable *Start with Windows* from the right-click menu to have it from logon.
