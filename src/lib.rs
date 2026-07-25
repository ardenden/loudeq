//! Core logic for toggling Windows "Loudness Equalization", shared by the
//! loudeq CLI and the loudeq-tray app.
//!
//! The Loudness Equalization flag is endpoint FX property
//! "{fc52a749-4be9-4510-896e-966ba6525980},3" (VT_BOOL). It lives in two
//! places under HKLM\...\MMDevices\Audio\Render\{endpoint}\FxProperties:
//! the legacy flat value, and (Windows 11) per-effect-instance user stores in
//! FxProperties\{instance}\User — the latter is what the effects engine and
//! the Enhancements dialog actually honor. Both are written here.

use std::io::{self, ErrorKind};

use windows::core::{w, IUnknown, IUnknown_Vtbl, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, BOOL, ERROR_ACCESS_DENIED, ERROR_SERVICE_ALREADY_RUNNING,
    ERROR_SERVICE_NOT_ACTIVE, HANDLE, VARIANT_BOOL,
};
use windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation;
use windows::Win32::Media::Audio::{
    eConsole, eMultimedia, eRender, ERole, IAudioSystemEffectsPropertyStore, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVEFORMATEX,
};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
    STGM_READWRITE,
};
use windows::Win32::System::Services::{
    CloseServiceHandle, ControlService, OpenSCManagerW, OpenServiceW, QueryServiceStatus,
    StartServiceW, SC_MANAGER_CONNECT, SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS,
    SERVICE_RUNNING, SERVICE_START, SERVICE_STATUS, SERVICE_STOP, SERVICE_STOPPED,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::Variant::{VT_BOOL, VT_CLSID, VT_UI4};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
use winreg::enums::{RegType, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE};
use winreg::{RegKey, RegValue};

pub const RENDER_PATH: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render";
/// PKEY for the Loudness Equalization enable flag (FxProperties).
pub const LOUDNESS_VALUE: &str = "{fc52a749-4be9-4510-896e-966ba6525980},3";
/// PKEY_AudioEndpoint_Disable_SysFx: 1 = all enhancements disabled (FxProperties).
pub const DISABLE_SYSFX_VALUE: &str = "{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5";
/// PKEY_Device_DeviceDesc, e.g. "Speakers" (Properties).
pub const ENDPOINT_NAME_VALUE: &str = "{a45c254e-df1c-4efd-8020-67d146a850e0},2";
/// PKEY_DeviceInterface_FriendlyName, e.g. "Philips SPA6109" (Properties).
pub const DEVICE_DESC_VALUE: &str = "{b3f8fa53-0004-438e-9003-51a46e139bfc},6";

// Microsoft-sysfx enhancement keys (the same on every generic-driver device,
// Provider: Microsoft) — recovered by diffing the classic Enhancements tab.
// Unlike Loudness EQ's VT_BOOL, these are VT_I4 with effect-specific values.
/// Bass Boost enable flag; VT_I4, on=2 off=0 (FxProperties).
pub const BASS_BOOST_VALUE: &str = "{1864a4e0-efc1-45e6-a675-5786cbf3b9f0},4";
/// Bass Boost cutoff frequency in raw Hz; VT_I4 (dialog offers 50-600, step 25).
pub const BASS_BOOST_FREQ_VALUE: &str = "{61e8acb9-f04f-4f40-a65f-8f49fab3ba10},4";
/// Bass Boost level as an index; VT_I4, dB = index*3 + 3 (i.e. 3-24 dB).
pub const BASS_BOOST_LEVEL_VALUE: &str = "{ae7f0b2a-96fc-493a-9247-a019f1f701e1},3";
/// Virtual Surround enable flag; VT_I4, on=4 off=0 (FxProperties).
pub const VIRTUAL_SURROUND_VALUE: &str = "{1b5c2483-0839-4523-ba87-95f89d27bd8c},3";
/// PKEY_AudioEndpoint_FormFactor (Properties): 1=Speakers, 3=Headphones,
/// 5=Headset, 9=HDMI, etc. Used to tell whether Virtual Surround applies.
pub const FORM_FACTOR_VALUE: &str = "{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},0";

/// The Loudness Equalization enable flag as a PROPERTYKEY (same property as
/// LOUDNESS_VALUE, for the property-store paths).
const PKEY_LOUDNESS_EQ: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xfc52a749_4be9_4510_896e_966ba6525980),
    pid: 3,
};
/// PKEY_AudioEndpoint_Disable_SysFx as a PROPERTYKEY.
const PKEY_DISABLE_SYSFX: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x1da5d803_d492_4edd_8c23_e0c0ffee7f0e),
    pid: 5,
};

/// Loudness EQ release-time parameter (2-7), not exposed by the standard
/// Enhancements checkbox. Key discovered by reading the open-source
/// competitor LEQControlPanel's registry-access script rather than blind
/// registry diffing: https://github.com/ArtIsWar/LEQControlPanel
/// (src/scripts/Modules/LEQ-Engine.ps1). It writes both pid 3 and pid 1599
/// with identical data; reason for the second PID is undocumented upstream
/// too, but matching a known-working implementation removes the guesswork.
const PKEY_RELEASE_TIME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9c00eeed_edce_4cd8_ae08_cb05e8ef57a0),
    pid: 3,
};
const PKEY_RELEASE_TIME_ALT: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9c00eeed_edce_4cd8_ae08_cb05e8ef57a0),
    pid: 1599,
};

/// CLSID of the audio policy configuration client (CPolicyConfigClient).
const CPOLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

/// Undocumented but long-stable audio policy interface (the one the Sound
/// control panel and tools like SoundSwitch use). Writing an FX property
/// through it makes the audio engine apply the change live — no service
/// restart, no admin rights. Method order must match the known vtable.
#[windows::core::interface("f8679f50-850a-41cf-9c72-430f290290c8")]
unsafe trait IPolicyConfig: IUnknown {
    unsafe fn get_mix_format(&self, device: PCWSTR, format: *mut *mut WAVEFORMATEX) -> HRESULT;
    unsafe fn get_device_format(
        &self,
        device: PCWSTR,
        default: BOOL,
        format: *mut *mut WAVEFORMATEX,
    ) -> HRESULT;
    unsafe fn reset_device_format(&self, device: PCWSTR) -> HRESULT;
    unsafe fn set_device_format(
        &self,
        device: PCWSTR,
        endpoint_format: *mut WAVEFORMATEX,
        mix_format: *mut WAVEFORMATEX,
    ) -> HRESULT;
    unsafe fn get_processing_period(
        &self,
        device: PCWSTR,
        default: BOOL,
        default_period: *mut i64,
        min_period: *mut i64,
    ) -> HRESULT;
    unsafe fn set_processing_period(&self, device: PCWSTR, period: *mut i64) -> HRESULT;
    unsafe fn get_share_mode(&self, device: PCWSTR, mode: *mut i32) -> HRESULT;
    unsafe fn set_share_mode(&self, device: PCWSTR, mode: *mut i32) -> HRESULT;
    unsafe fn get_property_value(
        &self,
        device: PCWSTR,
        fx_store: BOOL,
        key: *const PROPERTYKEY,
        value: *mut PROPVARIANT,
    ) -> HRESULT;
    unsafe fn set_property_value(
        &self,
        device: PCWSTR,
        fx_store: BOOL,
        key: *const PROPERTYKEY,
        value: *mut PROPVARIANT,
    ) -> HRESULT;
    unsafe fn set_default_endpoint(&self, device: PCWSTR, role: ERole) -> HRESULT;
    unsafe fn set_endpoint_visibility(&self, device: PCWSTR, visible: BOOL) -> HRESULT;
}

/// A *different* interface on the same CPolicyConfigClient object (CLSID
/// 870af99c). The widely-known f8679f50 IID everyone calls "IPolicyConfig" is
/// really the Win7-era layout, whose vtable has no mono method. The mono-audio
/// accessibility setter lives on this sibling interface instead — its IID and
/// the slot-21 position were recovered by disassembling AudioSes.dll's ATL
/// interface map and the CComObject<CPolicyConfigClient> vtable (the f8679f50
/// vtable diverges after the classic 12 methods; this one continues with the
/// newer setters). Slots 3-20 are placeholders we never call — only their
/// count matters, to land SetAccessibilityAudioMonoMixState at vtable slot 21.
/// Inherently Windows-version-sensitive, so set_mono_audio verifies the write
/// actually took effect and errors out rather than trusting the slot blindly.
#[windows::core::interface("e8478600-a74b-4b3a-a96b-1fc3e796fc46")]
unsafe trait IPolicyConfigMono: IUnknown {
    unsafe fn slot3(&self) -> HRESULT;
    unsafe fn slot4(&self) -> HRESULT;
    unsafe fn slot5(&self) -> HRESULT;
    unsafe fn slot6(&self) -> HRESULT;
    unsafe fn slot7(&self) -> HRESULT;
    unsafe fn slot8(&self) -> HRESULT;
    unsafe fn slot9(&self) -> HRESULT;
    unsafe fn slot10(&self) -> HRESULT;
    unsafe fn slot11(&self) -> HRESULT;
    unsafe fn slot12(&self) -> HRESULT;
    unsafe fn slot13(&self) -> HRESULT;
    unsafe fn slot14(&self) -> HRESULT;
    unsafe fn slot15(&self) -> HRESULT;
    unsafe fn slot16(&self) -> HRESULT;
    unsafe fn slot17(&self) -> HRESULT;
    unsafe fn slot18(&self) -> HRESULT;
    unsafe fn slot19(&self) -> HRESULT;
    unsafe fn slot20(&self) -> HRESULT;
    unsafe fn set_accessibility_mono_mix(&self, state: i32) -> HRESULT;
    unsafe fn get_accessibility_mono_mix(&self, state: *mut i32) -> HRESULT;
}

fn propvariant_bool(v: bool) -> PROPVARIANT {
    PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    boolVal: VARIANT_BOOL(if v { -1 } else { 0 }),
                },
            }),
        },
    }
}

fn propvariant_u32(v: u32) -> PROPVARIANT {
    PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_UI4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 { ulVal: v },
            }),
        },
    }
}

/// VT_I4 (signed 32-bit), the type the release-time property uses — distinct
/// from propvariant_u32's VT_UI4, matched to the competitor's known-working
/// wire format rather than assumed equivalent.
fn propvariant_i32(v: i32) -> PROPVARIANT {
    PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: windows::Win32::System::Variant::VT_I4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 { lVal: v },
            }),
        },
    }
}

fn propvariant_clsid(guid: *const GUID) -> PROPVARIANT {
    PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_CLSID,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    puuid: guid as *mut GUID,
                },
            }),
        },
    }
}

/// Make an endpoint the default playback device — the same action as the
/// Sound control panel's "Set Default Device" button. Sets both the eConsole
/// and eMultimedia roles (general playback + apps); eCommunications (the
/// separate "Default Communication Device") is deliberately left alone,
/// since users often want a different device for that (e.g. a headset mic
/// while speakers stay default for everything else).
pub fn set_default_device(full_id: &str) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let policy: IPolicyConfig = CoCreateInstance(&CPOLICY_CONFIG_CLIENT, None, CLSCTX_ALL)?;
        let idw: Vec<u16> = full_id.encode_utf16().chain(Some(0)).collect();
        let id = PCWSTR(idw.as_ptr());
        policy.set_default_endpoint(id, eConsole).ok()?;
        policy.set_default_endpoint(id, eMultimedia).ok()
    }
}

pub fn fx_properties_path(guid: &str) -> String {
    format!(r"{RENDER_PATH}\{guid}\FxProperties")
}

/// Windows 11 keeps per-effect-instance settings in FxProperties\{instance}\User
/// subkeys; the effects engine and the Enhancements dialog read those, not the
/// legacy flat value. Returns the instance GUIDs for an endpoint.
pub fn fx_instance_guids(guid: &str) -> Vec<String> {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(fx_properties_path(guid), KEY_READ)
        .map(|fx| fx.enum_keys().flatten().collect())
        .unwrap_or_default()
}

/// Set Loudness Equalization the way the Sound control panel does: through
/// the audio policy service (legacy flat value) AND through each effect
/// instance's user property store (what the Win11 engine actually honors).
/// Applied live by the engine, persisted by the service, no admin needed.
/// Returns the number of instance user stores written.
pub fn apply_loudness_live(
    full_id: &str,
    enable: bool,
    sysfx_disabled: bool,
    instances: &[String],
) -> windows::core::Result<usize> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let policy: IPolicyConfig = CoCreateInstance(&CPOLICY_CONFIG_CLIENT, None, CLSCTX_ALL)?;
        let idw: Vec<u16> = full_id.encode_utf16().chain(Some(0)).collect();
        let id = PCWSTR(idw.as_ptr());

        // Loudness EQ has no effect while "disable all enhancements" is set.
        if enable && sysfx_disabled {
            let mut pv = propvariant_u32(0);
            policy
                .set_property_value(id, BOOL(1), &PKEY_DISABLE_SYSFX, &mut pv)
                .ok()?;
        }
        let mut pv = propvariant_bool(enable);
        policy
            .set_property_value(id, BOOL(1), &PKEY_LOUDNESS_EQ, &mut pv)
            .ok()?;

        // Per-instance user stores (Windows 11). Failures on individual
        // instances are fine — not every instance belongs to the sysfx APO.
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDevice(id)?;
        let mut wrote = 0;
        for inst in instances {
            let inst_guid = GUID::from(inst.trim_matches(|c| c == '{' || c == '}'));
            let params = propvariant_clsid(&inst_guid);
            let Ok(store) =
                device.Activate::<IAudioSystemEffectsPropertyStore>(CLSCTX_ALL, Some(&params))
            else {
                continue;
            };
            let Ok(user) = store.OpenUserPropertyStore(STGM_READWRITE.0) else {
                continue;
            };
            let pv = propvariant_bool(enable);
            if user.SetValue(&PKEY_LOUDNESS_EQ, &pv).is_ok() {
                let _ = user.Commit();
                wrote += 1;
            }
        }
        Ok(wrote)
    }
}

/// Set an arbitrary VT_I4 FX property, using the same dual-write path as
/// Loudness EQ (flat value via the audio policy service + each effect
/// instance's per-`\User` store, which is what the Win11 engine actually
/// honours). Applied live, no admin. Returns the number of instance stores
/// written.
///
/// Used for the Microsoft-sysfx enhancement enable flags, which — unlike
/// Loudness EQ's VT_BOOL — are VT_I4 with an effect-specific "on" value.
/// Keys were recovered by diffing the classic Enhancements tab (Provider:
/// Microsoft, so the same on every generic-driver device):
///   Bass Boost        `{1864a4e0-efc1-45e6-a675-5786cbf3b9f0}`,4  on=2 off=0
///   Virtual Surround  `{1b5c2483-0839-4523-ba87-95f89d27bd8c}`,3  on=4 off=0
pub fn set_fx_i32(
    full_id: &str,
    fmtid: u128,
    pid: u32,
    value: i32,
    instances: &[String],
) -> windows::core::Result<usize> {
    let key = PROPERTYKEY {
        fmtid: GUID::from_u128(fmtid),
        pid,
    };
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let policy: IPolicyConfig = CoCreateInstance(&CPOLICY_CONFIG_CLIENT, None, CLSCTX_ALL)?;
        let idw: Vec<u16> = full_id.encode_utf16().chain(Some(0)).collect();
        let id = PCWSTR(idw.as_ptr());

        let mut pv = propvariant_i32(value);
        policy.set_property_value(id, BOOL(1), &key, &mut pv).ok()?;

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDevice(id)?;
        let mut wrote = 0;
        for inst in instances {
            let inst_guid = GUID::from(inst.trim_matches(|c| c == '{' || c == '}'));
            let params = propvariant_clsid(&inst_guid);
            let Ok(store) =
                device.Activate::<IAudioSystemEffectsPropertyStore>(CLSCTX_ALL, Some(&params))
            else {
                continue;
            };
            let Ok(user) = store.OpenUserPropertyStore(STGM_READWRITE.0) else {
                continue;
            };
            let pv = propvariant_i32(value);
            if user.SetValue(&key, &pv).is_ok() {
                let _ = user.Commit();
                wrote += 1;
            }
        }
        Ok(wrote)
    }
}

/// Bass Boost on/off — writes the enable flag (on=2, off=0), applied live.
pub fn set_bass_boost(
    full_id: &str,
    enable: bool,
    instances: &[String],
) -> windows::core::Result<usize> {
    set_fx_i32(
        full_id,
        0x1864a4e0_efc1_45e6_a675_5786cbf3b9f0,
        4,
        if enable { 2 } else { 0 },
        instances,
    )
}

/// Bass Boost cutoff frequency in Hz (dialog range 50-600, in 25 Hz steps;
/// stored as the raw Hz value).
pub fn set_bass_boost_freq(
    full_id: &str,
    hz: i32,
    instances: &[String],
) -> windows::core::Result<usize> {
    set_fx_i32(
        full_id,
        0x61e8acb9_f04f_4f40_a65f_8f49fab3ba10,
        4,
        hz,
        instances,
    )
}

/// Bass Boost level in dB (3, 6, … 24). Stored as an index: `dB/3 - 1`
/// (verified against the dropdown: 12 dB→3, 24 dB→7).
pub fn set_bass_boost_level(
    full_id: &str,
    db: i32,
    instances: &[String],
) -> windows::core::Result<usize> {
    set_fx_i32(
        full_id,
        0xae7f0b2a_96fc_493a_9247_a019f1f701e1,
        3,
        db / 3 - 1,
        instances,
    )
}

/// Virtual Surround on/off — writes the enable flag (on=4, off=0).
pub fn set_virtual_surround(
    full_id: &str,
    enable: bool,
    instances: &[String],
) -> windows::core::Result<usize> {
    set_fx_i32(
        full_id,
        0x1b5c2483_0839_4523_ba87_95f89d27bd8c,
        3,
        if enable { 4 } else { 0 },
        instances,
    )
}

/// Set the Loudness EQ release-time parameter (valid range 2-7; meaning
/// undocumented by Microsoft, taken from LEQControlPanel's exposed range).
/// Same dual-write approach as apply_loudness_live (flat value + per-instance
/// user stores) even though the source competitor only confirmed the flat
/// write — Win11 silently ignoring flat-only writes for the loudness enable
/// flag is exactly the failure mode this avoids repeating blind.
pub fn set_release_time(
    full_id: &str,
    value: i32,
    instances: &[String],
) -> windows::core::Result<usize> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let policy: IPolicyConfig = CoCreateInstance(&CPOLICY_CONFIG_CLIENT, None, CLSCTX_ALL)?;
        let idw: Vec<u16> = full_id.encode_utf16().chain(Some(0)).collect();
        let id = PCWSTR(idw.as_ptr());

        let mut pv = propvariant_i32(value);
        policy.set_property_value(id, BOOL(1), &PKEY_RELEASE_TIME, &mut pv).ok()?;
        let mut pv_alt = propvariant_i32(value);
        policy
            .set_property_value(id, BOOL(1), &PKEY_RELEASE_TIME_ALT, &mut pv_alt)
            .ok()?;

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDevice(id)?;
        let mut wrote = 0;
        for inst in instances {
            let inst_guid = GUID::from(inst.trim_matches(|c| c == '{' || c == '}'));
            let params = propvariant_clsid(&inst_guid);
            let Ok(store) =
                device.Activate::<IAudioSystemEffectsPropertyStore>(CLSCTX_ALL, Some(&params))
            else {
                continue;
            };
            let Ok(user) = store.OpenUserPropertyStore(STGM_READWRITE.0) else {
                continue;
            };
            let pv = propvariant_i32(value);
            if user.SetValue(&PKEY_RELEASE_TIME, &pv).is_ok() {
                let _ = user.Commit();
                wrote += 1;
            }
        }
        Ok(wrote)
    }
}

/// Toggle the endpoint's master "Enable audio enhancements" switch — the same
/// control the Sound control panel exposes and that Windows 11 buried in most
/// of its UI, backed by PKEY_AudioEndpoint_Disable_SysFx (1 = all effects off,
/// 0 = on). This is the parent of the individual enhancements: with it off the
/// whole sAPO chain is bypassed, so Loudness EQ goes silent too until it's back
/// on. (Enabling loudness via apply_loudness_live flips this back on for that
/// reason.) Applied live through the policy service and persisted to the FX
/// store, no admin required — same mechanism as the loudness write.
///
/// Unlike the per-instance loudness/release-time writes, this is a single
/// endpoint-level property: it lives flat under FxProperties, not in the
/// \{instance\}\User stores, so there's nothing per-instance to mirror.
pub fn set_enhancements_enabled(full_id: &str, enabled: bool) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let policy: IPolicyConfig = CoCreateInstance(&CPOLICY_CONFIG_CLIENT, None, CLSCTX_ALL)?;
        let idw: Vec<u16> = full_id.encode_utf16().chain(Some(0)).collect();
        let id = PCWSTR(idw.as_ptr());
        let mut pv = propvariant_u32(if enabled { 0 } else { 1 });
        policy
            .set_property_value(id, BOOL(1), &PKEY_DISABLE_SYSFX, &mut pv)
            .ok()
    }
}

/// Toggle Windows' "Mono audio" accessibility setting (Settings →
/// Accessibility → Audio) — system-wide, no admin. Backed by the same
/// CPolicyConfigClient object as everything else here, through IPolicyConfig's
/// SetAccessibilityAudioMonoMixState at vtable slot 21 (see the interface's
/// reserved-slot comment for how that was found).
///
/// A bare registry write to AccessibilityMonoMixState does NOT apply live —
/// the audio service owns the value and only re-reads it when asked through
/// this call, which is exactly what the Settings toggle does. Applying it
/// reconfigures the render endpoints, so expect a brief sub-second audio
/// glitch, same as flipping the switch in Settings.
///
/// The vtable slot is Windows-version-sensitive, so this reads the state back
/// and returns an error if it didn't actually change — a future build that
/// moved the method fails cleanly instead of silently calling something else.
pub fn set_mono_audio(enabled: bool) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let policy: IPolicyConfigMono =
            CoCreateInstance(&CPOLICY_CONFIG_CLIENT, None, CLSCTX_ALL)?;
        policy
            .set_accessibility_mono_mix(if enabled { 1 } else { 0 })
            .ok()?;
    }
    if read_mono_audio() == enabled {
        Ok(())
    } else {
        windows::Win32::Foundation::E_FAIL.ok()
    }
}

/// Read the current "Mono audio" state from its per-user registry value
/// (HKCU, no admin). Absent value means off — Windows' default.
pub fn read_mono_audio() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Multimedia\Audio")
        .and_then(|k| k.get_value::<u32, _>("AccessibilityMonoMixState"))
        .map(|v| v == 1)
        .unwrap_or(false)
}

/// Read a VT_I4 FX property by its "{fmtid},pid" value name, preferring the
/// Win11 per-instance `\User` store (what the engine honors), then the flat
/// value. None if the property isn't present on the device.
pub fn read_fx_i32(guid: &str, value_name: &str) -> Option<i32> {
    let fx = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(fx_properties_path(guid), KEY_READ)
        .ok()?;
    for inst in fx.enum_keys().flatten() {
        if let Ok(user) = fx.open_subkey_with_flags(format!(r"{inst}\User"), KEY_READ) {
            if let Ok(rv) = user.get_raw_value(value_name) {
                if let Some(v) = parse_i32_value(&rv) {
                    return Some(v);
                }
            }
        }
    }
    parse_i32_value(&fx.get_raw_value(value_name).ok()?)
}

/// Read the current release-time value (2-7).
pub fn read_release_time(guid: &str) -> Option<i32> {
    read_fx_i32(guid, "{9c00eeed-edce-4cd8-ae08-cb05e8ef57a0},3")
}

/// Bass Boost on/off (enable flag != 0). None if the device never set it.
pub fn read_bass_boost(guid: &str) -> Option<bool> {
    read_fx_i32(guid, BASS_BOOST_VALUE).map(|v| v != 0)
}

/// Bass Boost cutoff frequency in Hz, if set.
pub fn read_bass_boost_freq(guid: &str) -> Option<i32> {
    read_fx_i32(guid, BASS_BOOST_FREQ_VALUE)
}

/// Bass Boost level in dB (index*3 + 3), if set.
pub fn read_bass_boost_level(guid: &str) -> Option<i32> {
    read_fx_i32(guid, BASS_BOOST_LEVEL_VALUE).map(|idx| idx * 3 + 3)
}

/// Virtual Surround on/off (enable flag != 0). None if never set.
pub fn read_virtual_surround(guid: &str) -> Option<bool> {
    read_fx_i32(guid, VIRTUAL_SURROUND_VALUE).map(|v| v != 0)
}

/// The endpoint's form factor (Speakers=1, Headphones=3, Headset=5, …), read
/// from the device Properties store. None if absent.
pub fn read_form_factor(guid: &str) -> Option<i32> {
    let props = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(format!(r"{RENDER_PATH}\{guid}\Properties"), KEY_READ)
        .ok()?;
    parse_i32_value(&props.get_raw_value(FORM_FACTOR_VALUE).ok()?)
}

/// Whether Virtual Surround applies to this device. It's a speaker effect;
/// headphone/headset endpoints expose "Headphone Virtualization" instead (a
/// different effect loudeq doesn't control yet), so the toggle would be an
/// inert no-op there. Unknown form factor → assume available (fail open, so a
/// device that just doesn't report one isn't wrongly blocked).
pub fn virtual_surround_available(guid: &str) -> bool {
    !matches!(read_form_factor(guid), Some(3) | Some(5))
}

/// Like parse_bool_value but returns the raw i32/u32 payload instead of
/// coercing to bool — needed for release-time's 2-7 range.
fn parse_i32_value(rv: &RegValue) -> Option<i32> {
    match rv.vtype {
        RegType::REG_DWORD => {
            let b: [u8; 4] = rv.bytes.get(0..4)?.try_into().ok()?;
            Some(i32::from_le_bytes(b))
        }
        RegType::REG_BINARY => {
            let vt: [u8; 4] = rv.bytes.get(0..4)?.try_into().ok()?;
            match u32::from_le_bytes(vt) {
                0x03 | 0x13 => {
                    let v: [u8; 4] = rv.bytes.get(8..12)?.try_into().ok()?;
                    Some(i32::from_le_bytes(v))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Force the endpoint to re-initialize by re-applying its current device
/// format (what the Sound control panel's Apply does). Running streams get
/// invalidated and well-behaved apps (browsers, players) reopen them, picking
/// up the new effect chain — at the cost of a sub-second audio hiccup.
pub fn reset_endpoint(full_id: &str) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let policy: IPolicyConfig = CoCreateInstance(&CPOLICY_CONFIG_CLIENT, None, CLSCTX_ALL)?;
        let idw: Vec<u16> = full_id.encode_utf16().chain(Some(0)).collect();
        let id = PCWSTR(idw.as_ptr());
        let mut fmt: *mut WAVEFORMATEX = std::ptr::null_mut();
        // bDefault = 0: the currently configured shared-mode format.
        policy.get_device_format(id, BOOL(0), &mut fmt).ok()?;
        let hr = policy.set_device_format(id, fmt, fmt);
        CoTaskMemFree(Some(fmt as _));
        hr.ok()
    }
}

#[derive(Debug)]
pub struct Device {
    pub guid: String,
    /// Full MMDevice endpoint ID, e.g. "{0.0.0.00000000}.{a748ee06-...}".
    pub full_id: String,
    pub name: String,
    pub is_default: bool,
}

pub fn state_text(state: Option<bool>) -> &'static str {
    match state {
        Some(true) => "ON",
        Some(false) => "OFF",
        None => "OFF (never set)",
    }
}

pub fn resolve_target<'a>(devices: &'a [Device], filter: Option<&str>) -> Result<&'a Device, String> {
    match filter {
        Some(f) => {
            let needle = f.to_ascii_lowercase();
            let matches: Vec<&Device> = devices
                .iter()
                .filter(|d| d.name.to_ascii_lowercase().contains(&needle))
                .collect();
            match matches.as_slice() {
                [one] => Ok(one),
                [] => Err(format!(
                    "no active playback device matches \"{f}\" — try `loudeq list`"
                )),
                many => Err(format!(
                    "\"{f}\" matches {} devices — be more specific:\n{}",
                    many.len(),
                    many.iter()
                        .map(|d| format!("  {}", d.name))
                        .collect::<Vec<_>>()
                        .join("\n")
                )),
            }
        }
        None => devices
            .iter()
            .find(|d| d.is_default)
            .ok_or_else(|| "could not determine the default playback device — pass --device".into()),
    }
}

/// Endpoint GUID of the default render device, via the MMDevice COM API.
pub fn default_endpoint_guid() -> Option<String> {
    unsafe {
        // S_FALSE (already initialized) is fine too.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
        let id_ptr = device.GetId().ok()?;
        let id = id_ptr.to_string().ok();
        CoTaskMemFree(Some(id_ptr.0 as _));
        // Full ID looks like "{0.0.0.00000000}.{a748ee06-...}"; the registry
        // key name is just the trailing GUID.
        let id = id?;
        id.rfind('.').map(|pos| id[pos + 1..].to_string())
    }
}

pub fn enumerate_devices(default_guid: Option<&str>) -> Result<Vec<Device>, String> {
    let render = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(RENDER_PATH, KEY_READ)
        .map_err(|e| format!("cannot open audio endpoint registry key: {e}"))?;

    let mut devices = Vec::new();
    for guid in render.enum_keys().flatten() {
        let Ok(dev_key) = render.open_subkey_with_flags(&guid, KEY_READ) else {
            continue;
        };
        // DEVICE_STATE_ACTIVE = 1 (higher bits carry unrelated flags).
        let state: u32 = dev_key.get_value("DeviceState").unwrap_or(0);
        if state & 1 == 0 {
            continue;
        }
        let name = dev_key
            .open_subkey_with_flags("Properties", KEY_READ)
            .map(|props| {
                let endpoint: String = props.get_value(ENDPOINT_NAME_VALUE).unwrap_or_default();
                let desc: String = props.get_value(DEVICE_DESC_VALUE).unwrap_or_default();
                match (endpoint.is_empty(), desc.is_empty()) {
                    (false, false) => format!("{endpoint} ({desc})"),
                    (false, true) => endpoint,
                    (true, false) => desc,
                    (true, true) => guid.clone(),
                }
            })
            .unwrap_or_else(|_| guid.clone());

        devices.push(Device {
            is_default: default_guid == Some(guid.as_str()),
            full_id: format!("{{0.0.0.00000000}}.{guid}"),
            guid,
            name,
        });
    }
    devices.sort_by(|a, b| b.is_default.cmp(&a.is_default).then(a.name.cmp(&b.name)));
    Ok(devices)
}

/// Sample the endpoint's output peak meter; returns (max, average) in 0..=1.
/// Lets callers verify objectively that the toggle changes the signal.
pub fn measure_peaks(full_id: &str, seconds: u32) -> Result<(f32, f32), String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| format!("COM error: {e}"))?;
        let idw: Vec<u16> = full_id.encode_utf16().chain(Some(0)).collect();
        let device = enumerator
            .GetDevice(PCWSTR(idw.as_ptr()))
            .map_err(|e| format!("cannot open device: {e}"))?;
        let meter: IAudioMeterInformation = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| format!("cannot open peak meter: {e}"))?;

        let samples = seconds * 10;
        let mut max = 0.0f32;
        let mut sum = 0.0f32;
        for _ in 0..samples {
            let p = meter.GetPeakValue().unwrap_or(0.0);
            max = max.max(p);
            sum += p;
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Ok((max, sum / samples as f32))
    }
}

/// Single instantaneous peak reading, no internal sleep loop — unlike
/// `measure_peaks`, safe to call repeatedly from a UI timer tick (e.g. every
/// ~150ms to drive a live meter) without blocking the message loop.
pub fn peak_now(full_id: &str) -> Result<f32, String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| format!("COM error: {e}"))?;
        let idw: Vec<u16> = full_id.encode_utf16().chain(Some(0)).collect();
        let device = enumerator
            .GetDevice(PCWSTR(idw.as_ptr()))
            .map_err(|e| format!("cannot open device: {e}"))?;
        let meter: IAudioMeterInformation = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| format!("cannot open peak meter: {e}"))?;
        meter.GetPeakValue().map_err(|e| format!("cannot read peak: {e}"))
    }
}

/// Whether "disable all enhancements" is set for the endpoint.
pub fn read_sysfx_disabled(guid: &str) -> bool {
    let disabled = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(fx_properties_path(guid), KEY_READ)
        .ok()
        .and_then(|fx| fx.get_raw_value(DISABLE_SYSFX_VALUE).ok())
        .and_then(|rv| parse_bool_value(&rv));
    disabled == Some(true)
}

pub fn read_loudness(guid: &str) -> Option<bool> {
    let fx = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(fx_properties_path(guid), KEY_READ)
        .ok()?;
    // Prefer the Win11 per-instance user store — it's what the effects engine
    // and the Enhancements dialog actually honor.
    for inst in fx.enum_keys().flatten() {
        if let Ok(user) = fx.open_subkey_with_flags(format!(r"{inst}\User"), KEY_READ) {
            if let Ok(rv) = user.get_raw_value(LOUDNESS_VALUE) {
                if let Some(b) = parse_bool_value(&rv) {
                    return Some(b);
                }
            }
        }
    }
    parse_bool_value(&fx.get_raw_value(LOUDNESS_VALUE).ok()?)
}

/// Values in the MMDevice property stores are either native registry types or
/// a serialized PROPVARIANT: u32 vt, u32 reserved(=1), then the raw payload.
pub fn parse_bool_value(rv: &RegValue) -> Option<bool> {
    match rv.vtype {
        RegType::REG_DWORD => {
            let b: [u8; 4] = rv.bytes.get(0..4)?.try_into().ok()?;
            Some(u32::from_le_bytes(b) != 0)
        }
        RegType::REG_BINARY => {
            let vt: [u8; 4] = rv.bytes.get(0..4)?.try_into().ok()?;
            match u32::from_le_bytes(vt) {
                // VT_BOOL: payload is a 2-byte VARIANT_BOOL
                0x0b => {
                    let v: [u8; 2] = rv.bytes.get(8..10)?.try_into().ok()?;
                    Some(u16::from_le_bytes(v) != 0)
                }
                // VT_I4 / VT_UI4
                0x03 | 0x13 => {
                    let v: [u8; 4] = rv.bytes.get(8..12)?.try_into().ok()?;
                    Some(u32::from_le_bytes(v) != 0)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Serialized VT_BOOL PROPVARIANT, the format the Enhancements dialog writes.
fn vt_bool_value(enable: bool) -> RegValue {
    let payload: u16 = if enable { 0xffff } else { 0 };
    let mut bytes = Vec::with_capacity(12);
    bytes.extend_from_slice(&0x0b_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    bytes.extend_from_slice(&payload.to_le_bytes());
    bytes.extend_from_slice(&[0, 0]);
    RegValue {
        bytes,
        vtype: RegType::REG_BINARY,
    }
}

/// Fallback path: write the flat FxProperties value directly in the registry.
/// Only takes effect after the audio service restarts.
pub fn write_loudness(guid: &str, enable: bool) -> io::Result<()> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let fx = hklm
        .open_subkey_with_flags(fx_properties_path(guid), KEY_READ | KEY_SET_VALUE)
        .map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                io::Error::new(
                    ErrorKind::NotFound,
                    "this device has no FxProperties key — it likely does not support \
                     Windows audio enhancements at all",
                )
            } else {
                e
            }
        })?;

    // Match the type of an existing value; otherwise use the VT_BOOL blob
    // format Windows itself writes.
    let value = match fx.get_raw_value(LOUDNESS_VALUE) {
        Ok(existing) if existing.vtype == RegType::REG_DWORD => RegValue {
            bytes: (enable as u32).to_le_bytes().to_vec(),
            vtype: RegType::REG_DWORD,
        },
        _ => vt_bool_value(enable),
    };
    fx.set_raw_value(LOUDNESS_VALUE, &value)?;

    // Loudness EQ has no effect while "disable all enhancements" is set.
    if enable {
        if let Ok(disable_sysfx) = fx.get_raw_value(DISABLE_SYSFX_VALUE) {
            if parse_bool_value(&disable_sysfx) == Some(true) {
                let off = RegValue {
                    bytes: 0_u32.to_le_bytes().to_vec(),
                    vtype: RegType::REG_DWORD,
                };
                fx.set_raw_value(DISABLE_SYSFX_VALUE, &off)?;
            }
        }
    }
    Ok(())
}

pub enum RestartError {
    AccessDenied,
    Other(String),
}

/// Stop and start Windows Audio via the Service Control Manager so the
/// endpoint's effects graph re-reads FxProperties. Works without elevation
/// once `loudeq setup` has granted start/stop rights. Only Audiosrv is
/// touched; AudioEndpointBuilder and vendor services keep running.
pub fn restart_audio_service() -> Result<(), RestartError> {
    unsafe {
        let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT)
            .map_err(|e| RestartError::Other(format!("cannot connect to SCM: {e}")))?;
        let svc = OpenServiceW(
            scm,
            w!("Audiosrv"),
            SERVICE_STOP | SERVICE_START | SERVICE_QUERY_STATUS,
        )
        .map_err(|e| {
            let _ = CloseServiceHandle(scm);
            if e.code() == ERROR_ACCESS_DENIED.to_hresult() {
                RestartError::AccessDenied
            } else {
                RestartError::Other(format!("cannot open Audiosrv: {e}"))
            }
        })?;

        let result = (|| {
            let mut status = SERVICE_STATUS::default();
            match ControlService(svc, SERVICE_CONTROL_STOP, &mut status) {
                Ok(()) => {}
                Err(e) if e.code() == ERROR_SERVICE_NOT_ACTIVE.to_hresult() => {}
                Err(e) if e.code() == ERROR_ACCESS_DENIED.to_hresult() => {
                    return Err(RestartError::AccessDenied)
                }
                Err(e) => return Err(RestartError::Other(format!("cannot stop Audiosrv: {e}"))),
            }
            wait_for_state(svc, SERVICE_STOPPED.0)?;

            match StartServiceW(svc, None) {
                Ok(()) => {}
                Err(e) if e.code() == ERROR_SERVICE_ALREADY_RUNNING.to_hresult() => {}
                Err(e) => return Err(RestartError::Other(format!("cannot start Audiosrv: {e}"))),
            }
            wait_for_state(svc, SERVICE_RUNNING.0)
        })();

        let _ = CloseServiceHandle(svc);
        let _ = CloseServiceHandle(scm);
        result
    }
}

unsafe fn wait_for_state(
    svc: windows::Win32::Security::SC_HANDLE,
    wanted: u32,
) -> Result<(), RestartError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let mut status = SERVICE_STATUS::default();
        QueryServiceStatus(svc, &mut status)
            .map_err(|e| RestartError::Other(format!("cannot query Audiosrv: {e}")))?;
        if status.dwCurrentState.0 == wanted {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            return Err(RestartError::Other(
                "timed out waiting for the audio service to change state".into(),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        let _ = CloseHandle(token);
        ok.is_ok() && elevation.TokenIsElevated != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str, guid: &str, is_default: bool) -> Device {
        Device {
            guid: guid.into(),
            full_id: format!("{{0.0.0.00000000}}.{guid}"),
            name: name.into(),
            is_default,
        }
    }

    #[test]
    fn parse_bool_value_reg_dword() {
        let on = RegValue { bytes: 1u32.to_le_bytes().to_vec(), vtype: RegType::REG_DWORD };
        let off = RegValue { bytes: 0u32.to_le_bytes().to_vec(), vtype: RegType::REG_DWORD };
        assert_eq!(parse_bool_value(&on), Some(true));
        assert_eq!(parse_bool_value(&off), Some(false));
    }

    #[test]
    fn parse_bool_value_vt_bool_propvariant() {
        // vt=0x0b (VT_BOOL), reserved u32, then a 2-byte VARIANT_BOOL payload.
        let mut on_bytes = 0x0b_u32.to_le_bytes().to_vec();
        on_bytes.extend_from_slice(&1u32.to_le_bytes());
        on_bytes.extend_from_slice(&0xffffu16.to_le_bytes());
        on_bytes.extend_from_slice(&[0, 0]);
        let on = RegValue { bytes: on_bytes, vtype: RegType::REG_BINARY };
        assert_eq!(parse_bool_value(&on), Some(true));

        let mut off_bytes = 0x0b_u32.to_le_bytes().to_vec();
        off_bytes.extend_from_slice(&1u32.to_le_bytes());
        off_bytes.extend_from_slice(&0u16.to_le_bytes());
        off_bytes.extend_from_slice(&[0, 0]);
        let off = RegValue { bytes: off_bytes, vtype: RegType::REG_BINARY };
        assert_eq!(parse_bool_value(&off), Some(false));
    }

    #[test]
    fn parse_bool_value_vt_ui4_propvariant() {
        // vt=0x13 (VT_UI4), reserved u32, then a 4-byte value.
        let mut bytes = 0x13_u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&7u32.to_le_bytes());
        let rv = RegValue { bytes, vtype: RegType::REG_BINARY };
        assert_eq!(parse_bool_value(&rv), Some(true));
    }

    #[test]
    fn parse_bool_value_rejects_unrecognized_or_too_short() {
        let unrecognized_vt = RegValue {
            bytes: 0xffff_u32.to_le_bytes().to_vec(),
            vtype: RegType::REG_BINARY,
        };
        assert_eq!(parse_bool_value(&unrecognized_vt), None);

        let truncated = RegValue { bytes: vec![1, 2, 3], vtype: RegType::REG_BINARY };
        assert_eq!(parse_bool_value(&truncated), None);

        let wrong_type = RegValue { bytes: b"hello".to_vec(), vtype: RegType::REG_SZ };
        assert_eq!(parse_bool_value(&wrong_type), None);
    }

    #[test]
    fn parse_i32_value_reads_release_time_range() {
        // vt=0x03 (VT_I4), reserved u32, then a 4-byte signed payload —
        // the exact wire format LEQControlPanel uses for release time.
        for release_time in 2..=7i32 {
            let mut bytes = 0x03_u32.to_le_bytes().to_vec();
            bytes.extend_from_slice(&1u32.to_le_bytes());
            bytes.extend_from_slice(&release_time.to_le_bytes());
            let rv = RegValue { bytes, vtype: RegType::REG_BINARY };
            assert_eq!(parse_i32_value(&rv), Some(release_time));
        }
    }

    #[test]
    fn vt_bool_value_round_trips_through_parse_bool_value() {
        // The exact property we relied on throughout development: whatever
        // we write with vt_bool_value must read back correctly through
        // parse_bool_value, since Windows itself round-trips the same way.
        assert_eq!(parse_bool_value(&vt_bool_value(true)), Some(true));
        assert_eq!(parse_bool_value(&vt_bool_value(false)), Some(false));
    }

    #[test]
    fn state_text_formats_all_three_states() {
        assert_eq!(state_text(Some(true)), "ON");
        assert_eq!(state_text(Some(false)), "OFF");
        assert_eq!(state_text(None), "OFF (never set)");
    }

    #[test]
    fn resolve_target_defaults_to_the_default_device() {
        let devices = vec![
            device("Speakers", "guid-a", false),
            device("Headphones", "guid-b", true),
        ];
        let picked = resolve_target(&devices, None).unwrap();
        assert_eq!(picked.guid, "guid-b");
    }

    #[test]
    fn resolve_target_errs_with_no_default_and_no_filter() {
        let devices = vec![device("Speakers", "guid-a", false)];
        assert!(resolve_target(&devices, None).is_err());
    }

    #[test]
    fn resolve_target_matches_by_case_insensitive_substring() {
        let devices = vec![
            device("Speakers (Philips SPA6109)", "guid-a", true),
            device("EDIFIER W830NB", "guid-b", false),
        ];
        let picked = resolve_target(&devices, Some("philips")).unwrap();
        assert_eq!(picked.guid, "guid-a");
    }

    #[test]
    fn resolve_target_errs_on_no_match() {
        let devices = vec![device("Speakers", "guid-a", true)];
        assert!(resolve_target(&devices, Some("nonexistent")).is_err());
    }

    /// Touches the real default playback device's registry — skipped by
    /// default (`cargo test` doesn't run #[ignore]'d tests), run explicitly
    /// with `cargo test -- --ignored` on a machine with an active device.
    #[test]
    #[ignore]
    fn set_release_time_round_trips_on_real_device() {
        let guid = default_endpoint_guid().expect("no default playback device");
        let devices = enumerate_devices(Some(&guid)).unwrap();
        let dev = devices.into_iter().find(|d| d.guid == guid).unwrap();
        let instances = fx_instance_guids(&dev.guid);

        let wrote = set_release_time(&dev.full_id, 5, &instances).expect("write failed");
        assert!(wrote > 0, "expected at least one instance store written");
        assert_eq!(read_release_time(&dev.guid), Some(5));

        let wrote2 = set_release_time(&dev.full_id, 3, &instances).expect("write failed");
        assert!(wrote2 > 0);
        assert_eq!(read_release_time(&dev.guid), Some(3));
    }

    /// Round-trips the master enhancements switch on the real default device.
    /// Restores whatever state it started in so running the test doesn't
    /// silently leave the machine's enhancements off. `--ignored` only.
    #[test]
    #[ignore]
    fn set_enhancements_enabled_round_trips_on_real_device() {
        let guid = default_endpoint_guid().expect("no default playback device");
        let devices = enumerate_devices(Some(&guid)).unwrap();
        let dev = devices.into_iter().find(|d| d.guid == guid).unwrap();
        let original_enabled = !read_sysfx_disabled(&dev.guid);

        set_enhancements_enabled(&dev.full_id, false).expect("write failed");
        assert!(read_sysfx_disabled(&dev.guid), "expected enhancements off");

        set_enhancements_enabled(&dev.full_id, true).expect("write failed");
        assert!(!read_sysfx_disabled(&dev.guid), "expected enhancements on");

        // Leave the device the way we found it.
        set_enhancements_enabled(&dev.full_id, original_enabled).expect("restore failed");
    }

    /// Round-trips the system-wide "Mono audio" accessibility setting through
    /// the reverse-engineered IPolicyConfig slot, verifying it applies (the
    /// registry reflects the change) and restoring the original state. This
    /// is the real proof the vtable slot is correct on this Windows build —
    /// `--ignored` only, since it touches live audio. Expect a brief glitch.
    #[test]
    #[ignore]
    fn set_mono_audio_round_trips_live() {
        let original = read_mono_audio();

        set_mono_audio(true).expect("enable failed");
        assert!(read_mono_audio(), "expected mono on");

        set_mono_audio(false).expect("disable failed");
        assert!(!read_mono_audio(), "expected mono off");

        set_mono_audio(original).expect("restore failed");
    }

    #[test]
    fn resolve_target_errs_on_ambiguous_match() {
        let devices = vec![
            device("EDIFIER W830NB", "guid-a", false),
            device("EDIFIER W830NB Hands-Free", "guid-b", false),
        ];
        let err = resolve_target(&devices, Some("edifier")).unwrap_err();
        assert!(err.contains("2 devices"));
    }
}
