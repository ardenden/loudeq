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
    Bass(FxOp),
    Surround(FxOp),
}

/// Sub-operation for the Bass Boost / Virtual Surround commands. Freq/Level
/// only apply to Bass Boost.
#[derive(Clone, Copy, PartialEq)]
enum FxOp {
    Toggle,
    On,
    Off,
    Status,
    Freq(i32),
    Level(i32),
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
            "bass" | "bassboost" => action = Some(Action::Bass(parse_fx_op(&mut args, true)?)),
            "surround" | "virtualsurround" => {
                action = Some(Action::Surround(parse_fx_op(&mut args, false)?))
            }
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

/// Parse the sub-command after `bass`/`surround`. `allow_params` enables the
/// bass-only `freq <Hz>` / `level <dB>` forms; no sub-word means toggle.
fn parse_fx_op(
    args: &mut impl Iterator<Item = String>,
    allow_params: bool,
) -> Result<FxOp, String> {
    match args.next().as_deref().map(str::to_ascii_lowercase).as_deref() {
        None | Some("toggle") => Ok(FxOp::Toggle),
        Some("on") | Some("enable") => Ok(FxOp::On),
        Some("off") | Some("disable") => Ok(FxOp::Off),
        Some("status") => Ok(FxOp::Status),
        Some("freq") | Some("frequency") if allow_params => {
            let v = args.next().ok_or("`bass freq` needs a value in Hz (50-600)")?;
            Ok(FxOp::Freq(v.parse().map_err(|_| format!("invalid frequency: {v}"))?))
        }
        Some("level") if allow_params => {
            let v = args.next().ok_or("`bass level` needs a value in dB (3-24)")?;
            Ok(FxOp::Level(v.parse().map_err(|_| format!("invalid level: {v}"))?))
        }
        Some(other) => Err(format!("unknown sub-command: {other}")),
    }
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
    bass [on|off|status]        Toggle Bass Boost (default: toggle)
    bass freq <Hz>              Set Bass Boost cutoff frequency (50-600)
    bass level <dB>             Set Bass Boost level (3,6,9,12,15,18,21,24)
    surround [on|off|status]    Toggle Virtual Surround (default: toggle)
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
        Action::Bass(op) => {
            let dev = resolve_target(&devices, opts.device_filter.as_deref())?;
            let inst = fx_instance_guids(&dev.guid);
            match op {
                FxOp::Status => {
                    println!(
                        "{}: Bass Boost is {}",
                        dev.name,
                        state_text(read_bass_boost(&dev.guid))
                    );
                    if let (Some(f), Some(l)) =
                        (read_bass_boost_freq(&dev.guid), read_bass_boost_level(&dev.guid))
                    {
                        println!("  frequency: {f} Hz   boost level: {l} dB");
                    }
                    Ok(())
                }
                FxOp::Freq(hz) => {
                    if !(50..=600).contains(&hz) {
                        return Err("frequency must be between 50 and 600 Hz".into());
                    }
                    apply_fx(set_bass_boost_freq(&dev.full_id, hz, &inst))?;
                    let _ = reset_endpoint(&dev.full_id);
                    say!("{}: Bass Boost frequency set to {hz} Hz", dev.name);
                    bass_off_hint(&dev.guid);
                    Ok(())
                }
                FxOp::Level(db) => {
                    if !(3..=24).contains(&db) || db % 3 != 0 {
                        return Err(
                            "boost level must be one of 3, 6, 9, 12, 15, 18, 21, 24 (dB)".into()
                        );
                    }
                    apply_fx(set_bass_boost_level(&dev.full_id, db, &inst))?;
                    let _ = reset_endpoint(&dev.full_id);
                    say!("{}: Bass Boost level set to {db} dB", dev.name);
                    bass_off_hint(&dev.guid);
                    Ok(())
                }
                FxOp::On | FxOp::Off | FxOp::Toggle => {
                    let desired = match op {
                        FxOp::On => true,
                        FxOp::Off => false,
                        _ => !read_bass_boost(&dev.guid).unwrap_or(false),
                    };
                    ensure_enhancements_on(dev, desired);
                    apply_fx(set_bass_boost(&dev.full_id, desired, &inst))?;
                    let note = reset_word(&dev.full_id);
                    say!(
                        "{}: Bass Boost set to {} ({note})",
                        dev.name,
                        state_text(Some(desired))
                    );
                    Ok(())
                }
            }
        }
        Action::Surround(op) => {
            let dev = resolve_target(&devices, opts.device_filter.as_deref())?;
            let inst = fx_instance_guids(&dev.guid);
            match op {
                FxOp::Status => {
                    println!(
                        "{}: Virtual Surround is {}",
                        dev.name,
                        state_text(read_virtual_surround(&dev.guid))
                    );
                    Ok(())
                }
                FxOp::On | FxOp::Off | FxOp::Toggle => {
                    let desired = match op {
                        FxOp::On => true,
                        FxOp::Off => false,
                        _ => !read_virtual_surround(&dev.guid).unwrap_or(false),
                    };
                    ensure_enhancements_on(dev, desired);
                    apply_fx(set_virtual_surround(&dev.full_id, desired, &inst))?;
                    let note = reset_word(&dev.full_id);
                    say!(
                        "{}: Virtual Surround set to {} ({note})",
                        dev.name,
                        state_text(Some(desired))
                    );
                    Ok(())
                }
                FxOp::Freq(_) | FxOp::Level(_) => {
                    Err("Virtual Surround has no frequency/level settings".into())
                }
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

/// Map a core FX write result to a CLI error, logging the instance count.
fn apply_fx(r: windows::core::Result<usize>) -> Result<(), String> {
    match r {
        Ok(wrote) => {
            log_line(&format!("instance user stores written: {wrote}"));
            Ok(())
        }
        Err(e) => Err(format!(
            "live apply failed: {e}\n(check the master \"Audio enhancements\" switch is on, \
             or set it from the Enhancements tab)"
        )),
    }
}

/// Reset the endpoint so playing streams pick up the new effect chain; return
/// a note about whether it applied live.
fn reset_word(full_id: &str) -> &'static str {
    match reset_endpoint(full_id) {
        Ok(()) => "applied live",
        Err(_) => "applied — restart playback in running apps to hear it",
    }
}

/// An effect does nothing while the master "Audio enhancements" switch is off.
/// When enabling one, clear that switch first (same as the loudness path).
fn ensure_enhancements_on(dev: &Device, desired: bool) {
    if desired
        && read_sysfx_disabled(&dev.guid)
        && set_enhancements_enabled(&dev.full_id, true).is_ok()
    {
        say!("(also turned the master \"Audio enhancements\" switch back on)");
    }
}

/// Hint after setting a bass parameter while Bass Boost itself is off.
fn bass_off_hint(guid: &str) {
    if read_bass_boost(guid) != Some(true) {
        say!("(note: Bass Boost is currently OFF — run `loudeq bass on` to hear it)");
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
