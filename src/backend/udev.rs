// Ported from niri (GPL-3.0-or-later): src/backend/tty.rs
// Ported from anvil (GPL-3.0-or-later): anvil/src/udev.rs

//! Native backend: DRM/KMS and libinput on a seat, opened through libseat.
//!
//! The event flow is: libinput events and client commits queue redraws, the
//! main loop calls [`Udev::render`], the frame is queued to KMS, and the DRM
//! VBlank event finishes the frame by sending frame callbacks and presentation
//! feedback. If a frame produced no damage we schedule a timer at the estimated
//! VBlank instead, so that clients still get frame callbacks.

use std::collections::HashMap;
use std::mem;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use smithay::backend::allocator::Fourcc;
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::{FrameFlags, PrimaryPlaneElement};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements};
use smithay::backend::drm::{
    DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmEventTime, DrmNode, VrrSupport,
};
use smithay::backend::egl::context::ContextPriority;
use smithay::backend::egl::{EGLDevice, EGLDisplay};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer};
use smithay::backend::renderer::multigpu::gbm::GbmGlesBackend;
use smithay::backend::renderer::multigpu::{self, GpuManager, MultiRenderer};
use smithay::backend::renderer::{ImportDma as _, ImportMemWl as _};
use smithay::backend::session::Event as SessionEvent;
use smithay::backend::session::Session as _;
use smithay::backend::session::libseat::{LibSeatSession, LibSeatSessionNotifier};
use smithay::backend::udev::{UdevBackend, UdevEvent, primary_gpu};
use smithay::desktop::utils::OutputPresentationFeedback;
use smithay::output::{Mode as WlMode, Output, PhysicalProperties};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{LoopHandle, RegistrationToken};
use smithay::reexports::drm::control::{ModeTypeFlags, connector, crtc};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::utils::{DeviceFd, Monotonic, Time};
use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal};
use smithay::wayland::presentation::Refresh;
use smithay_drm_extras::display_info;
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};
use tracing::{debug, error, info, warn};

use crate::backend::{Backend, RenderResult};
use crate::core::Core;
use crate::core::redraw::RedrawState;
use crate::core::render::{self, AsGlesRenderer, OutputElements, PolyRenderer};
use crate::state::State;

/// Color formats we are happy to scan out, best first.
///
/// We do not pick from the intersection of all supported formats: low bit
/// depth formats would lose color, and some formats perform badly.
const COLOR_FORMATS: &[Fourcc] = &[
    Fourcc::Abgr2101010,
    Fourcc::Argb2101010,
    Fourcc::Abgr8888,
    Fourcc::Argb8888,
];

/// The renderer of the udev backend, always backed by GBM/GLES.
pub type UdevRenderer<'a> = MultiRenderer<
    'a,
    'a,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
>;

/// GL errors of a [`UdevRenderer`], as reported by its frames.
type UdevError = multigpu::Error<
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
>;

impl AsGlesRenderer for UdevRenderer<'_> {
    fn as_gles_renderer(&mut self) -> &mut GlesRenderer {
        return self.as_mut();
    }
}

impl PolyRenderer for UdevRenderer<'_> {
    fn as_gles_frame<'frame, 'buffer, 'a>(
        frame: &'a mut Self::Frame<'frame, 'buffer>,
    ) -> &'a mut GlesFrame<'frame, 'buffer>
    where
        'buffer: 'frame,
    {
        return frame.as_mut();
    }

    fn gles_error(err: GlesError) -> UdevError {
        return err.into();
    }
}

/// The per-CRTC DRM output.
type UdevOutput = DrmOutput<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    Option<OutputPresentationFeedback>,
    DrmDeviceFd,
>;

/// Allocates and export framebuffers for the outputs of one device.
type UdevOutputManager = DrmOutputManager<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    Option<OutputPresentationFeedback>,
    DrmDeviceFd,
>;

/// Identifies the CRTC an output belongs to, for the DRM callbacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UdevOutputId {
    node: DrmNode,
    crtc: crtc::Handle,
}

/// Native backend state.
pub struct Udev {
    /// The libseat session; all device access goes through it.
    session: LibSeatSession,
    /// Device node of the primary GPU (not necessarily its render node).
    primary_gpu: DrmNode,
    /// Render node of the primary GPU, once its device came up.
    primary_render_node: Option<DrmNode>,
    /// Renderers, one per DRM node.
    gpus: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    /// DRM devices we manage, keyed by their device node.
    devices: HashMap<DrmNode, DeviceState>,
    /// The dmabuf global; needs the primary render node's formats.
    _dmabuf_global: Option<DmabufGlobal>,
    /// The libinput handle, for suspending/resuming with the session.
    libinput: Libinput,
    /// Event loop handle, for timers.
    handle: LoopHandle<'static, State>,
}

/// One open DRM device.
struct DeviceState {
    /// The DRM event source (VBlanks and errors), removed when the device goes away.
    notifier_token: RegistrationToken,
    /// Render node of this device.
    render_node: DrmNode,
    /// Output manager: creates outputs, and locks the whole device for changes.
    manager: UdevOutputManager,
    /// Connector/CRTC bookkeeping.
    scanner: DrmScanner,
    /// Enabled outputs, keyed by their CRTC.
    outputs: HashMap<crtc::Handle, OutputDevice>,
}

/// One enabled output on a CRTC.
struct OutputDevice {
    /// The wayland output.
    output: Output,
    /// The DRM output driving this CRTC.
    drm_output: UdevOutput,
    /// The connector's modes, kept so a runtime mode change can find the DRM
    /// mode that matches a Wayland one again.
    modes: Vec<smithay::reexports::drm::control::Mode>,
    /// Whether this output is running with variable refresh rate.
    ///
    /// Kept here because the per-frame plane flags depend on it: under VRR a
    /// cursor-only update should not force a presented frame.
    vrr: bool,
}

impl Drop for OutputDevice {
    fn drop(&mut self) {
        self.output.leave_all();
    }
}

/// What one call to [`Udev::render`] produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenderOutcome {
    /// A frame was queued to KMS.
    Submitted,
    /// Nothing changed, no frame was queued.
    NoDamage,
    /// Rendering or queueing failed.
    Failed,
}

impl Udev {
    /// Open the seat, enumerate devices and start input.
    pub fn new(handle: &LoopHandle<'static, State>, core: &mut Core) -> anyhow::Result<Self> {
        let (session, session_notifier) =
            LibSeatSession::new().context("error opening a libseat session")?;
        let seat_name = session.seat();
        info!("using seat {seat_name}");

        let Some(primary_path) =
            primary_gpu(&seat_name).context("error looking up the primary GPU")?
        else {
            bail!("no GPU found on seat {seat_name}");
        };
        let primary_gpu = DrmNode::from_path(&primary_path)
            .with_context(|| return format!("error resolving {}", primary_path.display()))?;

        let udev_backend =
            UdevBackend::new(&seat_name).context("error creating the udev backend")?;
        let device_list = udev_backend
            .device_list()
            .map(|(device_id, path)| return (device_id, path.to_owned()))
            .collect::<Vec<(u64, PathBuf)>>();

        // libinput drives the seat; the session makes sure we lose access when
        // the VT is switched away.
        let libinput_session = LibinputSessionInterface::from(session.clone());
        let mut libinput = Libinput::new_with_udev(libinput_session);
        libinput
            .udev_assign_seat(&seat_name)
            .map_err(|_| return anyhow!("error assigning libinput to seat {seat_name}"))?;
        let libinput_backend = LibinputInputBackend::new(libinput.clone());

        let gpus = GpuManager::new(GbmGlesBackend::with_context_priority(ContextPriority::High))
            .map_err(|err| return anyhow!("error creating the GPU manager: {err}"))?;

        let mut udev = Self {
            session,
            primary_gpu,
            primary_render_node: None,
            gpus,
            devices: HashMap::new(),
            _dmabuf_global: None,
            libinput,
            handle: handle.clone(),
        };

        insert_sources(handle, libinput_backend, session_notifier, udev_backend)?;
        udev.bring_up_devices(core, device_list);
        udev.create_dmabuf_global(core);

        return Ok(udev);
    }

    /// Open every device, primary GPU first.
    fn bring_up_devices(&mut self, core: &mut Core, device_list: Vec<(u64, PathBuf)>) {
        let primary_dev_id = self.primary_gpu.dev_id();
        let (mut primary, mut others): (Vec<_>, Vec<_>) =
            device_list.into_iter().partition(|(device_id, _)| {
                return DrmNode::from_dev_id(*device_id)
                    .map(|node| return node.dev_id() == primary_dev_id)
                    .unwrap_or(false);
            });
        primary.append(&mut others);

        for (device_id, path) in primary {
            let node = match DrmNode::from_dev_id(device_id) {
                Ok(node) => node,
                Err(err) => {
                    warn!("error resolving device {device_id}: {err}");
                    continue;
                }
            };
            if let Err(err) = self.device_added(core, node, &path) {
                warn!("error adding device {}: {err:?}", path.display());
            }
        }
    }

    /// Publish dmabuf and shm formats once a renderer exists.
    fn create_dmabuf_global(&mut self, core: &mut Core) {
        let Some(render_node) = self.primary_render_node else {
            warn!("no render node; clients will fall back to shared memory buffers");
            return;
        };
        let renderer = match self.gpus.single_renderer(&render_node) {
            Ok(renderer) => renderer,
            Err(err) => {
                warn!("error creating a renderer for {render_node:?}: {err}");
                return;
            }
        };

        // Shared memory buffers are imported into the primary renderer.
        core.shm_state.update_formats(renderer.shm_formats());

        let feedback =
            match DmabufFeedbackBuilder::new(render_node.dev_id(), renderer.dmabuf_formats())
                .build()
            {
                Ok(feedback) => feedback,
                Err(err) => {
                    warn!("error building dmabuf feedback: {err:?}");
                    return;
                }
            };
        self._dmabuf_global = Some(
            core.dmabuf_state
                .create_global_with_default_feedback::<State>(&core.display_handle, &feedback),
        );
    }

    /// Open a DRM device and set up its outputs.
    fn device_added(&mut self, core: &mut Core, node: DrmNode, path: &Path) -> anyhow::Result<()> {
        if self.devices.contains_key(&node) {
            debug!("device {node:?} is already known");
            return Ok(());
        }

        let fd = self
            .session
            .open(
                path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .with_context(|| return format!("error opening {} via libseat", path.display()))?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));

        // `false`: leave the current connector state alone. Smithay enables and
        // disables connectors as they get attached to outputs.
        let (drm, notifier) =
            DrmDevice::new(fd.clone(), false).context("error creating the DRM device")?;
        let gbm = GbmDevice::new(fd).context("error creating the GBM device")?;

        let notifier_token = self
            .handle
            .insert_source(
                notifier,
                move |event, metadata, state: &mut State| match event {
                    DrmEvent::VBlank(crtc) => {
                        let State { backend, core, .. } = state;
                        if let Backend::Udev(udev) = backend {
                            udev.frame_finish(core, node, crtc, metadata.as_ref());
                        }
                    }
                    DrmEvent::Error(err) => error!("DRM error on {node:?}: {err}"),
                },
            )
            .map_err(|err| return anyhow!("error inserting the DRM event source: {err:?}"))?;

        // Find the render node of this device: the EGL device knows which node
        // can render for it.
        //
        // SAFETY: the EGL display is used only while the GBM device (which it
        // borrows from) is alive: both are dropped before this function returns
        // or owned by the output manager afterwards.
        let egl_display = unsafe { EGLDisplay::new(gbm.clone()) }
            .context("error creating an EGL display for the DRM device")?;
        let egl_device = EGLDevice::device_for_display(&egl_display)
            .context("error looking up the EGL device")?;
        if egl_device.is_software() {
            bail!(
                "device {} uses a software renderer, which is not supported",
                path.display()
            );
        }
        let render_node = egl_device
            .try_get_render_node()
            .ok()
            .flatten()
            .unwrap_or(node);
        drop(egl_display);

        self.gpus
            .as_mut()
            .add_node(render_node, gbm.clone())
            .map_err(|err| {
                return anyhow!("error adding {render_node:?} to the GPU manager: {err}");
            })?;
        if node.dev_id() == self.primary_gpu.dev_id() {
            self.primary_render_node = Some(render_node);
        }

        // The compositor's swapchain can only be allocated in formats the
        // renderer can render into.
        let renderer_formats: FormatSet = match self.gpus.single_renderer(&render_node) {
            Ok(mut renderer) => renderer
                .as_mut()
                .egl_context()
                .dmabuf_render_formats()
                .iter()
                .copied()
                .collect(),
            Err(err) => {
                bail!("error creating a renderer for {render_node:?}: {err}");
            }
        };

        let allocator = GbmAllocator::new(
            gbm.clone(),
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );
        let exporter = GbmFramebufferExporter::new(gbm.clone(), Some(render_node).into());
        let manager = UdevOutputManager::new(
            drm,
            allocator,
            exporter,
            Some(gbm),
            COLOR_FORMATS.iter().copied(),
            renderer_formats,
        );

        info!("device {node:?} ready (render node {render_node:?})");
        self.devices.insert(
            node,
            DeviceState {
                notifier_token,
                render_node,
                manager,
                scanner: DrmScanner::new(),
                outputs: HashMap::new(),
            },
        );

        self.device_changed(core, node);
        return Ok(());
    }

    /// Drop a device that went away.
    fn device_removed(&mut self, core: &mut Core, node: DrmNode) {
        let Some(device) = self.devices.remove(&node) else {
            return;
        };
        info!("device {node:?} removed");

        self.handle.remove(device.notifier_token);
        for (_, output_device) in device.outputs {
            let output = output_device.output.clone();
            core.space.unmap_output(&output);
            if let Some(state) = core.output_state.remove(&output) {
                if let Some(global) = state.global {
                    core.display_handle.remove_global::<State>(global);
                }
            }
            core.lock_backgrounds.remove(&output);
            core.xray_buffers.remove(&output);
            core.lock_surfaces
                .retain(|(lock_output, _)| return lock_output != &output);
        }

        core.queue_redraw_all();
    }

    /// Rescan the connectors of a device and react to changes.
    fn device_changed(&mut self, core: &mut Core, node: DrmNode) {
        let events = {
            let Some(device) = self.devices.get_mut(&node) else {
                return;
            };
            let DeviceState {
                scanner, manager, ..
            } = device;
            match scanner.scan_connectors(manager.device()) {
                Ok(result) => result.into_iter().collect::<Vec<DrmScanEvent>>(),
                Err(err) => {
                    warn!("error scanning connectors of {node:?}: {err}");
                    return;
                }
            }
        };

        for event in events {
            match event {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => self.connector_connected(core, node, connector, crtc),
                DrmScanEvent::Disconnected {
                    connector,
                    crtc: Some(crtc),
                } => self.connector_disconnected(core, node, connector, crtc),
                // The connector's mode list changed, which is how a mode that
                // `outputs.<name>.mode` asked for can appear after the output
                // was already running.
                DrmScanEvent::Changed {
                    connector,
                    crtc: Some(crtc),
                } => self.connector_modes_changed(core, node, connector, crtc),
                // A connector without a CRTC cannot be used.
                _ => (),
            }
        }
    }

    /// Refresh the mode list of an output whose connector changed.
    fn connector_modes_changed(
        &mut self,
        core: &mut Core,
        node: DrmNode,
        connector: connector::Info,
        crtc: crtc::Handle,
    ) {
        let Some(device) = self.devices.get_mut(&node) else {
            return;
        };
        let Some(output_device) = device.outputs.get_mut(&crtc) else {
            return;
        };
        let output = output_device.output.clone();
        output_device.modes = connector.modes().to_vec();
        // New timings are advertised to clients; Smithay cannot withdraw a
        // mode from the list again, so gone timings stay until the output goes
        // away.
        for mode in connector.modes() {
            output.add_mode(WlMode::from(*mode));
        }
        debug!(
            "modes of connector {:?} changed, {} mode(s) now",
            connector.handle(),
            connector.modes().len()
        );
        // The configured mode may have been the one that just appeared.
        core.output_config_pending = true;
    }

    /// Turn a connected connector into an output.
    fn connector_connected(
        &mut self,
        core: &mut Core,
        node: DrmNode,
        connector: connector::Info,
        crtc: crtc::Handle,
    ) {
        let Some(device) = self.devices.get_mut(&node) else {
            return;
        };
        if device.outputs.contains_key(&crtc) {
            return;
        }

        let render_node = device.render_node;
        let mut renderer = match self.gpus.single_renderer(&render_node) {
            Ok(renderer) => renderer,
            Err(err) => {
                warn!("error creating a renderer for {render_node:?}: {err}");
                return;
            }
        };

        let Some(&drm_mode) = connector
            .modes()
            .iter()
            .find(|mode| return mode.mode_type().contains(ModeTypeFlags::PREFERRED))
            .or_else(|| return connector.modes().first())
        else {
            warn!("connector {:?} has no modes", connector.handle());
            return;
        };
        let wl_mode = WlMode::from(drm_mode);

        let output_name = format!(
            "{}-{}",
            connector.interface().as_str(),
            connector.interface_id()
        );
        let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
        let display_info = display_info::for_connector(device.manager.device(), connector.handle());
        let output = Output::new(
            output_name.clone(),
            PhysicalProperties {
                size: (phys_w as i32, phys_h as i32).into(),
                subpixel: connector.subpixel().into(),
                make: display_info
                    .as_ref()
                    .and_then(|info| return info.make())
                    .unwrap_or_else(|| return "Unknown".into()),
                model: display_info
                    .as_ref()
                    .and_then(|info| return info.model())
                    .unwrap_or_else(|| return "Unknown".into()),
                serial_number: display_info
                    .as_ref()
                    .and_then(|info| return info.serial())
                    .unwrap_or_else(|| return "Unknown".into()),
            },
        );

        let global = output.create_global::<State>(&core.display_handle);
        core.output_state(&output).global = Some(global.clone());
        output.set_preferred(wl_mode);
        // Advertise every timing the connector reports, not just the preferred
        // one: `outputs.<name>.mode` picks out of this list, and so does
        // `poly msg output <name> mode`.
        for mode in connector.modes() {
            output.add_mode(WlMode::from(*mode));
        }

        // Place new outputs to the right of the existing ones.
        let position = (
            core.space
                .outputs()
                .filter_map(|output| return core.space.output_geometry(output))
                .fold(0, |acc, geometry| return acc + geometry.size.w),
            0,
        )
            .into();
        output.change_current_state(Some(wl_mode), None, None, Some(position));
        core.space.map_output(&output, position);
        output.user_data().insert_if_missing(|| {
            return UdevOutputId { node, crtc };
        });

        let planes = match device.manager.device().planes(&crtc) {
            Ok(planes) => Some(planes),
            Err(err) => {
                warn!("error querying the CRTC planes of {output_name}: {err}");
                None
            }
        };

        // VRR has to be turned on while the manager is locked, because the flag
        // lives on the `DrmCompositor`, which owns the CRTC's VRR property. Doing
        // it in the same lock as the output's creation keeps the output from ever
        // presenting a frame with VRR on and a fixed-grid frame clock, or the
        // other way round.
        //
        // `RequiresModeset` counts as "not available": enabling it would blank the
        // output right as it comes up, which is a poor trade for a smoothness
        // feature the session works without.
        let (drm_output, vrr) = {
            let mut manager = device.manager.lock();
            let drm_output = manager.initialize_output::<_, OutputElements<UdevRenderer<'_>>>(
                crtc,
                drm_mode,
                &[connector.handle()],
                &output,
                planes,
                &mut renderer,
                &DrmOutputRenderElements::default(),
            );
            let vrr = drm_output.is_ok()
                && manager
                    .compositors()
                    .get(&crtc)
                    .and_then(|compositor| return compositor.lock().ok())
                    .is_some_and(|mut compositor| {
                        if !matches!(
                            compositor.vrr_supported(connector.handle()),
                            Ok(VrrSupport::Supported)
                        ) {
                            return false;
                        }
                        match compositor.use_vrr(true) {
                            Ok(()) => {
                                info!("variable refresh rate enabled on {output_name}");
                                true
                            }
                            Err(err) => {
                                debug!("could not enable VRR on {output_name}: {err:?}");
                                false
                            }
                        }
                    });
            (drm_output, vrr)
        };
        let drm_output = match drm_output {
            Ok(drm_output) => drm_output,
            Err(err) => {
                warn!("error initializing output {output_name}: {err}");
                output.leave_all();
                core.space.unmap_output(&output);
                core.display_handle.remove_global::<State>(global);
                core.output_state.remove(&output);
                core.lock_backgrounds.remove(&output);
                core.xray_buffers.remove(&output);
                core.lock_surfaces
                    .retain(|(lock_output, _)| return lock_output != &output);
                return;
            }
        };

        info!(
            "output {output_name} enabled ({}x{}@{:.1} Hz)",
            wl_mode.size.w,
            wl_mode.size.h,
            wl_mode.refresh as f64 / 1000.0
        );

        // The frame clock is the only other thing that has to know: under VRR the
        // panel presents when a frame is ready, so animations must not be sampled
        // on the nominal refresh grid (`FrameClock::set_vrr`).
        core.output_state(&output).frame_clock.set_vrr(vrr);

        device.outputs.insert(
            crtc,
            OutputDevice {
                output: output.clone(),
                drm_output,
                modes: connector.modes().to_vec(),
                vrr,
            },
        );
        core.queue_redraw(&output);
        // A hotplug carries only `&mut Core`, so the `outputs` section is
        // applied by the event loop once the whole `State` is back.
        core.output_config_pending = true;
    }

    /// Drop an output whose connector went away.
    fn connector_disconnected(
        &mut self,
        core: &mut Core,
        node: DrmNode,
        connector: connector::Info,
        crtc: crtc::Handle,
    ) {
        let Some(device) = self.devices.get_mut(&node) else {
            return;
        };
        let Some(output_device) = device.outputs.remove(&crtc) else {
            return;
        };

        info!(
            "output {}-{} disconnected",
            connector.interface().as_str(),
            connector.interface_id()
        );
        let output = output_device.output.clone();
        drop(output_device);
        core.space.unmap_output(&output);
        if let Some(state) = core.output_state.remove(&output) {
            if let Some(global) = state.global {
                core.display_handle.remove_global::<State>(global);
            }
        }
        core.lock_backgrounds.remove(&output);
        core.xray_buffers.remove(&output);
        core.lock_surfaces
            .retain(|(lock_output, _)| return lock_output != &output);
        core.queue_redraw_all();
        // `outputs.<name>.focus` may have named the output that just went
        // away; the event loop drops it.
        core.output_config_pending = true;
    }

    /// Access the GLES renderer that draws `output` for one-shot offscreen work.
    ///
    /// A texture is tied to the renderer that created it. Looking the renderer
    /// up from the output's own DRM node keeps close snapshots working when an
    /// output is driven by a secondary GPU instead of the primary render node.
    pub fn with_gles_renderer_for_output<R>(
        &mut self,
        output: &Output,
        f: impl FnOnce(&mut GlesRenderer) -> R,
    ) -> Option<R> {
        let id = output.user_data().get::<UdevOutputId>().copied()?;
        let render_node = self.devices.get(&id.node)?.render_node;
        let mut renderer = self.gpus.single_renderer(&render_node).ok()?;
        return Some(f(renderer.as_gles_renderer()));
    }

    /// Render one output.
    pub fn render(&mut self, core: &mut Core, output: &Output) -> RenderResult {
        let Some(id) = output.user_data().get::<UdevOutputId>().copied() else {
            return RenderResult::Skipped;
        };

        let Some(render_node) = self
            .devices
            .get(&id.node)
            .map(|device| return device.render_node)
        else {
            return RenderResult::Skipped;
        };
        let mut renderer = match self.gpus.single_renderer(&render_node) {
            Ok(renderer) => renderer,
            Err(err) => {
                warn!("error creating a renderer for {render_node:?}: {err}");
                return RenderResult::Skipped;
            }
        };

        let Some(device) = self.devices.get_mut(&id.node) else {
            return RenderResult::Skipped;
        };
        // Rendering is not possible while the session is paused (the VT was
        // switched away): the frame waits for the session to come back.
        if !device.manager.device().is_active() {
            return RenderResult::Skipped;
        }
        let Some(output_device) = device.outputs.get_mut(&id.crtc) else {
            return RenderResult::Skipped;
        };

        let outcome = {
            let now = core.begin_frame(output);
            let background = core
                .fullscreen_clear(output)
                .or_else(|| {
                    return core
                        .output_background(output)
                        .map(|color| return color.to_floats());
                })
                .unwrap_or([0.0, 0.0, 0.0, 1.0]);
            let elements =
                render::output_elements(core, &mut renderer, output, true, now, background);

            // Direct scan-out is offered only for frames that can actually use
            // it: a single fullscreen window with nothing composited over or
            // under it (`render::may_scan_out`). The DRM compositor checks the
            // same conditions again and falls back to rendering when they do not
            // hold, so a wrong answer here only costs a failed attempt. Passing
            // the flag unconditionally would leave every composited frame
            // paying for that attempt.
            let output_geometry = core.space.output_geometry(output);
            let scanout_possible = output_geometry.is_some_and(|geometry| {
                return render::may_scan_out(
                    &elements,
                    geometry,
                    output.current_scale().fractional_scale(),
                );
            });
            let mut frame_flags = FrameFlags::empty();
            if scanout_possible {
                // `ANY` because the client's buffer format rarely matches the
                // swapchain's: requiring an exact match would make scan-out
                // succeed only for the format we happen to allocate.
                frame_flags.insert(FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT_ANY);
            }
            // A hardware cursor plane keeps the pointer from forcing a full
            // composite of the frame it moves across. The cursor element is
            // pushed first, i.e. it is already on top of everything, which is
            // the only placement the plane can express; whether it fits and
            // which plane to use is the DRM compositor's call.
            let cursor_plane_possible =
                matches!(elements.first(), Some(render::OutputElements::Cursor(_)));
            if cursor_plane_possible {
                frame_flags.insert(FrameFlags::ALLOW_CURSOR_PLANE_SCANOUT);
            }
            if core.config.general.enable_overlay_planes {
                frame_flags.insert(FrameFlags::ALLOW_OVERLAY_PLANE_SCANOUT);
            }
            // Under VRR a cursor-only update must not take a presentation slot:
            // the panel would refresh to show the pointer move, the refresh rate
            // would sit high while the desktop is idle, and the whole point of
            // VRR — sitting low when nothing changes — would be lost. The flag
            // makes the compositor skip drawing a frame whose only damage is the
            // cursor and move the pointer on the cursor plane instead.
            //
            // Only with a cursor plane: without one the pointer is part of the
            // composited frame, so skipping the frame would simply freeze it.
            if output_device.vrr && cursor_plane_possible {
                frame_flags.insert(FrameFlags::SKIP_CURSOR_ONLY_UPDATES);
            }
            match output_device.drm_output.render_frame(
                &mut renderer,
                &elements,
                background,
                frame_flags,
            ) {
                Ok(result) => {
                    if result.needs_sync() {
                        if let PrimaryPlaneElement::Swapchain(element) = &result.primary_element {
                            if let Err(err) = element.sync.wait() {
                                warn!("error waiting for frame completion: {err:?}");
                            }
                        }
                    }

                    if result.is_empty {
                        RenderOutcome::NoDamage
                    } else {
                        let feedback =
                            render::take_presentation_feedbacks(core, output, &result.states);
                        match output_device.drm_output.queue_frame(Some(feedback)) {
                            Ok(()) => RenderOutcome::Submitted,
                            Err(err) => {
                                warn!("error queueing a frame on {}: {err:?}", output.name());
                                RenderOutcome::Failed
                            }
                        }
                    }
                }
                Err(err) => {
                    warn!("error rendering {}: {err:?}", output.name());
                    RenderOutcome::Failed
                }
            }
        };

        match outcome {
            RenderOutcome::Submitted => {
                // Wait for the VBlank of the queued frame; a redraw coming in
                // while it is pending is rendered right after it.
                let previous = mem::replace(
                    &mut core.output_state(output).redraw_state,
                    RedrawState::WaitingForVBlank {
                        redraw_needed: false,
                    },
                );
                match previous {
                    RedrawState::Queued => (),
                    RedrawState::WaitingForEstimatedVBlankAndQueued(token) => {
                        // The frame is on its way to the screen, so the estimated
                        // vblank this timer was waiting for is moot.
                        self.handle.remove(token);
                    }
                    other => debug!(
                        "unexpected redraw state after queueing a frame on {}: {other:?}",
                        output.name()
                    ),
                }
                return RenderResult::Submitted;
            }
            RenderOutcome::NoDamage | RenderOutcome::Failed => {
                let frame_duration = frame_duration(output);
                self.queue_estimated_vblank_timer(core, output, frame_duration);
                return match outcome {
                    RenderOutcome::NoDamage => RenderResult::NoDamage,
                    _ => RenderResult::Skipped,
                };
            }
        }
    }

    /// The VBlank of a queued frame: send feedback and callbacks.
    fn frame_finish(
        &mut self,
        core: &mut Core,
        node: DrmNode,
        crtc: crtc::Handle,
        metadata: Option<&DrmEventMetadata>,
    ) {
        let Some(device) = self.devices.get_mut(&node) else {
            error!("vblank on unknown device {node:?}");
            return;
        };
        let Some(output_device) = device.outputs.get_mut(&crtc) else {
            error!("vblank on unknown crtc {crtc:?}");
            return;
        };
        let output = output_device.output.clone();
        let frame_duration = frame_duration(&output);

        match output_device.drm_output.frame_submitted() {
            Ok(Some(Some(mut feedback))) => {
                let (time, sequence, flags) = match metadata.map(|metadata| return metadata.time) {
                    Some(DrmEventTime::Monotonic(time)) if !time.is_zero() => (
                        Time::<Monotonic>::from(time),
                        metadata
                            .map(|metadata| return metadata.sequence)
                            .unwrap_or(0) as u64,
                        wp_presentation_feedback::Kind::Vsync
                            | wp_presentation_feedback::Kind::HwClock
                            | wp_presentation_feedback::Kind::HwCompletion,
                    ),
                    _ => (
                        Time::<Monotonic>::from(core.monotonic_now()),
                        0,
                        wp_presentation_feedback::Kind::Vsync,
                    ),
                };
                feedback.presented::<_, Monotonic>(
                    time,
                    Refresh::fixed(frame_duration),
                    sequence,
                    flags,
                );
            }
            Ok(_) => (),
            Err(err) => warn!("error finishing a frame on {}: {err:?}", output.name()),
        }

        // The DRM timestamp is on `CLOCK_MONOTONIC`; map it onto our `Instant`
        // timeline so the frame clock can predict the next VBlank. Without one
        // (a driver that does not report it), the moment we handled the event
        // is close enough, and the snap-to-grid math removes the jitter.
        let presented = match metadata.map(|metadata| return metadata.time) {
            Some(DrmEventTime::Monotonic(time)) if !time.is_zero() => {
                core.instant_at_monotonic(time)
            }
            _ => Instant::now(),
        };
        core.frame_presented(&output, presented);

        let time = core.monotonic_now();
        render::send_frame_callbacks(core, &output, time);

        // The frame is on the screen: leave the wait state, and render again if a
        // redraw was queued while it was pending.
        let previous = mem::replace(
            &mut core.output_state(&output).redraw_state,
            RedrawState::Idle,
        );
        let redraw_needed = match previous {
            RedrawState::WaitingForVBlank { redraw_needed } => redraw_needed,
            // A VBlank we did not expect (it can happen after resuming from
            // sleep): rendering an extra frame is cheaper than missing one.
            other => {
                error!(
                    "unexpected redraw state for output {} at VBlank: {other:?}",
                    output.name()
                );
                true
            }
        };
        if redraw_needed {
            core.queue_redraw(&output);
            return;
        }
        // An ongoing animation needs the next frame now that this one is on
        // screen. Driving it from the VBlank (instead of an independent
        // wall-clock timer) locks animation frames to the refresh rate, like
        // niri's `unfinished_animations_remain`.
        if core.layout.animations_running() {
            core.queue_redraw(&output);
        }
    }

    /// Nothing was submitted, so wake up again at the estimated VBlank.
    fn queue_estimated_vblank_timer(
        &mut self,
        core: &mut Core,
        output: &Output,
        duration: Duration,
    ) {
        // A timer that is already pending fires at the right time, so keep it and
        // clear the redraw it was waiting for: that redraw is being handled now.
        match mem::take(&mut core.output_state(output).redraw_state) {
            RedrawState::WaitingForEstimatedVBlank(token)
            | RedrawState::WaitingForEstimatedVBlankAndQueued(token) => {
                core.output_state(output).redraw_state =
                    RedrawState::WaitingForEstimatedVBlank(token);
                return;
            }
            _ => (),
        }

        let timer_output = output.clone();
        // `duration` is the output's own refresh interval, so the wake-up lands
        // on the refresh grid: clamping it to 60 Hz here would hold a 144 Hz
        // animation down to 60 frames per second.
        let timer = Timer::from_duration(duration);
        let token = match self
            .handle
            .insert_source(timer, move |_, _, state: &mut State| {
                let State { backend, core, .. } = state;
                if let Backend::Udev(udev) = backend {
                    udev.on_estimated_vblank_timer(core, &timer_output);
                }
                return TimeoutAction::Drop;
            }) {
            Ok(token) => token,
            Err(err) => {
                warn!("error scheduling the estimated vblank timer: {err:?}");
                return;
            }
        };

        core.output_state(output).redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
    }

    /// Timer callback at (roughly) the next VBlank.
    fn on_estimated_vblank_timer(&mut self, core: &mut Core, output: &Output) {
        let previous = mem::replace(
            &mut core.output_state(output).redraw_state,
            RedrawState::Idle,
        );
        match previous {
            RedrawState::WaitingForEstimatedVBlank(_) => {
                // Pretend a VBlank happened, so clients keep animating. The
                // timer fires at the estimated VBlank, so this is also the
                // frame clock's next grid point.
                let time = core.monotonic_now();
                core.frame_presented(output, Instant::now());
                render::send_frame_callbacks(core, output, time);
                // The estimated VBlank is a frame boundary too: an ongoing
                // animation continues from it, exactly like after a real
                // VBlank in `frame_finish`.
                if core.layout.animations_running() {
                    core.queue_redraw(output);
                }
            }
            RedrawState::WaitingForEstimatedVBlankAndQueued(_) => {
                // A redraw was queued while we waited; render it now.
                core.output_state(output).redraw_state = RedrawState::Queued;
            }
            other => core.output_state(output).redraw_state = other,
        }
    }

    /// React to the session being paused (VT switched away) or resumed.
    fn on_session_event(&mut self, core: &mut Core, event: SessionEvent) {
        match event {
            SessionEvent::PauseSession => {
                debug!("session paused");
                self.libinput.suspend();
                for device in self.devices.values_mut() {
                    device.manager.lock().pause();
                }
            }
            SessionEvent::ActivateSession => {
                debug!("session resumed");
                if self.libinput.resume().is_err() {
                    warn!("error resuming libinput");
                }
                for (node, device) in self.devices.iter_mut() {
                    if let Err(err) = device.manager.lock().activate(false) {
                        warn!("error activating DRM device {node:?}: {err:?}");
                    }
                }

                // Connectors may have changed while we were away.
                let nodes = self.devices.keys().copied().collect::<Vec<_>>();
                for node in nodes {
                    self.device_changed(core, node);
                }
                core.queue_redraw_all();
            }
        }
    }

    /// Switch virtual terminal.
    pub fn change_vt(&mut self, vt: i32) {
        if let Err(err) = self.session.change_vt(vt) {
            warn!("error switching to VT {vt}: {err}");
        }
    }

    /// Program a new mode on the CRTC that drives `output`.
    ///
    /// `DrmOutput::use_mode` commits the new mode straight away, and only
    /// re-composites a frame if that commit is rejected (a bandwidth problem on
    /// some setups); the render elements are therefore only a fallback, and an
    /// empty set makes that one frame black.
    pub fn set_output_mode(&mut self, output: &Output, mode: WlMode) -> Result<(), String> {
        let Some(id) = output.user_data().get::<UdevOutputId>().copied() else {
            return Err(format!("output {} is not a DRM output", output.name()));
        };
        let Some(render_node) = self
            .devices
            .get(&id.node)
            .map(|device| return device.render_node)
        else {
            return Err(format!("the device of {} is gone", output.name()));
        };
        let mut renderer = match self.gpus.single_renderer(&render_node) {
            Ok(renderer) => renderer,
            Err(err) => {
                return Err(format!(
                    "error creating a renderer for {render_node:?}: {err}"
                ));
            }
        };

        let Some(device) = self.devices.get_mut(&id.node) else {
            return Err(format!("the device of {} is gone", output.name()));
        };
        if !device.manager.device().is_active() {
            return Err("the session is paused".to_owned());
        }
        let Some(output_device) = device.outputs.get_mut(&id.crtc) else {
            return Err(format!("{} has no CRTC any more", output.name()));
        };
        // Wayland mode and DRM mode are different types: match on the timing,
        // which is what `poly msg output ... mode` was given.
        let Some(drm_mode) = output_device
            .modes
            .iter()
            .find(|candidate| return WlMode::from(**candidate) == mode)
            .copied()
        else {
            return Err(format!(
                "{} has no mode {}x{}@{}",
                output.name(),
                mode.size.w,
                mode.size.h,
                mode.refresh
            ));
        };
        return use_mode(&mut output_device.drm_output, drm_mode, &mut renderer);
    }

    /// Enable or disable variable refresh rate on the connector of `output`.
    ///
    /// `DrmCompositor::use_vrr` programs the connector property and takes
    /// effect with the next commit, which the caller's redraw provides.
    pub fn set_output_vrr(&mut self, output: &Output, vrr: bool) -> Result<(), String> {
        let Some(id) = output.user_data().get::<UdevOutputId>().copied() else {
            return Err(format!("output {} is not a DRM output", output.name()));
        };
        let Some(device) = self.devices.get_mut(&id.node) else {
            return Err(format!("the device of {} is gone", output.name()));
        };
        let Some(output_device) = device.outputs.get_mut(&id.crtc) else {
            return Err(format!("{} has no CRTC any more", output.name()));
        };
        let result = output_device
            .drm_output
            .with_compositor(|compositor| return compositor.use_vrr(vrr));
        return match result {
            Ok(()) => Ok(()),
            Err(err) => Err(format!(
                "error {} VRR on {}: {err:?}",
                match vrr {
                    true => "enabling",
                    false => "disabling",
                },
                output.name()
            )),
        };
    }
}

/// Commit `mode` on the DRM output.
fn use_mode(
    drm_output: &mut UdevOutput,
    mode: smithay::reexports::drm::control::Mode,
    renderer: &mut UdevRenderer<'_>,
) -> Result<(), String> {
    let elements =
        DrmOutputRenderElements::<UdevRenderer<'_>, OutputElements<UdevRenderer<'_>>>::new();
    return drm_output
        .use_mode(mode, renderer, &elements)
        .map_err(|err| return format!("error setting mode {mode:?}: {err:?}"));
}

/// Hook input, session and device hotplug into the event loop.
fn insert_sources(
    handle: &LoopHandle<'static, State>,
    libinput_backend: LibinputInputBackend,
    session_notifier: LibSeatSessionNotifier,
    udev_backend: UdevBackend,
) -> anyhow::Result<()> {
    handle
        .insert_source(libinput_backend, |event, _, state: &mut State| {
            crate::core::input::process_libinput_event(state, event);
        })
        .map_err(|err| return anyhow!("error inserting the libinput source: {err:?}"))?;

    handle
        .insert_source(session_notifier, |event, _, state: &mut State| {
            let State { backend, core, .. } = state;
            if let Backend::Udev(udev) = backend {
                udev.on_session_event(core, event);
            }
        })
        .map_err(|err| return anyhow!("error inserting the session source: {err:?}"))?;

    handle
        .insert_source(udev_backend, |event, _, state: &mut State| {
            let State { backend, core, .. } = state;
            let Backend::Udev(udev) = backend else {
                return;
            };
            match event {
                UdevEvent::Added { device_id, path } => match DrmNode::from_dev_id(device_id) {
                    Ok(node) => {
                        if let Err(err) = udev.device_added(core, node, &path) {
                            warn!("error adding {}: {err:?}", path.display());
                        }
                    }
                    Err(err) => warn!("error resolving device {device_id}: {err}"),
                },
                UdevEvent::Changed { device_id } => {
                    if let Ok(node) = DrmNode::from_dev_id(device_id) {
                        udev.device_changed(core, node);
                    }
                }
                UdevEvent::Removed { device_id } => {
                    if let Ok(node) = DrmNode::from_dev_id(device_id) {
                        udev.device_removed(core, node);
                    }
                }
            }
        })
        .map_err(|err| return anyhow!("error inserting the udev source: {err:?}"))?;

    return Ok(());
}

/// How long one frame of `output` lasts.
///
/// Shared with the animation clock so a 90/120/144 Hz output is not held back
/// to 60 Hz (`core::redraw::frame_interval`).
fn frame_duration(output: &Output) -> Duration {
    return crate::core::redraw::frame_interval(output);
}
