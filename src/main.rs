//! loudeq — CLI to toggle Windows "Loudness Equalization" for playback devices.
//! Core logic lives in the loudeq library (src/lib.rs); a tray companion app
//! is in src/tray.rs (loudeq-tray.exe).

use std::env;
use std::ffi::OsStr;
use std::io::{self, BufRead, ErrorKind, Write as _};
use std::os::windows::ffi::OsStrExt;
use std::process::Command;

use loudeq::*;
use windows::core::PCWSTR;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
use winreg::RegKey;

/// Transcript log for elevated runs, whose console disappears on close.
static LOG: std::sync::OnceLock<std::sync::Mutex<std::fs::File>> = std::sync::OnceLock::new();

fn log_line(s: &str) {
    if let Some(m) = LOG.get() {
        if let Ok(mut f) = m.lock() {
            let _ = writeln!(f, "{s}");
        }
    }
}

/// println! that also lands in the transcript log.
macro_rules! say {
    ($($t:tt)*) => {{
        let s = format!($($t)*);
        println!("{s}");
        log_line(&s);
    }};
}

#[derive(Clone, Copy, PartialEq)]
enum Action {
    Toggle,
    On,
    Off,
    Status,
    List,
    Setup,
    Meter,
    Tray,
}

struct Options {
    action: Action,
    device_filter: Option<String>,
    no_restart: bool,
    remove: bool,
    elevated: bool,
}

fn main() {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(msg) => {
            eprintln!("{msg}\n");
            print_usage();
            std::process::exit(2);
        }
    };

    if opts.elevated {
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(env::temp_dir().join("loudeq.log"))
        {
            let _ = LOG.set(std::sync::Mutex::new(f));
            log_line(&format!("--- elevated run: {:?}", env::args().collect::<Vec<_>>()));
        }
    }

    let code = match run(&opts) {
        Ok(()) => 0,
        Err(msg) => {
            eprintln!("error: {msg}");
            log_line(&format!("error: {msg}"));
            1
        }
    };

    if opts.elevated {
        // We were launched in a fresh elevated console; keep it open so the
        // user can read the result.
        print!("\nPress Enter to close...");
        let _ = io::stdout().flush();
        let _ = io::stdin().lock().read_line(&mut String::new());
    }
    std::process::exit(code);
}

fn parse_args() -> Result<Options, String> {
    let mut action = None;
    let mut device_filter = None;
    let mut no_restart = false;
    let mut remove = false;
    let mut elevated = false;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_ascii_lowercase().as_str() {
            "on" | "enable" => action = Some(Action::On),
            "off" | "disable" => action = Some(Action::Off),
            "toggle" => action = Some(Action::Toggle),
            "status" => action = Some(Action::Status),
            "list" => action = Some(Action::List),
            "setup" => action = Some(Action::Setup),
            "meter" => action = Some(Action::Meter),
            "tray" => action = Some(Action::Tray),
            "-d" | "--device" => {
                device_filter =
                    Some(args.next().ok_or("--device requires a name (substring)")?);
            }
            "--no-restart" => no_restart = true,
            "--remove" => remove = true,
            "--elevated" => elevated = true,
            "-h" | "--help" | "help" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(Options {
        action: action.unwrap_or(Action::Toggle),
        device_filter,
        no_restart,
        remove,
        elevated,
    })
}

fn print_usage() {
    println!(
        "loudeq — toggle Windows Loudness Equalization

USAGE:
    loudeq [COMMAND] [OPTIONS]

COMMANDS:
    toggle      Flip Loudness Equalization on the default playback device (default)
    on          Enable it
    off         Disable it
    status      Show the current state
    list        List active playback devices and their state
    meter       Sample the device's output level for 5 s (verify the effect)
    tray        Start the tray app (loudeq-tray.exe): icon shows the state,
                click toggles
    setup       One-time UAC-free fallback setup: allow your user to restart
                the audio service. `setup --remove` restores the permissions.

OPTIONS:
    -d, --device <NAME>   Target a device by name substring instead of the default one
    --no-restart          Fallback path only: write the setting but skip the
                          audio service restart
    -h, --help            Show this help

Changes apply live (like the Sound control panel), no admin rights needed.
If the live path fails, loudeq falls back to registry + audio service restart,
which asks for elevation — run `loudeq setup` once to make that UAC-free too."
    );
}

fn run(opts: &Options) -> Result<(), String> {
    if opts.action == Action::Tray {
        return start_tray();
    }

    let default_guid = default_endpoint_guid();
    let devices = enumerate_devices(default_guid.as_deref())?;
    if devices.is_empty() {
        return Err("no active playback devices found".into());
    }

    match opts.action {
        Action::Tray => unreachable!(),
        Action::List => {
            for dev in &devices {
                println!(
                    "{} {}  —  Loudness Equalization: {}",
                    if dev.is_default { "*" } else { " " },
                    dev.name,
                    state_text(read_loudness(&dev.guid)),
                );
            }
            println!("\n(* = default playback device)");
            Ok(())
        }
        Action::Status => {
            let dev = resolve_target(&devices, opts.device_filter.as_deref())?;
            println!(
                "{}: Loudness Equalization is {}",
                dev.name,
                state_text(read_loudness(&dev.guid))
            );
            Ok(())
        }
        Action::Meter => {
            let dev = resolve_target(&devices, opts.device_filter.as_deref())?;
            println!(
                "Sampling output level of {} for 5 seconds — play some audio now...",
                dev.name
            );
            let (max, avg) = measure_peaks(&dev.full_id, 5)?;
            println!("peak: {:.1}%   average: {:.1}%", max * 100.0, avg * 100.0);
            Ok(())
        }
        Action::Setup => {
            if !is_elevated() {
                if opts.elevated {
                    return Err("still not elevated after requesting elevation".into());
                }
                println!("Setup needs administrator rights once — requesting elevation...");
                relaunch_elevated(&own_args());
            }
            if opts.remove {
                remove_service_grant()
            } else {
                grant_service_rights()
            }
        }
        Action::Toggle | Action::On | Action::Off => {
            let dev = resolve_target(&devices, opts.device_filter.as_deref())?;
            let current = read_loudness(&dev.guid).unwrap_or(false);
            let desired = match opts.action {
                Action::On => true,
                Action::Off => false,
                _ => !current,
            };

            // Preferred path: write through the audio policy service and the
            // per-instance effect stores, which applies the change live (this
            // is what the Sound control panel does).
            match apply_loudness_live(
                &dev.full_id,
                desired,
                read_sysfx_disabled(&dev.guid),
                &fx_instance_guids(&dev.guid),
            ) {
                Ok(wrote) => {
                    log_line(&format!("instance user stores written: {wrote}"));
                    // Already-playing streams keep their old effect chain;
                    // reset the endpoint so they reopen with the new one.
                    let note = match reset_endpoint(&dev.full_id) {
                        Ok(()) => "applied live",
                        Err(_) => "applied — restart playback in running apps to hear it",
                    };
                    say!(
                        "{}: Loudness Equalization set to {} ({note})",
                        dev.name,
                        state_text(Some(desired))
                    );
                    return Ok(());
                }
                Err(e) => {
                    say!("Live apply failed ({e}); falling back to registry + service restart.");
                }
            }

            // The FxProperties key is normally user-writable (that's how the
            // Enhancements dialog works unelevated), so try without admin
            // rights first.
            match write_loudness(&dev.guid, desired) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                    if opts.elevated {
                        return Err(
                            "access denied writing to the registry even though elevated".into()
                        );
                    }
                    println!("Administrator rights are needed — requesting elevation...");
                    relaunch_elevated(&own_args());
                }
                Err(e) => return Err(format!("failed to write setting: {e}")),
            }

            say!(
                "{}: Loudness Equalization set to {}",
                dev.name,
                state_text(Some(desired))
            );

            if opts.no_restart {
                say!("Skipped the audio service restart; the change applies after the device or service restarts.");
                return Ok(());
            }

            say!("Restarting the Windows Audio service...");
            match restart_audio_service() {
                Ok(()) => {
                    say!("Done. Audio output was interrupted for a moment while the service restarted.");
                    Ok(())
                }
                Err(RestartError::AccessDenied) => {
                    if opts.elevated {
                        return Err("access denied restarting the audio service even though elevated".into());
                    }
                    println!("Restarting the audio service needs administrator rights — requesting elevation...");
                    println!("(tip: run `loudeq setup` once and you'll never see this UAC prompt again)");
                    // The setting is already written; the elevated child only
                    // needs to apply the explicit new state, not toggle again.
                    let mut args = vec![if desired { "on".into() } else { "off".into() }];
                    if let Some(f) = &opts.device_filter {
                        args.push("--device".into());
                        args.push(f.clone());
                    }
                    relaunch_elevated(&args);
                }
                Err(RestartError::Other(msg)) => Err(msg),
            }
        }
    }
}

/// Launch loudeq-tray.exe (from the same directory as this exe), detached.
fn start_tray() -> Result<(), String> {
    let tray = env::current_exe()
        .map_err(|e| format!("cannot determine own path: {e}"))?
        .with_file_name("loudeq-tray.exe");
    if !tray.exists() {
        return Err(format!("{} not found — build/install it first", tray.display()));
    }
    Command::new(&tray)
        .spawn()
        .map_err(|e| format!("cannot start tray app: {e}"))?;
    println!("Tray app started — look for the loudeq icon near the clock.");
    println!("(if the tray was already running, this toggles Loudness EQ instead)");
    Ok(())
}

/// Own command line minus the internal --elevated flag.
fn own_args() -> Vec<String> {
    env::args().skip(1).filter(|a| a != "--elevated").collect()
}

/// ACE granting Interactive Users start (RP), stop (WP) and query (LC) on a
/// service — in the normalized right-order `sc sdshow` reports, so the
/// idempotency and removal checks can find it again.
const LOUDEQ_ACE: &str = "(A;;LCRPWP;;;IU)";
const SDDL_BACKUP_KEY: &str = r"Software\loudeq";
const SDDL_BACKUP_VALUE: &str = "AudiosrvSddlBackup";

fn audiosrv_sddl() -> Result<String, String> {
    let out = Command::new("sc")
        .args(["sdshow", "Audiosrv"])
        .output()
        .map_err(|e| format!("failed to run `sc sdshow`: {e}"))?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("D:"))
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "unexpected `sc sdshow` output: {}",
                String::from_utf8_lossy(&out.stdout).trim()
            )
        })
}

fn set_audiosrv_sddl(sddl: &str) -> Result<(), String> {
    let out = Command::new("sc")
        .args(["sdset", "Audiosrv", sddl])
        .output()
        .map_err(|e| format!("failed to run `sc sdset`: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "`sc sdset` failed: {} {}",
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Grant interactive users the right to start/stop the audio service, so
/// the fallback path never needs elevation. The original security descriptor
/// is backed up in HKCU for `setup --remove`.
fn grant_service_rights() -> Result<(), String> {
    let sddl = audiosrv_sddl()?;
    if sddl.contains(LOUDEQ_ACE) {
        say!("Already set up — toggling works without UAC prompts.");
        return Ok(());
    }

    let backup = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(SDDL_BACKUP_KEY)
        .map_err(|e| format!("cannot create backup registry key: {e}"))?
        .0;
    // Keep the oldest backup if setup ran before.
    if backup.get_value::<String, _>(SDDL_BACKUP_VALUE).is_err() {
        backup
            .set_value(SDDL_BACKUP_VALUE, &sddl)
            .map_err(|e| format!("cannot back up current permissions: {e}"))?;
    }

    // Keep the DACL/SACL structure intact; insert our ACE at the end of the
    // discretionary part (before "S:" if a SACL is present).
    let new_sddl = match sddl.find("S:") {
        Some(pos) => format!("{}{}{}", &sddl[..pos], LOUDEQ_ACE, &sddl[pos..]),
        None => format!("{sddl}{LOUDEQ_ACE}"),
    };
    set_audiosrv_sddl(&new_sddl)?;
    say!("Setup complete — from now on, toggling Loudness Equalization won't show UAC prompts.");
    say!("(undo anytime with `loudeq setup --remove`)");
    Ok(())
}

fn remove_service_grant() -> Result<(), String> {
    let current = audiosrv_sddl()?;
    if !current.contains(LOUDEQ_ACE) {
        say!("Nothing to remove — the service permissions were not modified.");
        return Ok(());
    }
    set_audiosrv_sddl(&current.replace(LOUDEQ_ACE, ""))?;
    if let Ok(backup) = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(SDDL_BACKUP_KEY, KEY_READ | KEY_SET_VALUE)
    {
        let _ = backup.delete_value(SDDL_BACKUP_VALUE);
    }
    say!("Removed — toggling will ask for UAC elevation again.");
    Ok(())
}

/// Re-run ourselves with the given arguments through UAC and exit.
fn relaunch_elevated(args: &[String]) -> ! {
    let exe = env::current_exe().expect("cannot determine own path");
    let mut params: Vec<String> = args.iter().map(|a| format!("\"{a}\"")).collect();
    params.push("--elevated".into());
    let params = params.join(" ");

    let wide = |s: &OsStr| -> Vec<u16> { s.encode_wide().chain(Some(0)).collect() };
    let verb = wide(OsStr::new("runas"));
    let file = wide(exe.as_os_str());
    let args = wide(OsStr::new(&params));

    let h = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR(args.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecute returns a fake HINSTANCE; values <= 32 are error codes.
    if h.0 as isize <= 32 {
        eprintln!("Elevation was declined or failed — the setting was not changed.");
        std::process::exit(1);
    }
    std::process::exit(0);
}
