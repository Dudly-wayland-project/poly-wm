//! Point the per-user desktop portal stack at a running Poly session.
//!
//! `xdg-desktop-portal` is a per-user singleton activated by D-Bus (usually
//! through systemd). It picks its backend from `*-portals.conf` and opens the
//! Wayland socket it was handed **at startup**, both taken from the environment
//! of the systemd user manager — not from the environment of the client that
//! asks for a screencast. A Poly session therefore has to move that environment
//! over before a portal client can reach us:
//!
//! ```text
//! XDG_CURRENT_DESKTOP=POLY   -> picks poly-portals.conf and wlr's POLY config
//! WAYLAND_DISPLAY=wayland-N  -> the socket xdg-desktop-portal-wlr connects to
//! ```
//!
//! Switching is explicit rather than automatic at startup: the manager is
//! shared with whatever desktop the user logged into, so leaving `POLY` behind
//! after exiting Poly would route the *host* desktop's screenshots into a dead
//! socket. [`run`] remembers the previous values so that `poly portal off`
//! restores them.

use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, anyhow};
use tracing::{info, warn};

/// Value of `XDG_CURRENT_DESKTOP` while the portal stack points at Poly.
///
/// Matches the file names the user-side configuration uses: portals.conf is
/// case-folded (`poly-portals.conf`) while the wlr backend looks for the name
/// verbatim (`xdg-desktop-portal-wlr/POLY`).
pub const DESKTOP_NAME: &str = "POLY";

/// The portal frontend and the backends it may have activated.
const UNITS: [&str; 3] = [
    "xdg-desktop-portal.service",
    "xdg-desktop-portal-gtk.service",
    "xdg-desktop-portal-wlr.service",
];

/// Variables that decide where the portal looks for the compositor.
const KEYS: [&str; 3] = ["WAYLAND_DISPLAY", "XDG_CURRENT_DESKTOP", "XDG_SESSION_TYPE"];

/// File holding the values `poly portal on` replaced.
const STATE_FILE: &str = "poly-portal.previous";

/// What `poly portal` was asked to do.
#[derive(Debug, Clone)]
pub enum Action {
    /// Route ScreenCast/Screenshot to the running Poly session.
    On {
        /// Wayland display name; detected from the running session when omitted.
        display: Option<String>,
        /// Skip the "is this really a Poly socket" check.
        force: bool,
    },
    /// Restore the values `on` replaced.
    Off,
    /// Print what the portal stack currently points at.
    Status,
}

/// Run one `poly portal` invocation.
pub fn run(action: Action) -> anyhow::Result<()> {
    return match action {
        Action::On { display, force } => on(display, force),
        Action::Off => off(),
        Action::Status => status(),
    };
}

fn on(display: Option<String>, force: bool) -> anyhow::Result<()> {
    let display = match display {
        Some(display) => display,
        None => detect_display(force)?,
    };
    ensure_session(&display, force)?;
    check_config_files();

    let previous = systemd_env();
    if previous.is_empty() {
        return Err(anyhow!(
            "the systemd user manager is not reachable (no `systemctl --user` environment); \
             the portal is activated through it, so its environment cannot be changed from here"
        ));
    }
    if previous
        .iter()
        .any(|(key, value)| return key == "XDG_CURRENT_DESKTOP" && value == DESKTOP_NAME)
    {
        // Already pointing here: keep the original values we would have saved.
        info!("the portal stack already points at Poly");
    } else {
        save_previous(&previous);
    }

    set_systemd_env(&[
        ("XDG_CURRENT_DESKTOP", DESKTOP_NAME.to_owned()),
        ("WAYLAND_DISPLAY", display.clone()),
        ("XDG_SESSION_TYPE", "wayland".to_owned()),
    ])?;
    restart_portals()?;

    println!("portal: routed ScreenCast/Screenshot to Poly on {display}");
    println!("  XDG_CURRENT_DESKTOP={DESKTOP_NAME}  WAYLAND_DISPLAY={display}");
    println!("  backends restarted: {}", UNITS.join(", "));
    println!();
    println!("Screen recording now works from OBS, Firefox and `wf-recorder --portal`.");
    println!("Run `poly portal off` (from the host session) before logging back into KDE,");
    println!("otherwise KDE's screenshots and file dialogs keep pointing at Poly.");
    return Ok(());
}

fn off() -> anyhow::Result<()> {
    let restored = match load_previous() {
        Some(previous) => previous,
        None => {
            // No record (never ran `on`, or the runtime directory was cleaned):
            // fall back to the caller's environment, which is correct when this
            // runs from a terminal of the host session.
            let mut fallback = Vec::new();
            for key in KEYS {
                match std::env::var(key) {
                    Ok(value) if !value.is_empty() => fallback.push((key.to_owned(), value)),
                    _ => (),
                }
            }
            if fallback.is_empty() {
                return Err(anyhow!(
                    "no saved portal environment in ${}/{} and the current environment has \
                     none of {}; run this from the host desktop session, or set \
                     `XDG_CURRENT_DESKTOP` and `WAYLAND_DISPLAY` explicitly",
                    runtime_dir()?.display(),
                    STATE_FILE,
                    KEYS.join(", ")
                ));
            }
            warn!("no saved portal environment, restoring from this process' environment");
            fallback
        }
    };

    let pairs: Vec<(&str, String)> = restored
        .iter()
        .map(|(key, value)| return (key.as_str(), value.clone()))
        .collect();
    set_systemd_env(&pairs)?;
    restart_portals()?;
    let _ = std::fs::remove_file(state_path()?);

    let desktop = restored
        .iter()
        .find(|(key, _)| return key == "XDG_CURRENT_DESKTOP")
        .map(|(_, value)| return value.clone())
        .unwrap_or_else(|| return "the login session".to_owned());
    println!("portal: restored the previous routing ({desktop})");
    for (key, value) in &restored {
        println!("  {key}={value}");
    }
    return Ok(());
}

fn status() -> anyhow::Result<()> {
    let env = systemd_env();
    if env.is_empty() {
        println!("systemd user environment: not reachable (`systemctl --user show-environment`)");
    } else {
        println!("systemd user environment:");
        for key in KEYS {
            let value = env
                .iter()
                .find(|(name, _)| return name == key)
                .map(|(_, value)| return value.as_str())
                .unwrap_or("-");
            println!("  {key}={value}");
        }
    }
    let routed = env.iter().any(|(key, value)| {
        return key == "XDG_CURRENT_DESKTOP"
            && value.split(':').any(|part| {
                return part.eq_ignore_ascii_case(DESKTOP_NAME);
            });
    });
    println!(
        "  -> portals.conf: {}",
        if routed {
            "poly-portals.conf"
        } else {
            "the host desktop's"
        }
    );

    match detect_display(true) {
        Ok(display) => println!("running Poly session: {display}"),
        Err(_) => println!("running Poly session: none"),
    }
    check_config_files();
    return Ok(());
}

/// The display name of the running Poly session.
fn detect_display(quiet: bool) -> anyhow::Result<String> {
    let runtime = runtime_dir()?;
    let mut candidates: Vec<(std::time::SystemTime, String, PathBuf)> = Vec::new();
    let entries = std::fs::read_dir(&runtime)
        .with_context(|| return format!("error reading {}", runtime.display()))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // `poly.<display>.<pid>.sock`
        let Some(socket) = name
            .strip_prefix("poly.")
            .and_then(|rest| return rest.strip_suffix(".sock"))
        else {
            continue;
        };
        let Some((display, _pid)) = socket.rsplit_once('.') else {
            continue;
        };
        let path = entry.path();
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
        candidates.push((modified, display.to_owned(), path));
    }
    candidates.sort_by(|a, b| return b.0.cmp(&a.0));

    for (_, display_name, ipc) in candidates {
        // A leftover socket from a crashed compositor has no listener; this
        // check keeps us from pointing the portal at a dead display.
        if UnixStream::connect(&ipc).is_err() {
            continue;
        }
        if runtime.join(&display_name).exists() {
            return Ok(display_name);
        }
        if !quiet {
            warn!(
                "Poly's IPC socket {} is live but {} is gone",
                ipc.display(),
                display_name
            );
        }
    }

    if quiet {
        return Err(anyhow!("no running Poly session"));
    }
    return Err(anyhow!(
        "no running Poly session found in {}; start Poly first, or pass --display",
        runtime.display()
    ));
}

/// Refuse to point the portal at something that is not Poly.
fn ensure_session(display: &str, force: bool) -> anyhow::Result<()> {
    let runtime = runtime_dir()?;
    let socket = runtime.join(display);
    if !socket.exists() {
        return Err(anyhow!(
            "{} does not exist; is Poly running on {display}?",
            socket.display()
        ));
    }
    let is_socket = std::fs::metadata(&socket)
        .map(|metadata| return metadata.file_type().is_socket())
        .unwrap_or(false);
    if !is_socket {
        return Err(anyhow!("{} is not a socket", socket.display()));
    }
    if force {
        return Ok(());
    }
    // The IPC socket is what makes a wayland display provably ours; pointing
    // `XDG_CURRENT_DESKTOP=POLY` at the host compositor would silently break
    // its screenshots, so this is a hard stop rather than a warning.
    let ours = detect_display(true)
        .map(|found| return found == display)
        .unwrap_or(false);
    if !ours {
        return Err(anyhow!(
            "no running Poly IPC socket for {display}; if Poly really is on that display \
             (for example after a crash left the socket behind), pass --force"
        ));
    }
    return Ok(());
}

/// Warn about the two user-side portal configuration files.
fn check_config_files() {
    let Some(config_home) = config_home() else {
        return;
    };
    let portals = config_home.join("xdg-desktop-portal/poly-portals.conf");
    if !portals.exists() {
        warn!(
            "{} is missing; ScreenCast would fall back to the host desktop's backends",
            portals.display()
        );
    }
    let wlr = config_home.join(format!("xdg-desktop-portal-wlr/{DESKTOP_NAME}"));
    if !wlr.exists() {
        // Not fatal: the backend then falls back to its `config` file or the
        // built-in defaults (chooser_type=default).
        warn!(
            "{} is missing; xdg-desktop-portal-wlr uses its defaults",
            wlr.display()
        );
    }
}

fn runtime_dir() -> anyhow::Result<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .context("XDG_RUNTIME_DIR is not set; cannot find the running session")?;
    return Ok(PathBuf::from(dir));
}

fn config_home() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    let home = std::env::var_os("HOME")?;
    return Some(Path::new(&home).join(".config"));
}

fn state_path() -> anyhow::Result<PathBuf> {
    return Ok(runtime_dir()?.join(STATE_FILE));
}

/// `VAR=value` lines of the systemd user manager.
fn systemd_env() -> Vec<(String, String)> {
    let output = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut env = Vec::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            env.push((key.to_owned(), value.to_owned()));
        }
    }
    return env;
}

fn save_previous(env: &[(String, String)]) {
    let path = match state_path() {
        Ok(path) => path,
        Err(err) => {
            warn!("not saving the previous portal environment: {err}");
            return;
        }
    };
    let mut body = String::new();
    for key in KEYS {
        if let Some((_, value)) = env.iter().find(|(name, _)| return name == key) {
            body.push_str(key);
            body.push('=');
            body.push_str(value);
            body.push('\n');
        }
    }
    // A leftover file from an interrupted session must not win over the values
    // read now, so this always overwrites.
    if let Err(err) = std::fs::write(&path, body) {
        warn!("error writing {}: {err}", path.display());
    }
}

fn load_previous() -> Option<Vec<(String, String)>> {
    let path = state_path().ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    let mut env = Vec::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            if !value.is_empty() {
                env.push((key.to_owned(), value.to_owned()));
            }
        }
    }
    if env.is_empty() {
        return None;
    }
    return Some(env);
}

fn set_systemd_env(pairs: &[(&str, String)]) -> anyhow::Result<()> {
    let mut args = vec!["--user".to_owned(), "set-environment".to_owned()];
    for (key, value) in pairs {
        args.push(format!("{key}={value}"));
    }
    let status = Command::new("systemctl")
        .args(&args)
        .status()
        .context("error running systemctl; is this a systemd user session?")?;
    if !status.success() {
        return Err(anyhow!(
            "`systemctl --user set-environment` failed ({status})"
        ));
    }
    // D-Bus activation without systemd delegation reads dbus-daemon's own copy
    // of the environment; keeping the two in sync costs one fork. The values
    // are passed explicitly: a bare `NAME` would be read from *this* process'
    // environment, which is the host session's and would undo the change.
    let mut sync = vec!["--systemd".to_owned()];
    sync.extend(
        pairs
            .iter()
            .map(|(key, value)| return format!("{key}={value}")),
    );
    let _ = Command::new("dbus-update-activation-environment")
        .args(&sync)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    return Ok(());
}

/// Restart the frontend and its backends so they re-read the environment.
///
/// The frontend caches its backends and the backends cache their Wayland
/// connection, so changing the environment alone changes nothing until they
/// are restarted. Failures are reported but not fatal: a backend that is not
/// running (or not installed) simply has nothing to restart.
fn restart_portals() -> anyhow::Result<()> {
    let mut args = vec!["--user".to_owned(), "stop".to_owned()];
    args.extend(UNITS.iter().map(|unit| return (*unit).to_owned()));
    let _ = Command::new("systemctl")
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    // Start the frontend only: the backends are D-Bus activated on demand, and
    // an idle one costs nothing.
    let status = Command::new("systemctl")
        .args(["--user", "start", UNITS[0]])
        .status()
        .context("error running systemctl")?;
    if !status.success() {
        return Err(anyhow!(
            "`systemctl --user start {}` failed ({status})",
            UNITS[0]
        ));
    }
    return Ok(());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_name_matches_the_configuration_file_names() {
        // `poly-portals.conf` (case-folded) and `xdg-desktop-portal-wlr/POLY`
        // (verbatim) are two different spellings of this one value.
        assert_eq!(DESKTOP_NAME, "POLY");
        assert_eq!(DESKTOP_NAME.to_ascii_lowercase(), "poly");
    }

    #[test]
    fn state_round_trips_through_the_runtime_directory() {
        // The parser is what `off` depends on: one `KEY=value` per line, empty
        // values dropped so that a blank `XDG_SESSION_TYPE=` cannot unset it.
        let text = "WAYLAND_DISPLAY=wayland-1\nXDG_CURRENT_DESKTOP=KDE\nXDG_SESSION_TYPE=\n";
        let mut env = Vec::new();
        for line in text.lines() {
            if let Some((key, value)) = line.split_once('=') {
                if !value.is_empty() {
                    env.push((key.to_owned(), value.to_owned()));
                }
            }
        }
        assert_eq!(
            env,
            vec![
                ("WAYLAND_DISPLAY".to_owned(), "wayland-1".to_owned()),
                ("XDG_CURRENT_DESKTOP".to_owned(), "KDE".to_owned()),
            ]
        );
    }
}
