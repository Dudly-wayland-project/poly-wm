// Ported from niri (GPL-3.0-or-later): src/ipc/server.rs

//! The IPC server: one Unix socket, one calloop source per client.
//!
//! Each connection is a [`calloop::generic::Generic`] registered for
//! `READ | WRITE` in edge mode, so it is only woken when something actually
//! happened, and never re-armed. Reading and writing are line buffered by hand
//! because a request may arrive in two pieces and a reply may not fit in the
//! socket buffer.
//!
//! # Slow clients
//!
//! An event-stream client that stops reading must not slow the compositor down:
//! its socket buffer fills, `write` returns `WouldBlock`, and further events pile
//! up in a bounded queue. When the queue is full the client is disconnected.
//! Rendering never blocks on a socket.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::rc::Rc;

use poly_ipc::{Event, EventStreamState, Reply, Request, Response};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use tracing::{debug, info, warn};

use crate::core::layout::LayoutManager;
use crate::state::State;

/// How many messages may wait in one client's queue before it is dropped.
///
/// At 16 Hz of focus changes a 64 message backlog is several seconds of events:
/// a client that is that far behind is not going to catch up.
const QUEUE_LIMIT: usize = 64;

/// Largest request line we accept. A `spawn` with a huge argument list is still
/// far below this; anything bigger is a client bug or an attack.
const REQUEST_LIMIT: usize = 256 * 1024;

/// Everything needed to talk to one client.
struct Connection {
    /// A duplicate of the socket, used for writing.
    ///
    /// The registered file descriptor is read-only for calloop (`Generic` hands
    /// out `&mut NoIoDrop<F>`, and `Read`/`Write` are implemented for
    /// `&UnixStream`), so writes go through this second handle on the same
    /// socket. Both share the socket, so ordering is preserved.
    writer: UnixStream,
    /// The loop source, so the connection can be dropped from a normal handler.
    token: Option<RegistrationToken>,
    /// Bytes of a request that is not complete yet.
    input: Vec<u8>,
    /// Replies and events waiting for the socket.
    output: VecDeque<String>,
    /// How much of the front of `output` has already been written.
    written: usize,
    /// Whether the client asked for the event stream.
    streaming: bool,
    /// Set when the connection must go away; cleaned up by [`IpcServer::prune`].
    dead: bool,
}

impl Connection {
    /// Queue one line and report whether it fits.
    fn push(&mut self, line: String) -> bool {
        if self.output.len() >= QUEUE_LIMIT {
            return false;
        }
        self.output.push_back(line);
        return true;
    }

    /// Write as much of the queue as the socket takes.
    fn flush(&mut self) {
        while let Some(front) = self.output.front() {
            let bytes = &front.as_bytes()[self.written..];
            match self.writer.write(bytes) {
                Ok(0) => {
                    self.dead = true;
                    return;
                }
                Ok(count) => {
                    self.written += count;
                    if self.written >= front.len() {
                        self.output.pop_front();
                        self.written = 0;
                    }
                }
                // The socket buffer is full: calloop wakes us when it drains.
                Err(err) if err.kind() == ErrorKind::WouldBlock => return,
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => {
                    debug!("error writing to an IPC client: {err}");
                    self.dead = true;
                    return;
                }
            }
        }
    }
}

/// The IPC server.
#[derive(Default)]
pub struct IpcServer {
    /// The bound but not yet served listener.
    listener: Option<UnixListener>,
    /// The accepting source, removed when the compositor stops.
    listener_token: Option<RegistrationToken>,
    /// Every connected client.
    connections: Vec<Rc<RefCell<Connection>>>,
    /// What the clients have been told so far (`ipc::events::diff`).
    pub(crate) events: EventStreamState,
}

impl IpcServer {
    /// Bind the socket.
    ///
    /// This happens before the backend starts so that a compositor that cannot
    /// create its socket fails immediately instead of after taking over a TTY.
    pub fn bind(&mut self, path: &Path) -> anyhow::Result<()> {
        let listener = UnixListener::bind(path).map_err(|err| {
            return anyhow::anyhow!("error binding IPC socket {}: {err}", path.display());
        })?;
        listener.set_nonblocking(true).map_err(|err| {
            return anyhow::anyhow!("error making the IPC socket non-blocking: {err}");
        })?;
        info!("IPC socket: {}", path.display());
        self.listener = Some(listener);
        return Ok(());
    }

    /// Register the listener with the event loop.
    pub fn start(&mut self, handle: &LoopHandle<'static, State>) -> anyhow::Result<()> {
        let Some(listener) = self.listener.take() else {
            return Ok(());
        };
        let source = Generic::new(listener, Interest::READ, Mode::Level);
        self.listener_token = Some(
            handle
                .insert_source(source, |_, listener, state: &mut State| {
                    accept(state, listener);
                    return Ok(PostAction::Continue);
                })
                .map_err(|err| {
                    return anyhow::anyhow!("error inserting the IPC listener: {err:?}");
                })?,
        );
        return Ok(());
    }

    /// Drop connections that closed or were disconnected.
    fn prune(&mut self, handle: &LoopHandle<'static, State>) {
        let mut tokens = Vec::new();
        self.connections.retain(|connection| {
            let mut connection = connection.borrow_mut();
            if !connection.dead {
                return true;
            }
            tokens.extend(connection.token.take());
            return false;
        });
        for token in tokens {
            handle.remove(token);
        }
    }

    /// Whether at least one client is receiving events.
    pub(crate) fn has_stream(&self) -> bool {
        return self.connections.iter().any(|connection| {
            let connection = connection.borrow();
            return connection.streaming && !connection.dead;
        });
    }
}

/// Accept every pending connection.
fn accept(state: &mut State, listener: &UnixListener) {
    loop {
        match listener.accept() {
            Ok((stream, _)) => add_connection(state, stream),
            Err(err) if err.kind() == ErrorKind::WouldBlock => return,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => {
                warn!("error accepting an IPC connection: {err}");
                return;
            }
        }
    }
}

/// Register a new client with the event loop.
fn add_connection(state: &mut State, stream: UnixStream) {
    if let Err(err) = stream.set_nonblocking(true) {
        warn!("error making an IPC connection non-blocking: {err}");
        return;
    }
    let writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(err) => {
            warn!("error duplicating an IPC connection: {err}");
            return;
        }
    };
    let connection = Rc::new(RefCell::new(Connection {
        writer,
        token: None,
        input: Vec::new(),
        output: VecDeque::new(),
        written: 0,
        streaming: false,
        dead: false,
    }));

    let shared = connection.clone();
    let handle = state.core.event_loop_handle.clone();
    let source = Generic::new(stream, Interest::BOTH, Mode::Edge);
    let token = match handle.insert_source(source, move |_, stream, state: &mut State| {
        return Ok(on_event(state, stream, &shared));
    }) {
        Ok(token) => token,
        Err(err) => {
            warn!("error inserting an IPC connection: {err:?}");
            return;
        }
    };
    connection.borrow_mut().token = Some(token);
    state.core.ipc.connections.push(connection);
    debug!("an IPC client connected");
}

/// The socket of one client is readable or writable.
fn on_event(
    state: &mut State,
    stream: &UnixStream,
    shared: &Rc<RefCell<Connection>>,
) -> PostAction {
    let mut requests = Vec::new();
    {
        let mut connection = shared.borrow_mut();
        if connection.dead {
            return PostAction::Remove;
        }
        if !read_requests(stream, &mut connection, &mut requests) {
            connection.dead = true;
            return PostAction::Remove;
        }
    }

    // Requests are handled outside the borrow: they change the compositor state,
    // which sends events to every client (including this one).
    for line in requests {
        handle_request(state, shared, &line);
    }

    let mut connection = shared.borrow_mut();
    connection.flush();
    return match connection.dead {
        true => PostAction::Remove,
        false => PostAction::Continue,
    };
}

/// Read everything the socket has, splitting complete request lines.
///
/// Returns `false` when the client is gone or sent something we refuse to
/// buffer.
fn read_requests(
    stream: &UnixStream,
    connection: &mut Connection,
    requests: &mut Vec<String>,
) -> bool {
    let mut reader = stream;
    let mut buffer = [0u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                debug!("an IPC client disconnected");
                return false;
            }
            Ok(count) => {
                connection.input.extend_from_slice(&buffer[..count]);
                if connection.input.len() > REQUEST_LIMIT {
                    warn!("disconnecting an IPC client that sent an oversized request");
                    return false;
                }
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => break,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => {
                debug!("error reading from an IPC client: {err}");
                return false;
            }
        }
    }

    while let Some(end) = connection
        .input
        .iter()
        .position(|byte| return *byte == b'\n')
    {
        let line = connection.input.drain(..=end).collect::<Vec<_>>();
        let text = String::from_utf8_lossy(&line[..line.len() - 1])
            .trim()
            .to_owned();
        if !text.is_empty() {
            requests.push(text);
        }
    }
    return true;
}

/// Answer one request line.
fn handle_request(state: &mut State, connection: &Rc<RefCell<Connection>>, line: &str) {
    // An event-stream connection does not answer to anything any more: its
    // remaining input would be interpreted as nonsense (`docs/ipc.md`).
    if connection.borrow().streaming {
        return;
    }

    let reply = match serde_json::from_str::<Request>(line) {
        Ok(Request::EventStream) => {
            push_reply(connection, &Ok(Response::Handled));
            start_stream(state, connection);
            return;
        }
        Ok(request) => process_request(state, request),
        Err(err) => Err(format!("malformed request: {err}")),
    };
    push_reply(connection, &reply);
}

/// Queue a reply line for one client.
fn push_reply(connection: &Rc<RefCell<Connection>>, reply: &Reply) {
    let mut line = match serde_json::to_string(reply) {
        Ok(line) => line,
        Err(err) => {
            warn!("error encoding an IPC reply: {err}");
            return;
        }
    };
    line.push('\n');
    let mut connection = connection.borrow_mut();
    if !connection.push(line) {
        warn!("disconnecting an IPC client whose reply queue is full");
        connection.dead = true;
    }
}

/// Turn a request into a reply.
fn process_request(state: &mut State, request: Request) -> Reply {
    return match request {
        Request::Version => Ok(Response::Version(env!("CARGO_PKG_VERSION").to_owned())),
        Request::Outputs => Ok(Response::Outputs(
            super::events::outputs(state)
                .into_iter()
                .map(|output| return (output.name.clone(), output))
                .collect(),
        )),
        Request::Workspaces => Ok(Response::Workspaces(super::events::workspaces(state))),
        Request::Windows => Ok(Response::Windows(super::events::windows(state))),
        Request::FocusedWindow => Ok(Response::FocusedWindow(super::events::focused_window(
            state,
        ))),
        Request::FocusedOutput => Ok(Response::FocusedOutput(super::events::focused_output(
            state,
        ))),
        Request::XwaylandStatus => Ok(Response::XwaylandStatus(xwayland_status(state))),
        Request::Metrics => Ok(Response::Metrics(metrics(state))),
        Request::Action(action) => {
            crate::core::actions::do_ipc_action(state, action);
            Ok(Response::Handled)
        }
        Request::ReloadConfig => {
            crate::core::config::reload(state);
            Ok(Response::Handled)
        }
        Request::OutputConfig { output, action } => {
            let changed = crate::core::output::apply(state, &output, action);
            Ok(Response::OutputConfigChanged(changed))
        }
        Request::LayoutSwitch { output, name } => {
            let target = match output {
                Some(name) => state
                    .core
                    .space
                    .outputs()
                    .find(|output| return output.name() == name)
                    .cloned()
                    .ok_or_else(|| return format!("no output named {name:?}")),
                None => match LayoutManager::active_output(state) {
                    Some(output) => Ok(output),
                    None => Err("no output has focus".to_owned()),
                },
            };
            match target {
                Ok(output) => match LayoutManager::set_layout_on(state, &output, None, &name) {
                    Ok(()) => Ok(Response::Handled),
                    Err(err) => Err(err),
                },
                Err(err) => Err(err),
            }
        }
        // Handled by `handle_request`, which has to send the reply first.
        Request::EventStream => Err("the event stream cannot be requested twice".to_owned()),
    };
}

/// The XWayland integration as it stands right now.
///
/// `enabled` is what the compositor managed to set up, not what the file asked
/// for: a satellite that cannot inherit sockets leaves the integration off, and
/// the log and a Toast say why.
/// The frame and capture counters, as the wire type reports them.
///
/// Percentiles come from the compositor's own ring of recent frames, so they
/// describe what the last couple of seconds looked like rather than the whole
/// session — which is what a person checking "is it smooth right now" wants.
fn metrics(state: &State) -> poly_ipc::Metrics {
    let mut outputs = std::collections::HashMap::new();
    for (output, metrics) in state.core.metrics.outputs() {
        outputs.insert(
            output.name(),
            poly_ipc::OutputMetrics {
                frames_submitted: metrics.frames_submitted,
                frames_no_damage: metrics.frames_no_damage,
                frames_skipped: metrics.frames_skipped,
                frames_late: metrics.frames_late,
                since_last_frame_us: micros(metrics.since_last_frame(std::time::Instant::now())),
                last_render_us: micros(metrics.last_render),
                mean_render_us: micros(metrics.mean_render()),
                p95_render_us: micros(metrics.percentile(0.95)),
            },
        );
    }
    let capture = &state.core.metrics.capture;
    let mean_time = if capture.frames == 0 {
        std::time::Duration::ZERO
    } else {
        capture.total_time / u32::try_from(capture.frames).unwrap_or(1)
    };
    return poly_ipc::Metrics {
        outputs,
        capture: poly_ipc::CaptureMetrics {
            frames: capture.frames,
            unchanged: capture.unchanged,
            deferred: capture.deferred,
            dmabuf_frames: capture.dmabuf_frames,
            failed: capture.failed,
            bytes_read_back: capture.bytes_read_back,
            last_time_us: micros(capture.last_time),
            mean_time_us: micros(mean_time),
        },
    };
}

/// Durations on the wire are whole microseconds.
fn micros(duration: std::time::Duration) -> u64 {
    return u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
}

fn xwayland_status(state: &State) -> poly_ipc::XwaylandStatus {
    let config = &state.core.config.xwayland;
    let satellite = state.core.satellite.as_ref();
    return poly_ipc::XwaylandStatus {
        enabled: satellite.is_some(),
        configured: config.enable,
        display: satellite.map(|satellite| return satellite.display_name().to_owned()),
        running: satellite.is_some_and(|satellite| return satellite.is_running()),
        // The effective mode: a satellite that keeps failing falls back to
        // on-demand activation, and callers should see that.
        on_demand: satellite.map_or(config.on_demand, |satellite| {
            return satellite.is_on_demand();
        }),
        satellite_path: config.satellite_path.clone(),
    };
}

/// Turn the connection into an event stream and send the full state.
fn start_stream(state: &mut State, connection: &Rc<RefCell<Connection>>) {
    super::events::resync(state);
    let events = state.core.ipc.events.replicate();
    {
        let mut connection = connection.borrow_mut();
        for event in &events {
            let mut line = match serde_json::to_string(event) {
                Ok(line) => line,
                Err(err) => {
                    warn!("error encoding an event: {err}");
                    continue;
                }
            };
            line.push('\n');
            if !connection.push(line) {
                warn!("disconnecting an IPC client whose queue is full");
                connection.dead = true;
                break;
            }
        }
        connection.streaming = !connection.dead;
        connection.flush();
    }
    info!("an IPC client subscribed to the event stream");
}

/// Send events to every streaming client.
pub(crate) fn send(state: &mut State, events: &[Event]) {
    if events.is_empty() {
        return;
    }
    let handle = state.core.event_loop_handle.clone();
    {
        let ipc = &mut state.core.ipc;
        for connection in &ipc.connections {
            let mut connection = connection.borrow_mut();
            if !connection.streaming || connection.dead {
                continue;
            }
            for event in events {
                let mut line = match serde_json::to_string(event) {
                    Ok(line) => line,
                    Err(err) => {
                        warn!("error encoding an event: {err}");
                        continue;
                    }
                };
                line.push('\n');
                if !connection.push(line) {
                    // The client is not keeping up. Dropping it keeps the
                    // compositor responsive; a client that wants the events has
                    // to reconnect and gets the whole state again.
                    warn!("disconnecting an IPC client that reads too slowly");
                    connection.dead = true;
                    break;
                }
            }
            connection.flush();
        }
        ipc.prune(&handle);
    }
}
