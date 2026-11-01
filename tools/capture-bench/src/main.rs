//! `poly-capture-bench` — a minimal `ext-image-copy-capture-v1` client that
//! hammers a running compositor with capture frames and reports what each frame
//! costs.
//!
//! It exists to verify the two things the capture optimisations have to get
//! right, which `grim` cannot check:
//!
//! 1. **Incremental damage is complete.** The client rotates several `wl_shm`
//!    buffers like a real screencast client, accumulates the damage rectangles
//!    it is told about, and never reads a buffer region it was not told about.
//!    The accumulated image is written as a PPM so it can be compared
//!    pixel-by-pixel against `grim`/`poly`'s own output (`magick compare`).
//!    A missing damage rectangle therefore shows up as a stale block.
//! 2. **Unchanged frames are cheap.** A static scene must produce frames with
//!    zero damage rectangles and near-zero latency; the report prints how many
//!    frames carried damage and how many bytes were reported in total.
//!
//! Not a library: the whole point is a standalone client that does not drag the
//! `staging`/`client` features of `wayland-protocols` into `poly` itself.

// Same style as the rest of the workspace: `return` is written explicitly.
#![allow(clippy::needless_return)]

use std::collections::HashMap;
use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::{
    Event as FrameEvent, ExtImageCopyCaptureFrameV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::{
    ExtImageCopyCaptureManagerV1, Options,
};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::{
    Event as SessionEvent, ExtImageCopyCaptureSessionV1,
};

/// A buffer-space damage rectangle, in pixels.
#[derive(Debug, Clone, Copy)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

#[derive(Parser, Debug)]
#[command(about = "Continuous capture stress client for Poly", version)]
struct Args {
    /// How many frames to capture.
    #[arg(long, default_value_t = 120)]
    frames: u64,

    /// How many `wl_shm` buffers to rotate, like a real screencast client.
    #[arg(long, default_value_t = 3)]
    buffers: usize,

    /// Name of the `wl_output` to capture (default: the first one).
    #[arg(long)]
    output: Option<String>,

    /// Accumulate every reported damage rectangle into an image and write it
    /// here as a binary PPM (P6), for comparison with `grim` output.
    #[arg(long)]
    ppm: Option<PathBuf>,

    /// Sleep this long between frames, to emulate a client that captures at a
    /// fixed frame rate instead of as fast as the compositor answers.
    #[arg(long, default_value_t = 0)]
    interval_ms: u64,

    /// Write the raw contents of the last used buffer here as a binary PPM
    /// (P6), ignoring what the damage events said: useful to tell "the
    /// compositor never wrote this region" apart from "the damage event for it
    /// never arrived".
    #[arg(long)]
    dump_slot: Option<PathBuf>,
}

#[derive(Default)]
struct OutputSlot {
    proxy: Option<wl_output::WlOutput>,
    name: Option<String>,
}

#[derive(Default)]
struct Bench {
    outputs: Vec<OutputSlot>,
    /// Buffer constraints from the session; only valid once `session_done` is
    /// set, because they arrive as separate events.
    width: i32,
    height: i32,
    session_done: bool,
    session_stopped: bool,
    has_xrgb8888: bool,
    /// Per-frame state, reset before every `capture` request.
    frame_damage: Vec<Rect>,
    frame_ready: bool,
    frame_failed: Option<String>,
    frame_transform: Option<wl_output::Transform>,
}

impl Bench {
    fn reset_frame(&mut self) {
        self.frame_damage.clear();
        self.frame_ready = false;
        self.frame_failed = None;
        self.frame_transform = None;
    }
}

macro_rules! ignore_events {
    ($($iface:ty),* $(,)?) => {
        $(
            impl Dispatch<$iface, ()> for Bench {
                fn event(
                    _: &mut Self,
                    _: &$iface,
                    _: <$iface as wayland_client::Proxy>::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }
        )*
    };
}

// `wl_shm_pool` has no events at all; the managers and the capture source only
// exist to create other objects, so we have nothing to do for them.
ignore_events!(
    wl_shm_pool::WlShmPool,
    ExtImageCaptureSourceV1,
    ExtOutputImageCaptureSourceManagerV1,
    ExtImageCopyCaptureManagerV1,
);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Bench {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for Bench {
    fn event(
        state: &mut Self,
        _: &wl_shm::WlShm,
        event: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_shm::Event::Format { format } = event
            && format == WEnum::Value(wl_shm::Format::Xrgb8888)
        {
            state.has_xrgb8888 = true;
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for Bench {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // `wl_buffer.release` is unused by this protocol (the frame object tells
        // us when the copy is done), so there is nothing to do.
    }
}

impl Dispatch<wl_output::WlOutput, u32> for Bench {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        index: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(slot) = state.outputs.get_mut(*index as usize)
        {
            slot.name = Some(name);
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for Bench {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: SessionEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            SessionEvent::BufferSize { width, height } => {
                state.width = width as i32;
                state.height = height as i32;
            }
            SessionEvent::ShmFormat { format } => {
                if format == WEnum::Value(wl_shm::Format::Xrgb8888) {
                    state.has_xrgb8888 = true;
                }
            }
            SessionEvent::Done => state.session_done = true,
            SessionEvent::Stopped => state.session_stopped = true,
            // dmabuf constraints: this client only offers shm, so ignore them.
            SessionEvent::DmabufDevice { .. } | SessionEvent::DmabufFormat { .. } => {}
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for Bench {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: FrameEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            FrameEvent::Damage {
                x,
                y,
                width,
                height,
            } => state.frame_damage.push(Rect {
                x,
                y,
                w: width,
                h: height,
            }),
            FrameEvent::Transform { transform } => match transform {
                WEnum::Value(transform) => state.frame_transform = Some(transform),
                WEnum::Unknown(_) => {}
            },
            FrameEvent::Ready => state.frame_ready = true,
            FrameEvent::Failed { reason } => {
                state.frame_failed = Some(match reason {
                    WEnum::Value(reason) => format!("{reason:?}"),
                    WEnum::Unknown(raw) => format!("unknown reason {raw}"),
                });
            }
            // Presentation timestamps are not part of what we measure here.
            FrameEvent::PresentationTime { .. } => {}
            _ => {}
        }
    }
}

/// Create a file suitable for a `wl_shm` pool.
///
/// The compositor only needs an fd it can `mmap`, so any regular file works; we
/// prefer tmpfs locations and unlink the file right away so a crash cannot leave
/// garbage behind, keeping our own handle alive for `pread`-style readback.
fn shm_file(size: usize) -> Result<File> {
    let mut candidates = Vec::new();
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    candidates.push(PathBuf::from("/dev/shm"));
    candidates.push(std::env::temp_dir());

    for dir in candidates {
        let path = dir.join(format!(
            "poly-capture-bench-{}-{}",
            std::process::id(),
            size
        ));
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(_) => continue,
        };
        let _ = std::fs::remove_file(&path);
        file.set_len(size as u64)
            .context("failed to size the shm pool file")?;
        return Ok(file);
    }

    bail!("no writable tmpfs directory for the shm pool");
}

fn apply_damage(
    file: &File,
    accum: &mut [u8],
    buffer_offset: u64,
    stride: usize,
    width: i32,
    height: i32,
    rect: Rect,
) -> Result<u64> {
    // Clamp to the buffer: the compositor should never report out-of-bounds
    // damage, but a bad rectangle must not make the bench read out of bounds.
    let x = rect.x.max(0);
    let y = rect.y.max(0);
    let x_end = (rect.x + rect.w).min(width);
    let y_end = (rect.y + rect.h).min(height);
    if x_end <= x || y_end <= y {
        return Ok(0);
    }
    let row_len = ((x_end - x) as usize) * 4;
    let mut bytes = 0u64;
    for row in y..y_end {
        let offset = row as usize * stride + x as usize * 4;
        let src = buffer_offset + offset as u64;
        let dst = &mut accum[offset..offset + row_len];
        file.read_exact_at(dst, src)?;
        bytes += row_len as u64;
    }
    return Ok(bytes);
}

/// Convert XRGB8888 (`B`, `G`, `R`, `X` byte order, little endian) to PPM.
fn write_ppm(path: &PathBuf, accum: &[u8], stride: usize, width: i32, height: i32) -> Result<()> {
    let mut out = Vec::with_capacity(width as usize * height as usize * 3 + 32);
    out.extend_from_slice(format!("P6\n{width} {height}\n255\n").as_bytes());
    for y in 0..height as usize {
        for x in 0..width as usize {
            let i = y * stride + x * 4;
            out.push(accum[i + 2]);
            out.push(accum[i + 1]);
            out.push(accum[i]);
        }
    }
    std::fs::write(path, &out).with_context(|| format!("failed to write {}", path.display()))?;
    return Ok(());
}

fn percentile(sorted: &[Duration], fraction: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let index = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    return sorted[index.min(sorted.len() - 1)];
}

fn main() -> Result<()> {
    let args = Args::parse();
    if args.buffers == 0 {
        bail!("--buffers must be at least 1");
    }

    let connection = Connection::connect_to_env()
        .context("failed to connect to a Wayland compositor (is WAYLAND_DISPLAY set?)")?;
    let (globals, mut queue) = registry_queue_init::<Bench>(&connection)?;
    let qhandle = queue.handle();
    let mut state = Bench::default();

    // Bind every output: the name only arrives with an event, so we need a
    // roundtrip before we can honour `--output`. The user data carries the
    // index into `state.outputs` so the event can be filed correctly.
    let mut output_globals = Vec::new();
    for global in globals.contents().clone_list() {
        if global.interface == "wl_output" {
            output_globals.push(global);
        }
    }
    if output_globals.is_empty() {
        bail!("the compositor exposes no wl_output global");
    }
    for (index, global) in output_globals.iter().enumerate() {
        state.outputs.push(OutputSlot::default());
        let proxy = globals
            .bind::<wl_output::WlOutput, Bench, u32>(
                &qhandle,
                1..=global.version.min(4),
                index as u32,
            )
            .context("failed to bind wl_output")?;
        state.outputs[index].proxy = Some(proxy);
    }
    queue.roundtrip(&mut state)?;

    let chosen = match &args.output {
        Some(wanted) => state
            .outputs
            .iter()
            .position(|slot| slot.name.as_deref() == Some(wanted.as_str()))
            .with_context(|| {
                let names: Vec<_> = state
                    .outputs
                    .iter()
                    .map(|slot| slot.name.clone().unwrap_or_default())
                    .collect();
                format!("no output named {wanted:?} (available: {names:?})")
            })?,
        None => 0,
    };
    let output_name = state.outputs[chosen]
        .name
        .clone()
        .unwrap_or_else(|| format!("#{chosen}"));

    let shm = globals
        .bind::<wl_shm::WlShm, Bench, ()>(&qhandle, 1..=1, ())
        .context("failed to bind wl_shm")?;
    let src_manager = globals
        .bind::<ExtOutputImageCaptureSourceManagerV1, Bench, ()>(&qhandle, 1..=1, ())
        .map_err(|error| {
            anyhow::anyhow!(
                "compositor does not support ext-output-image-capture-source-v1: {error:?}"
            )
        })?;
    let copy_manager = globals
        .bind::<ExtImageCopyCaptureManagerV1, Bench, ()>(&qhandle, 1..=1, ())
        .map_err(|error| {
            anyhow::anyhow!("compositor does not support ext-image-copy-capture-v1: {error:?}")
        })?;
    let output = state.outputs[chosen]
        .proxy
        .clone()
        .expect("chosen output was bound above");
    let source = src_manager.create_source(&output, &qhandle, ());
    // `paint_cursors` is not set: the bench compares raw output content, and a
    // moving cursor would only add noise to the damage we are verifying.
    let session = copy_manager.create_session(&source, Options::empty(), &qhandle, ());

    while !state.session_done && !state.session_stopped {
        queue.blocking_dispatch(&mut state)?;
    }
    if state.session_stopped {
        bail!("the compositor stopped the session before it was ready");
    }
    if state.width <= 0 || state.height <= 0 {
        bail!(
            "session reported no usable buffer size ({}x{})",
            state.width,
            state.height
        );
    }
    if !state.has_xrgb8888 {
        bail!("session does not offer XRGB8888 shm buffers");
    }

    let width = state.width;
    let height = state.height;
    let stride = width as usize * 4;
    let buffer_size = stride * height as usize;
    let pool_size = buffer_size * args.buffers;
    let file = shm_file(pool_size)?;

    let pool = shm.create_pool(file.as_fd(), pool_size as i32, &qhandle, ());
    let mut buffers = Vec::with_capacity(args.buffers);
    for index in 0..args.buffers {
        let buffer = pool.create_buffer(
            (index * buffer_size) as i32,
            width,
            height,
            stride as i32,
            wl_shm::Format::Xrgb8888,
            &qhandle,
            (),
        );
        buffers.push(buffer);
    }

    println!(
        "output {} ({}x{}, stride {}, {} buffers) — capturing {} frames",
        output_name, width, height, stride, args.buffers, args.frames
    );

    // The accumulator only ever receives the regions the compositor says it
    // wrote, so a missing damage rectangle shows up as an untouched block.
    let mut accum = vec![0u8; buffer_size];
    let mut latencies = Vec::with_capacity(args.frames as usize);
    let mut damaged_frames = 0u64;
    let mut unchanged_frames = 0u64;
    let mut reported_bytes = 0u64;
    let mut transforms: HashMap<String, u64> = HashMap::new();

    let started = Instant::now();
    for frame_index in 0..args.frames {
        let slot = frame_index as usize % buffers.len();
        let buffer = &buffers[slot];
        let buffer_offset = (slot * buffer_size) as u64;

        state.reset_frame();
        let frame = session.create_frame(&qhandle, ());
        frame.attach_buffer(buffer);
        // A client is expected to damage what it wants refreshed; our
        // compositor decides damage itself, so the whole buffer is the honest
        // request.
        frame.damage_buffer(0, 0, width, height);
        let start = Instant::now();
        frame.capture();

        while !state.frame_ready && state.frame_failed.is_none() {
            queue.blocking_dispatch(&mut state)?;
        }
        let elapsed = start.elapsed();
        if let Some(reason) = state.frame_failed.take() {
            frame.destroy();
            bail!("frame {frame_index} failed: {reason}");
        }

        if let Some(transform) = state.frame_transform {
            let label = format!("{transform:?}");
            *transforms.entry(label).or_default() += 1;
        }

        let damage = std::mem::take(&mut state.frame_damage);
        let mut frame_bytes = 0u64;
        for rect in &damage {
            frame_bytes += apply_damage(
                &file,
                &mut accum,
                buffer_offset,
                stride,
                width,
                height,
                *rect,
            )?;
        }
        reported_bytes += frame_bytes;
        if damage.is_empty() {
            unchanged_frames += 1;
        } else {
            damaged_frames += 1;
        }
        latencies.push(elapsed);
        frame.destroy();

        if args.interval_ms > 0 {
            std::thread::sleep(Duration::from_millis(args.interval_ms));
        }
    }
    let total = started.elapsed();

    let mut sorted = latencies.clone();
    sorted.sort();
    let mean: Duration = latencies.iter().sum::<Duration>() / (latencies.len().max(1) as u32);
    println!(
        "frames: {} ({} with damage, {} unchanged)",
        latencies.len(),
        damaged_frames,
        unchanged_frames
    );
    println!(
        "latency: min {:.3} ms, p50 {:.3} ms, p95 {:.3} ms, max {:.3} ms, mean {:.3} ms",
        sorted.first().copied().unwrap_or_default().as_secs_f64() * 1e3,
        percentile(&sorted, 0.50).as_secs_f64() * 1e3,
        percentile(&sorted, 0.95).as_secs_f64() * 1e3,
        sorted.last().copied().unwrap_or_default().as_secs_f64() * 1e3,
        mean.as_secs_f64() * 1e3,
    );
    println!(
        "reported damage: {:.2} MiB total, {:.1} KiB/frame mean",
        reported_bytes as f64 / (1024.0 * 1024.0),
        reported_bytes as f64 / 1024.0 / (latencies.len().max(1) as f64),
    );
    println!(
        "wall time: {:.2} s ({:.1} frames/s)",
        total.as_secs_f64(),
        latencies.len() as f64 / total.as_secs_f64().max(f64::MIN_POSITIVE),
    );
    for (transform, count) in &transforms {
        println!("transform: {transform} ({count} frames)");
    }

    if let Some(path) = &args.ppm {
        write_ppm(path, &accum, stride, width, height)?;
        println!("accumulated image written to {}", path.display());
    }

    if let Some(path) = &args.dump_slot {
        let slot = (args.frames.max(1) - 1) as usize % buffers.len();
        let mut raw = vec![0u8; buffer_size];
        file.read_exact_at(&mut raw, (slot * buffer_size) as u64)?;
        write_ppm(path, &raw, stride, width, height)?;
        println!(
            "buffer {} written to {} (raw, ignores the damage events)",
            slot,
            path.display()
        );
    }

    // Tidy up: the compositor keeps per-session state alive until the objects
    // are destroyed, and `stop` is the polite way to end a session.
    for buffer in &buffers {
        buffer.destroy();
    }
    pool.destroy();
    session.destroy();
    source.destroy();
    src_manager.destroy();
    copy_manager.destroy();
    drop(file);

    return Ok(());
}
