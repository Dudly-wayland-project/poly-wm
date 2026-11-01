//! Command line interface and process setup (logging, panic hook).

use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use tracing::error;
use tracing_subscriber::EnvFilter;

use crate::state;

const DEFAULT_LOG_FILTER: &str = "poly=debug,smithay=warn,info";

#[derive(Debug, Parser)]
#[command(
    name = "poly",
    version,
    about = "A Wayland compositor with Lua-scriptable layouts"
)]
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    sub: Option<Sub>,

    #[command(flatten)]
    run: RunArgs,
}

/// Options for running the compositor.
#[derive(Debug, Clone, clap::Args)]
pub struct RunArgs {
    /// Backend to use.
    #[arg(long, value_enum, default_value_t = BackendKind::Auto)]
    pub backend: BackendKind,

    /// Path to config.lua (overrides $POLY_CONFIG and the XDG lookup).
    #[arg(short, long)]
    pub config: Option<PathBuf>,

    /// Command to run through `sh -c` once the compositor is up (after `--`).
    #[arg(last = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BackendKind {
    /// winit when WAYLAND_DISPLAY or DISPLAY is set, udev otherwise.
    Auto,
    /// Nested window inside an existing Wayland/X11 session.
    Winit,
    /// Native TTY session via libseat + DRM/KMS.
    Udev,
    /// No display, for tests and CI.
    Headless,
}

impl BackendKind {
    /// Resolve `Auto` into a concrete backend.
    pub fn resolve(self) -> BackendKind {
        if self != BackendKind::Auto {
            return self;
        }
        let nested = ["WAYLAND_DISPLAY", "WAYLAND_SOCKET", "DISPLAY"]
            .iter()
            .any(|var| return std::env::var_os(var).is_some_and(|v| return !v.is_empty()));
        if nested {
            return BackendKind::Winit;
        }
        return BackendKind::Udev;
    }
}

#[derive(Debug, Subcommand)]
enum Sub {
    /// Locate and validate the config file without starting the compositor.
    Validate {
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Talk to a running compositor over IPC.
    Msg {
        /// Print machine-readable JSON instead of text, for scripts.
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        msg: Msg,
    },
    /// Print the IPC socket of the running compositor.
    Socket,
    /// Route the system-wide desktop portal at a running Poly session.
    ///
    /// Screen recording through `xdg-desktop-portal` (`OBS`, Firefox,
    /// `wf-recorder --portal`) needs the portal to talk to Poly instead of the
    /// desktop the session was logged into.
    Portal {
        #[command(subcommand)]
        portal: PortalCmd,
    },
}

#[derive(Debug, Subcommand)]
enum PortalCmd {
    /// Point ScreenCast/Screenshot at the running Poly session.
    On {
        /// Wayland display name; detected from the running session when omitted.
        #[arg(long)]
        display: Option<String>,
        /// Skip the "is this really a Poly socket" check.
        #[arg(long)]
        force: bool,
    },
    /// Restore the portal routing saved by `on`.
    Off,
    /// Print what the portal stack currently points at.
    Status,
}

#[derive(Debug, Subcommand)]
enum Msg {
    /// Print the running compositor's version.
    Version,
    /// List the connected outputs.
    Outputs,
    /// List the workspace stack of every output.
    Workspaces,
    /// List the managed windows.
    Windows,
    /// Print the window that currently has the keyboard focus.
    FocusedWindow,
    /// Print the output that currently has the keyboard focus.
    FocusedOutput,
    /// Print the state of the XWayland integration (`xwayland-satellite`).
    Xwayland,
    /// Run an action, spelled as in `config.lua`.
    ///
    /// `poly msg action focus-workspace 2`, `poly msg action -- spawn foot -e top`.
    Action {
        /// The action and its arguments.
        #[arg(required = true, num_args = 1..)]
        argv: Vec<String>,
    },
    /// Print one event per line until interrupted.
    EventStream,
    /// Change an output at runtime; lost on the next `reload-config`.
    Output {
        /// Output name, as printed by `poly msg outputs`.
        output: String,
        #[command(subcommand)]
        action: OutputMsg,
    },
    /// Switch a workspace to a named layout.
    Layout {
        /// Layout name from the configuration.
        name: String,
        /// Output name; the focused output when omitted.
        #[arg(short, long)]
        output: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum OutputMsg {
    /// Set the fractional scale.
    Scale {
        /// New scale, e.g. `1.5`.
        scale: f64,
    },
    /// Switch to another mode: `1920x1080` or `1920x1080@60`.
    Mode {
        /// Size in pixels, optionally followed by `@` and a refresh rate in Hz.
        mode: String,
    },
    /// Move the output in compositor space.
    Position {
        /// Horizontal position in logical pixels.
        x: i32,
        /// Vertical position in logical pixels.
        y: i32,
    },
}

pub fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging();

    let res = match cli.sub {
        None => run_guarded(cli.run),
        Some(Sub::Validate { config }) => validate(config),
        Some(Sub::Msg { msg, json }) => send_msg(msg, json),
        Some(Sub::Socket) => print_socket(),
        Some(Sub::Portal { portal }) => crate::utils::portal::run(match portal {
            PortalCmd::On { display, force } => crate::utils::portal::Action::On { display, force },
            PortalCmd::Off => crate::utils::portal::Action::Off,
            PortalCmd::Status => crate::utils::portal::Action::Status,
        }),
    };

    match res {
        Ok(()) => return ExitCode::SUCCESS,
        Err(err) => {
            error!("{err:?}");
            return ExitCode::FAILURE;
        }
    }
}

fn init_logging() {
    let directives =
        std::env::var("RUST_LOG").unwrap_or_else(|_| return DEFAULT_LOG_FILTER.to_owned());
    let filter = EnvFilter::builder().parse_lossy(directives);
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Run the compositor; a panic anywhere in the event loop unwinds out of
/// [`state::run`], which drops the whole `State` (releasing DRM master, input
/// devices and the libseat session) before we report and exit.
fn run_guarded(args: RunArgs) -> anyhow::Result<()> {
    install_panic_hook();
    match panic::catch_unwind(AssertUnwindSafe(|| return state::run(args))) {
        Ok(res) => return res,
        Err(_) => {
            error!("poly crashed; session resources were released");
            return Err(anyhow::anyhow!("compositor panicked"));
        }
    }
}

fn install_panic_hook() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        error!("panic: {info}\n{backtrace}");
        default_hook(info);
    }));
}

fn validate(config: Option<PathBuf>) -> anyhow::Result<()> {
    let source = poly_config::locate_config(config.as_deref())?;
    match source.path() {
        Some(path) => println!("config: {}", path.display()),
        None => {
            println!("no config file found; the built-in defaults apply");
            if let Some(path) = source.watch_path() {
                println!(
                    "hint: starting poly once writes a documented default to {}",
                    path.display()
                );
            }
        }
    }

    let result = poly_config::load(&source);
    for warning in &result.warnings {
        println!("warning: {warning}");
    }

    match result.config {
        Ok(config) => {
            println!(
                "ok: {} ({} key binding(s), {} output rule(s), {} startup command(s))",
                config.input.keyboard.layout,
                config.keybinds.len(),
                config.outputs.len(),
                config.spawn_at_startup.len(),
            );
            if config.xwayland.enable {
                let when = match (config.xwayland.on_demand, config.xwayland.spawn_at_startup) {
                    (true, false) => "on demand".to_owned(),
                    (true, true) => "on demand, started at startup".to_owned(),
                    (false, _) => "kept running".to_owned(),
                };
                println!("xwayland: {} ({when})", config.xwayland.satellite_path);
            } else {
                println!("xwayland: disabled");
            }
            #[cfg(not(feature = "dbus"))]
            if config.errors.dbus {
                println!(
                    "warning: errors.dbus is enabled, but this build has no D-Bus support \
                     (rebuild with --features dbus); errors stay on-screen only"
                );
            }
            return Ok(());
        }
        Err(err) => {
            println!("{err}");
            return Err(anyhow::anyhow!("the configuration has errors"));
        }
    }
}

/// Print the socket path the compositor announced in the environment.
fn print_socket() -> anyhow::Result<()> {
    let Some(path) = std::env::var_os(poly_ipc::SOCKET_ENV) else {
        anyhow::bail!("${} is not set; is poly running?", poly_ipc::SOCKET_ENV);
    };
    println!("{}", PathBuf::from(path).display());
    return Ok(());
}

fn send_msg(msg: Msg, json: bool) -> anyhow::Result<()> {
    let mut socket = poly_ipc::Socket::connect()?;
    let request = match msg {
        Msg::EventStream => return event_stream(socket, json),
        Msg::Version => poly_ipc::Request::Version,
        Msg::Outputs => poly_ipc::Request::Outputs,
        Msg::Workspaces => poly_ipc::Request::Workspaces,
        Msg::Windows => poly_ipc::Request::Windows,
        Msg::FocusedWindow => poly_ipc::Request::FocusedWindow,
        Msg::FocusedOutput => poly_ipc::Request::FocusedOutput,
        Msg::Xwayland => poly_ipc::Request::XwaylandStatus,
        Msg::Action { argv } => poly_ipc::Request::Action(poly_ipc::Action::from_argv(&argv)?),
        Msg::Layout { name, output } => poly_ipc::Request::LayoutSwitch { output, name },
        Msg::Output { output, action } => poly_ipc::Request::OutputConfig {
            output,
            action: match action {
                OutputMsg::Scale { scale } => poly_ipc::OutputAction::Scale(scale),
                OutputMsg::Mode { mode } => parse_mode(&mode)?,
                OutputMsg::Position { x, y } => poly_ipc::OutputAction::Position { x, y },
            },
        },
    };
    let response = socket.request(&request)?;
    return print_response(&response, json);
}

/// `1920x1080`, or `1920x1080@60` for a refresh rate in Hz.
fn parse_mode(text: &str) -> anyhow::Result<poly_ipc::OutputAction> {
    let (size, refresh) = match text.split_once('@') {
        Some((size, refresh)) => (size, Some(refresh)),
        None => (text, None),
    };
    let Some((width, height)) = size.split_once(['x', 'X']) else {
        anyhow::bail!("expected a mode such as 1920x1080 or 1920x1080@60, got {text:?}");
    };
    let width = width
        .trim()
        .parse::<u16>()
        .with_context(|| return format!("{text:?}: bad width"))?;
    let height = height
        .trim()
        .parse::<u16>()
        .with_context(|| return format!("{text:?}: bad height"))?;
    let refresh = match refresh {
        Some(refresh) => {
            let hz = refresh
                .trim()
                .parse::<f64>()
                .with_context(|| return format!("{text:?}: bad refresh rate"))?;
            // The protocol carries millihertz; the command line is in Hz
            // because that is what people read off a monitor spec sheet.
            Some((hz * 1000.0).round().max(1.0) as u32)
        }
        None => None,
    };
    return Ok(poly_ipc::OutputAction::Mode {
        width,
        height,
        refresh,
    });
}

/// Print every event of a stream until the compositor goes away.
fn event_stream(mut socket: poly_ipc::Socket, json: bool) -> anyhow::Result<()> {
    socket.request(&poly_ipc::Request::EventStream)?;
    loop {
        match socket.read_event()? {
            Some(event) if json => println!("{}", serde_json::to_string(&event)?),
            Some(event) => println!("{}", event_text(&event)),
            None => return Ok(()),
        }
    }
}

fn print_response(response: &poly_ipc::Response, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", json_payload(response)?);
        return Ok(());
    }
    match response {
        poly_ipc::Response::Handled => (),
        poly_ipc::Response::Version(version) => println!("{version}"),
        poly_ipc::Response::Outputs(outputs) => print_outputs(outputs),
        poly_ipc::Response::Workspaces(workspaces) => print_workspaces(workspaces),
        poly_ipc::Response::Windows(windows) => print_windows(windows),
        poly_ipc::Response::FocusedWindow(Some(window)) => {
            print_windows(std::slice::from_ref(window));
        }
        poly_ipc::Response::FocusedWindow(None) => println!("no focused window"),
        poly_ipc::Response::FocusedOutput(Some(name)) => println!("{name}"),
        poly_ipc::Response::FocusedOutput(None) => println!("no focused output"),
        poly_ipc::Response::XwaylandStatus(status) => print_xwayland(status),
        poly_ipc::Response::OutputConfigChanged(changed) => match changed {
            poly_ipc::OutputConfigChanged::Applied => println!("applied"),
            poly_ipc::OutputConfigChanged::OutputWasMissing => {
                anyhow::bail!("no output with that name is connected")
            }
            poly_ipc::OutputConfigChanged::Rejected => {
                anyhow::bail!("the compositor rejected the change; see its log for the reason")
            }
        },
    }
    return Ok(());
}

/// The payload of a reply as JSON, so scripts do not have to unwrap the enum.
fn json_payload(response: &poly_ipc::Response) -> serde_json::Result<String> {
    let value = match response {
        poly_ipc::Response::Handled => return Ok("null".to_owned()),
        poly_ipc::Response::Version(version) => serde_json::to_value(version)?,
        poly_ipc::Response::Outputs(outputs) => serde_json::to_value(outputs)?,
        poly_ipc::Response::Workspaces(workspaces) => serde_json::to_value(workspaces)?,
        poly_ipc::Response::Windows(windows) => serde_json::to_value(windows)?,
        poly_ipc::Response::FocusedWindow(window) => serde_json::to_value(window)?,
        poly_ipc::Response::FocusedOutput(output) => serde_json::to_value(output)?,
        poly_ipc::Response::OutputConfigChanged(changed) => serde_json::to_value(changed)?,
        poly_ipc::Response::XwaylandStatus(status) => serde_json::to_value(status)?,
    };
    return Ok(value.to_string());
}

fn print_outputs(outputs: &std::collections::HashMap<String, poly_ipc::Output>) {
    let mut names = outputs.keys().collect::<Vec<_>>();
    names.sort();
    if names.is_empty() {
        println!("no outputs");
        return;
    }
    for name in names {
        let output = &outputs[name];
        println!("Output {name:?} ({} {})", output.make, output.model);
        match output.current_mode {
            Some(mode) => println!("  Current mode: {}", mode_text(mode)),
            None => println!("  Current mode: unknown"),
        }
        println!("  Scale: {}", output.scale);
        println!("  Position: {},{}", output.position.0, output.position.1);
        println!(
            "  Logical size: {}x{}",
            output.logical_size.0, output.logical_size.1
        );
        if !output.modes.is_empty() {
            let modes = output
                .modes
                .iter()
                .map(|mode| return mode_text(*mode))
                .collect::<Vec<_>>()
                .join(", ");
            println!("  Available modes: {modes}");
        }
    }
}

/// The XWayland integration in words (`poly msg xwayland`).
fn print_xwayland(status: &poly_ipc::XwaylandStatus) {
    if !status.enabled {
        match status.configured {
            true => println!(
                "xwayland failed to start ({}); see the compositor log",
                status.satellite_path
            ),
            false => println!("xwayland is switched off in the configuration"),
        }
        return;
    }
    match &status.display {
        Some(display) => println!("DISPLAY={display}"),
        None => println!("no DISPLAY"),
    }
    match (status.running, status.on_demand) {
        (true, _) => println!("{} is running", status.satellite_path),
        (false, true) => println!("{} starts with the first X11 client", status.satellite_path),
        (false, false) => println!("{} is not running", status.satellite_path),
    }
}

fn mode_text(mode: poly_ipc::Mode) -> String {
    return format!(
        "{}x{}@{:.3} Hz",
        mode.width,
        mode.height,
        f64::from(mode.refresh) / 1000.0
    );
}

fn print_workspaces(workspaces: &[poly_ipc::Workspace]) {
    if workspaces.is_empty() {
        println!("no workspaces");
        return;
    }
    let mut current = None;
    for workspace in workspaces {
        let name = workspace.output.as_deref().unwrap_or("<no output>");
        if current != Some(name) {
            println!("Output {name:?}");
            current = Some(name);
        }
        let mut flags = Vec::new();
        if workspace.is_active {
            flags.push("active");
        }
        if workspace.is_focused {
            flags.push("focused");
        }
        let flags = match flags.is_empty() {
            true => String::new(),
            false => format!(" ({})", flags.join(", ")),
        };
        match workspace.active_window_id {
            Some(id) => println!(
                "  {}: {}{flags} window {id}",
                workspace.index, workspace.layout
            ),
            None => println!("  {}: {}{flags}", workspace.index, workspace.layout),
        }
    }
}

fn print_windows(windows: &[poly_ipc::Window]) {
    if windows.is_empty() {
        println!("no windows");
        return;
    }
    for window in windows {
        let mut flags = String::new();
        if window.is_focused {
            flags.push_str(" focused");
        }
        if window.is_floating {
            flags.push_str(" floating");
        }
        match window.fullscreen {
            Some(poly_ipc::FullscreenMode::Fullscreen) => flags.push_str(" fullscreen"),
            Some(poly_ipc::FullscreenMode::Windowed) => flags.push_str(" windowed-fullscreen"),
            None => (),
        }
        let place = match &window.output {
            Some(output) => format!(
                " output={output:?} workspace={}",
                match window.workspace {
                    Some(index) => index.to_string(),
                    None => "-".to_owned(),
                }
            ),
            None => String::new(),
        };
        println!(
            "id={} app_id={:?} title={:?}{place}{flags}",
            window.id,
            window.app_id.as_deref().unwrap_or(""),
            window.title.as_deref().unwrap_or(""),
        );
    }
}

/// One line of text for one event.
fn event_text(event: &poly_ipc::Event) -> String {
    return match event {
        poly_ipc::Event::WorkspacesChanged { workspaces } => {
            format!("workspaces changed: {} workspace(s)", workspaces.len())
        }
        poly_ipc::Event::WorkspaceActiveWindowChanged {
            output,
            index,
            active_window_id,
        } => match active_window_id {
            Some(id) => format!("workspace {output}:{index} now focuses window {id}"),
            None => format!("workspace {output}:{index} has no active window"),
        },
        poly_ipc::Event::WindowsChanged { windows } => {
            format!("windows changed: {} window(s)", windows.len())
        }
        poly_ipc::Event::WindowOpenedOrChanged { window } => format!(
            "window {} {:?} opened or changed",
            window.id,
            window.title.as_deref().unwrap_or("")
        ),
        poly_ipc::Event::WindowClosed { id } => format!("window {id} closed"),
        poly_ipc::Event::WindowFocusChanged { id } => match id {
            Some(id) => format!("window {id} focused"),
            None => "no window is focused".to_owned(),
        },
        poly_ipc::Event::FloatingChanged { id, floating } => match floating {
            true => format!("window {id} floats"),
            false => format!("window {id} is tiled"),
        },
        poly_ipc::Event::FullscreenChanged { id, fullscreen } => match fullscreen {
            Some(poly_ipc::FullscreenMode::Fullscreen) => format!("window {id} is fullscreen"),
            Some(poly_ipc::FullscreenMode::Windowed) => {
                format!("window {id} is windowed-fullscreen")
            }
            None => format!("window {id} is windowed"),
        },
        poly_ipc::Event::LayoutChanged {
            output,
            workspace_index,
            name,
        } => match (output, workspace_index) {
            (Some(output), Some(index)) => format!("workspace {output}:{index} now uses {name}"),
            (Some(output), None) => format!("output {output:?} now uses {name}"),
            (None, _) => format!("the layout is now {name}"),
        },
        poly_ipc::Event::OutputsChanged { outputs } => {
            format!("outputs changed: {} output(s)", outputs.len())
        }
        poly_ipc::Event::OutputAdded { output } => format!("output {:?} added", output.name),
        poly_ipc::Event::OutputRemoved { name } => format!("output {name:?} removed"),
        poly_ipc::Event::OutputChanged { output } => format!("output {:?} changed", output.name),
        poly_ipc::Event::OverviewOpenedOrClosed { is_open } => match is_open {
            true => "the overview opened".to_owned(),
            false => "the overview closed".to_owned(),
        },
        poly_ipc::Event::ConfigLoaded { failed } => match failed {
            true => "the configuration has errors; the previous one is still in effect".to_owned(),
            false => "the configuration was loaded".to_owned(),
        },
    };
}
