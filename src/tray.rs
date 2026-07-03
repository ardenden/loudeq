//! loudeq-tray — system tray companion for loudeq.
//!
//! Shows a tray icon reflecting the Loudness Equalization state of the
//! default playback device (green dot = ON, gray ring = OFF). Left-click
//! toggles; right-click opens a menu. Launching a second instance (e.g. from
//! a pinned taskbar shortcut) relays a toggle to the running instance, so a
//! pinned button acts as a toggle button.

#![windows_subsystem = "windows"]

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};

use loudeq::*;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, TRUE, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyMenu, DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW, PostMessageW,
    PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, SetTimer,
    TrackPopupMenu, TranslateMessage, HICON, ICONINFO, MF_CHECKED, MF_GRAYED, MF_SEPARATOR,
    MF_STRING, MSG, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP,
    WM_DESTROY, WM_LBUTTONUP, WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_OVERLAPPED,
};
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

const WM_TRAYICON: u32 = WM_APP + 1;
const WM_EXTERNAL_TOGGLE: u32 = WM_APP + 2;
const IDM_TOGGLE: usize = 1;
const IDM_AUTOSTART: usize = 2;
const IDM_EXIT: usize = 3;
const TRAY_UID: u32 = 1;
const CLASS_NAME: PCWSTR = w!("LoudeqTrayWindow");
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "loudeq-tray";

static ICON_ON: AtomicIsize = AtomicIsize::new(0);
static ICON_OFF: AtomicIsize = AtomicIsize::new(0);
static LAST_STATE: AtomicBool = AtomicBool::new(false);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

fn main() {
    unsafe {
        // Single instance: a second launch (pinned shortcut click) relays a
        // toggle to the running one and exits.
        let existing = FindWindowW(CLASS_NAME, PCWSTR::null());
        if existing.0 != 0 {
            let _ = PostMessageW(existing, WM_EXTERNAL_TOGGLE, WPARAM(0), LPARAM(0));
            return;
        }

        let Ok(hinstance) = GetModuleHandleW(None) else {
            return;
        };

        ICON_ON.store(make_icon(true).0 as isize, Ordering::Relaxed);
        ICON_OFF.store(make_icon(false).0 as isize, Ordering::Relaxed);
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS_NAME,
            w!("loudeq"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            hinstance,
            None,
        );
        if hwnd.0 == 0 {
            return;
        }

        let state = current_state().unwrap_or(false);
        LAST_STATE.store(state, Ordering::Relaxed);
        add_tray_icon(hwnd, state);

        // Keep the icon in sync with changes made elsewhere (CLI, panel).
        SetTimer(hwnd, 1, 5000, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TRAYICON => {
                match lparam.0 as u32 {
                    WM_LBUTTONUP => do_toggle(hwnd),
                    WM_RBUTTONUP => show_menu(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_EXTERNAL_TOGGLE => {
                do_toggle(hwnd);
                LRESULT(0)
            }
            WM_TIMER => {
                if let Some(state) = current_state() {
                    if state != LAST_STATE.load(Ordering::Relaxed) {
                        LAST_STATE.store(state, Ordering::Relaxed);
                        update_icon(hwnd, state);
                    }
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let mut nid = base_nid(hwnd);
                let _ = Shell_NotifyIconW(NIM_DELETE, &mut nid);
                PostQuitMessage(0);
                LRESULT(0)
            }
            m if m == TASKBAR_CREATED.load(Ordering::Relaxed) && m != 0 => {
                // Explorer restarted; re-add our icon.
                add_tray_icon(hwnd, LAST_STATE.load(Ordering::Relaxed));
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn current_device() -> Option<Device> {
    let default_guid = default_endpoint_guid();
    enumerate_devices(default_guid.as_deref())
        .ok()?
        .into_iter()
        .find(|d| d.is_default)
}

fn current_state() -> Option<bool> {
    current_device().map(|d| read_loudness(&d.guid).unwrap_or(false))
}

fn do_toggle(hwnd: HWND) {
    let Some(dev) = current_device() else {
        balloon(hwnd, "loudeq", "No default playback device found.");
        return;
    };
    let desired = !read_loudness(&dev.guid).unwrap_or(false);
    match apply_loudness_live(
        &dev.full_id,
        desired,
        read_sysfx_disabled(&dev.guid),
        &fx_instance_guids(&dev.guid),
    ) {
        Ok(_) => {
            // Already-playing streams keep their old effect chain; reset the
            // endpoint so they reopen with the new one.
            let _ = reset_endpoint(&dev.full_id);
            LAST_STATE.store(desired, Ordering::Relaxed);
            update_icon(hwnd, desired);
            balloon(
                hwnd,
                &dev.name,
                if desired {
                    "Loudness Equalization: ON"
                } else {
                    "Loudness Equalization: OFF"
                },
            );
        }
        Err(e) => balloon(hwnd, "loudeq error", &format!("Could not apply: {e}")),
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };

    let state = current_state();
    let status = format!(
        "Loudness EQ: {}{}",
        state_text(state),
        current_device()
            .map(|d| format!(" — {}", d.name))
            .unwrap_or_default()
    );
    let status_w: Vec<u16> = status.encode_utf16().chain(Some(0)).collect();
    let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(status_w.as_ptr()));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, IDM_TOGGLE, w!("Toggle"));
    let autostart_flags = if autostart_enabled() {
        MF_STRING | MF_CHECKED
    } else {
        MF_STRING
    };
    let _ = AppendMenuW(menu, autostart_flags, IDM_AUTOSTART, w!("Start with Windows"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_EXIT, w!("Exit"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // Required so the menu closes when clicking elsewhere.
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);

    match cmd.0 as usize {
        IDM_TOGGLE => do_toggle(hwnd),
        IDM_AUTOSTART => set_autostart(!autostart_enabled()),
        IDM_EXIT => {
            let mut nid = base_nid(hwnd);
            let _ = Shell_NotifyIconW(NIM_DELETE, &mut nid);
            PostQuitMessage(0);
        }
        _ => {}
    }
}

fn autostart_enabled() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(RUN_KEY)
        .and_then(|k| k.get_value::<String, _>(RUN_VALUE))
        .is_ok()
}

fn set_autostart(enable: bool) {
    let Ok((key, _)) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(RUN_KEY) else {
        return;
    };
    if enable {
        if let Ok(exe) = std::env::current_exe() {
            let _ = key.set_value(RUN_VALUE, &format!("\"{}\"", exe.display()));
        }
    } else {
        let _ = key.delete_value(RUN_VALUE);
    }
}

// ---- tray icon plumbing ----------------------------------------------------

fn icon_for(state: bool) -> HICON {
    HICON(if state {
        ICON_ON.load(Ordering::Relaxed)
    } else {
        ICON_OFF.load(Ordering::Relaxed)
    })
}

fn base_nid(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW::default();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_UID;
    nid
}

fn tip_text(state: bool) -> String {
    format!(
        "Loudness EQ: {} — click to toggle",
        if state { "ON" } else { "OFF" }
    )
}

fn copy_utf16<const N: usize>(s: &str, buf: &mut [u16; N]) {
    let mut i = 0;
    for u in s.encode_utf16() {
        if i >= N - 1 {
            break;
        }
        buf[i] = u;
        i += 1;
    }
    buf[i] = 0;
}

fn add_tray_icon(hwnd: HWND, state: bool) {
    let mut nid = base_nid(hwnd);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAYICON;
    nid.hIcon = icon_for(state);
    copy_utf16(&tip_text(state), &mut nid.szTip);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &mut nid);
    }
}

fn update_icon(hwnd: HWND, state: bool) {
    let mut nid = base_nid(hwnd);
    nid.uFlags = NIF_ICON | NIF_TIP;
    nid.hIcon = icon_for(state);
    copy_utf16(&tip_text(state), &mut nid.szTip);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &mut nid);
    }
}

fn balloon(hwnd: HWND, title: &str, text: &str) {
    let mut nid = base_nid(hwnd);
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO;
    copy_utf16(title, &mut nid.szInfoTitle);
    copy_utf16(text, &mut nid.szInfo);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &mut nid);
    }
}

/// Build a 32x32 ARGB icon at runtime: solid green dot for ON, gray ring for
/// OFF — distinguishable by shape as well as color.
fn make_icon(on: bool) -> HICON {
    const S: usize = 32;
    let (r, g, b) = if on {
        (0x21u32, 0xc9u32, 0x3bu32)
    } else {
        (0x8au32, 0x8au32, 0x8au32)
    };
    let mut px = vec![0u8; S * S * 4];
    for y in 0..S {
        for x in 0..S {
            let dx = x as f32 - 15.5;
            let dy = y as f32 - 15.5;
            let d = (dx * dx + dy * dy).sqrt();
            // Outer edge, antialiased between r=13..15.
            let mut a = if d <= 13.0 {
                255.0
            } else if d < 15.0 {
                255.0 * (15.0 - d) / 2.0
            } else {
                0.0
            };
            // OFF is a ring: hollow center, antialiased between r=7..9.
            if !on {
                let hole = if d <= 7.0 {
                    0.0
                } else if d < 9.0 {
                    (d - 7.0) / 2.0
                } else {
                    1.0
                };
                a *= hole;
            }
            let a = a as u32;
            let i = (y * S + x) * 4;
            // BGRA, premultiplied alpha.
            px[i] = (b * a / 255) as u8;
            px[i + 1] = (g * a / 255) as u8;
            px[i + 2] = (r * a / 255) as u8;
            px[i + 3] = a as u8;
        }
    }
    unsafe {
        let color = CreateBitmap(S as i32, S as i32, 1, 32, Some(px.as_ptr() as _));
        let mask_bits = vec![0u8; S * S / 8];
        let mask = CreateBitmap(S as i32, S as i32, 1, 1, Some(mask_bits.as_ptr() as _));
        let info = ICONINFO {
            fIcon: TRUE,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info).unwrap_or_default();
        let _ = DeleteObject(color);
        let _ = DeleteObject(mask);
        icon
    }
}
