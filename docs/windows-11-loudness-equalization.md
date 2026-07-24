# Where did Loudness Equalization go in Windows 11 (and how to get it back)

**Short answer:** it wasn't removed. Loudness Equalization still exists in Windows 11 and
still works — Microsoft just took away the checkbox on most systems. The setting lives on
the audio device itself, and anything that can write that setting can still turn it on.

This page explains why the Enhancements tab disappeared, how to tell whether your device
actually supports the feature, and every way to switch it back on — including doing it by
hand, with no extra software.

---

## What Loudness Equalization actually is

It's a built-in Windows audio effect that evens out volume differences: quiet dialogue gets
lifted, loud peaks get tamed. People mainly want it for:

- Watching films at night without riding the volume knob
- Hearing quiet speech in videos and calls
- Getting usable volume out of weak laptop or USB speakers

It's sometimes called *volume levelling* or *loudness equalizer*. It is **not** an equalizer
in the tone-control sense — it doesn't change bass or treble.

## Why the Enhancements tab vanished

Historically you'd find it at *Sound Control Panel → device → Properties → Enhancements*. On
Windows 11 that tab is missing on a lot of machines. A few separate things cause this:

- **Windows 11 reworked the audio settings UI.** Much of the old Enhancements surface was
  dropped or moved into per-device settings, and what's exposed now depends on the driver.
- **The generic Microsoft audio driver doesn't show it.** If Windows replaced a vendor driver
  with its own "High Definition Audio Device" driver, the tab often disappears with it.
- **Vendors moved it into their own apps** — Realtek Audio Console, HP Audio Control,
  Bang & Olufsen, Dolby, and so on.

The important part: **this is a UI change, not a removal.** The effect is implemented by an
audio processing object that Windows still loads, and it still honours the setting. There's
just no longer a built-in way to reach it on many devices.

That's also why the common suggestion — *roll back to an older audio driver* — tends not to
stick. Windows Update reinstalls the newer driver, and you're back where you started.

## First: check it isn't just switched off globally

Before anything else, rule this out — it catches people out constantly.

Windows has a **master switch that disables all audio enhancements** for a device. If that's
off, Loudness Equalization does nothing even when it's enabled, because the whole effects
chain is bypassed.

- **Settings → System → Sound →** click your output device **→ Audio enhancements**
- If it's set to **Off**, switch it to **Device Default**

Then try enabling Loudness Equalization again.

## How to tell whether your device supports it at all

Not every device can do this. Loudness Equalization is provided by the audio effects that
ship with your device's driver; if a device has none, there's nothing to switch on and no
tool can conjure it.

A quick check in the registry (read-only, nothing to change):

```
HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render
```

Each subkey is one playback endpoint. If a device has an **`FxProperties`** subkey, it
supports audio effects. If it doesn't, that device can't do Loudness Equalization at all.

Bluetooth headsets and some HDMI outputs commonly have no effects. Most built-in speakers,
USB speakers and headsets do.

## Ways to turn it back on

### 1. Check the classic Sound control panel

On some systems the old tab is still reachable even though it's hidden elsewhere. Press
<kbd>Win</kbd>+<kbd>R</kbd>, run `mmsys.cpl`, pick your device, click **Properties**, and look
for an **Enhancements** tab. If it's there, tick *Loudness Equalization* and click Apply.

Worth 30 seconds before trying anything else.

### 2. Your manufacturer's audio app

If you have Realtek, HP, Dell, Lenovo, Bang & Olufsen or Dolby audio hardware, the vendor app
may expose it (often under a different name like "volume levelling" or "smart volume").

**This only helps if you actually have that hardware and app.** It's the advice you'll see
most often online, and it's useless for generic USB speakers and headsets, which have no
vendor console at all.

### 3. Set it directly, by hand

The setting is an endpoint property, so you can write it yourself. The value is:

```
{fc52a749-4be9-4510-896e-966ba6525980},3     (a VT_BOOL PROPVARIANT)
```

under your device's key in:

```
HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\{endpoint}\FxProperties
```

Two things make this fiddlier than it looks:

- On **Windows 11** the value also lives in a per-effect-instance store at
  `FxProperties\{instance}\User`, and that's the copy the audio engine actually reads. Setting
  only the flat value often appears to work but changes nothing.
- Editing the registry directly needs administrator rights, and the change doesn't apply to
  audio that's already playing until the audio service restarts.

Doable, but it's the least pleasant option.

### 4. Use a tool that does it for you

I wrote a small free, open-source utility for exactly this problem:
**[loudeq](https://github.com/ardenden/loudeq)**.

It writes the setting straight to the audio endpoint, so it doesn't need the Enhancements tab
or any vendor app, and it handles the Windows 11 per-instance store correctly.

- Toggle from a tray icon, a taskbar button, or the command line
- Applies **live** to audio that's already playing — no restart, no UAC prompt
- Works on Windows 10 and 11, including USB devices vendor tools ignore
- ~400 KB, no runtime, fully offline (no network, no telemetry), open source

[**Get it on the Microsoft Store**](https://apps.microsoft.com/detail/9P5P88XR7NB8?cid=docs-seo)
· [Source on GitHub](https://github.com/ardenden/loudeq)

*(Disclosure: I'm the author. It's free and the source is public — and options 1–3 above work
without it if you'd rather not install anything.)*

## Why it keeps turning itself off

A few people find it won't stay enabled. Usual culprits:

- **A driver update reset it.** Audio driver updates frequently clear effect settings.
- **The device was re-enumerated.** Plugging a USB device into a different port can create a
  new audio endpoint, which starts with default settings — the old ones are still saved
  against the old endpoint.
- **The master "Audio enhancements" switch got turned off**, silently bypassing everything
  (see above).

## Related settings people look for

- **Bass Boost / Virtual Surround / Room Correction** — these were also on the old
  Enhancements tab, but unlike Loudness Equalization they have **no portable setting**: they're
  either vendor-specific or not exposed at all. Your manufacturer's audio app is genuinely the
  only route for these.
- **Mono audio** — *Settings → Accessibility → Audio → Mono audio*. Still fully supported and
  easy to reach.
- **Release time** — an undocumented Loudness Equalization parameter (roughly, how quickly the
  effect reacts) that was never in the UI at all.

---

*Maintained alongside [loudeq](https://github.com/ardenden/loudeq). Corrections and additions
welcome — open an issue.*
