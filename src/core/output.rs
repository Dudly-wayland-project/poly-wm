//! Output configuration: the `outputs` section of the configuration file, plus
//! the runtime changes `poly msg output` can make (`docs/plan.md` §12).
//!
//! Two entry points share one implementation:
//!
//! - [`apply_config`] runs at startup, on `reload-config` and when a new output
//!   connects. It walks every connected output, resolves the `outputs.<pattern>`
//!   entry that matches it (first match wins, most specific pattern first) and
//!   applies mode, scale, transform, position, VRR, background colour and the
//!   `focus` flag.
//! - [`apply`] applies a single runtime change from IPC. Such a change is
//!   transient: `reload-config` puts the file's values back.
//!
//! Every path ends with the same settle step: re-arrange the layer maps (their
//! cached non-exclusive zone depends on the size), re-flow the windows, resize
//! the lock surface and queue a redraw. Forget one of those and windows keep
//! the old size until something else touches the layout.

use poly_config::{OutputConfig, OutputMode, OutputPosition, OutputScale, OutputTransform};
use poly_ipc::{OutputAction, OutputConfigChanged};
use smithay::desktop::layer_map_for_output;
use smithay::output::{Mode, Output, Scale};
use smithay::utils::{Logical, Point, Transform};
use smithay::wayland::seat::WaylandFocus as _;
use tracing::{debug, info, warn};

use crate::state::State;

/// Apply the `outputs` section of the configuration to every connected output.
///
/// Runs at startup (after the backend created its outputs and before the first
/// frame), on `reload-config` and after a hotplug. Values that do not apply are
/// logged and skipped, so one wrong line cannot keep the screen black: the
/// output keeps whatever it had for that field.
pub fn apply_config(state: &mut State) {
    let outputs = state.core.space.outputs().cloned().collect::<Vec<_>>();
    let mut preferred = None;
    for output in outputs {
        // `output_for` hands back an owned, merged copy, so the borrow of the
        // configuration does not live across the `&mut State` calls below.
        let Some(config) = state.core.config.output_for(&output.name()) else {
            continue;
        };
        if config.focus == Some(true) && preferred.is_none() {
            preferred = Some(output.clone());
        }
        apply_output(state, &output, &config);
    }

    // A `focus = true` output that is not connected anymore must not keep
    // claiming the focus, and neither must one that lost the flag on reload.
    let preferred = preferred.filter(|output| {
        return state
            .core
            .space
            .outputs()
            .any(|candidate| return candidate == output);
    });
    if state.core.preferred_output != preferred {
        match &preferred {
            Some(output) => debug!("output {} is the configured focus output", output.name()),
            None => debug!("no output is marked as the focus output"),
        }
        state.core.preferred_output = preferred;
    }
}

/// Apply one already resolved [`OutputConfig`] to one output.
fn apply_output(state: &mut State, output: &Output, config: &OutputConfig) {
    let mut mode: Option<Mode> = None;
    let mut scale: Option<Scale> = None;
    let mut transform: Option<Transform> = None;
    let mut location: Option<Point<i32, Logical>> = None;

    // 1. Mode. The backend has to switch the hardware first (a KMS mode set for
    //    DRM), and only then does the compositor-side state follow.
    if let Some(wanted) = &config.mode {
        match resolve_mode(output, wanted) {
            Ok(resolved) => {
                if Some(resolved) != output.current_mode() {
                    if let Err(err) = state.backend.set_output_mode(output, resolved) {
                        warn!(
                            "cannot switch {} to mode {resolved:?}: {err}",
                            output.name()
                        );
                    } else {
                        output.set_preferred(resolved);
                        mode = Some(resolved);
                    }
                }
            }
            Err(message) => warn!("output {}: {message}", output.name()),
        }
    }

    // 2. Transform.
    if let Some(wanted) = config.transform {
        match state.backend.can_transform_output(output) {
            Ok(()) => {
                let wanted = to_smithay_transform(wanted);
                if output.current_transform() != wanted {
                    transform = Some(wanted);
                }
            }
            Err(message) => warn!("{message}; ignoring outputs.transform"),
        }
    }

    // 3. Scale.
    if let Some(wanted) = config.scale {
        match wanted {
            OutputScale::Auto => (),
            OutputScale::Fixed(factor) => {
                let scale_now = output.current_scale().fractional_scale();
                // The scale comes from a config file that may be reloaded on
                // every keystroke of an editor; comparing the rounded value
                // avoids a scale change (and its client reconfigure) for a
                // value that is unchanged up to float noise.
                if (scale_now - factor).abs() > 0.001 {
                    scale = Some(Scale::Fractional(factor));
                }
            }
        }
    }

    // 4. Position.
    if let Some(wanted) = config.position {
        match wanted {
            OutputPosition::Auto => (),
            OutputPosition::Fixed { x, y } => {
                let target = Point::from((x, y));
                if state
                    .core
                    .space
                    .output_geometry(output)
                    .map(|geometry| return geometry.loc)
                    != Some(target)
                {
                    location = Some(target);
                    state.core.space.map_output(output, target);
                }
            }
        }
    }

    let changed = mode.is_some() || scale.is_some() || transform.is_some() || location.is_some();
    if changed {
        output.change_current_state(mode, transform, scale, location);
        info!(
            "output {} configured: {}x{}@{}, scale {}, transform {:?}",
            output.name(),
            output
                .current_mode()
                .map(|mode| return mode.size.w)
                .unwrap_or(0),
            output
                .current_mode()
                .map(|mode| return mode.size.h)
                .unwrap_or(0),
            output
                .current_mode()
                .map(|mode| return mode.refresh)
                .unwrap_or(0),
            output.current_scale().fractional_scale(),
            output.current_transform(),
        );
        settle(state, output);
        if scale.is_some() || transform.is_some() || mode.is_some() {
            propagate_scale(state, output);
        }
    }

    // 5. VRR.
    if let Some(vrr) = config.vrr {
        match state.backend.set_output_vrr(output, vrr) {
            Ok(()) => debug!("output {} VRR {}", output.name(), vrr),
            Err(message) => warn!("{message}; ignoring outputs.vrr"),
        }
    }

    // 6. Background colour.
    if let Some(color) = config.background {
        let state_entry = state.core.output_state(output);
        if state_entry.background != Some(color) {
            state_entry.background = Some(color);
            state.core.queue_redraw(output);
        }
    }
}

/// Translate the configuration transform into Smithay's.
fn to_smithay_transform(transform: OutputTransform) -> Transform {
    return match transform {
        OutputTransform::Normal => Transform::Normal,
        OutputTransform::Rotate90 => Transform::_90,
        OutputTransform::Rotate180 => Transform::_180,
        OutputTransform::Rotate270 => Transform::_270,
        OutputTransform::Flipped => Transform::Flipped,
        OutputTransform::Flipped90 => Transform::Flipped90,
        OutputTransform::Flipped180 => Transform::Flipped180,
        OutputTransform::Flipped270 => Transform::Flipped270,
    };
}

/// Pick the mode described by `wanted` out of the output's own mode list.
fn resolve_mode(output: &Output, wanted: &OutputMode) -> Result<Mode, String> {
    let modes = output.modes();
    let wanted = match wanted {
        OutputMode::Preferred => {
            return output.preferred_mode().ok_or_else(|| {
                return "the output reports no preferred mode".to_owned();
            });
        }
        OutputMode::Size {
            width,
            height,
            refresh,
        } => Mode {
            size: (i32::from(*width), i32::from(*height)).into(),
            // Only the size and the (optional) refresh rate are compared; the
            // flags come from the mode the output actually reports.
            refresh: refresh.map(|refresh| return refresh as i32).unwrap_or(0),
        },
    };

    let matching = modes
        .iter()
        .filter(|mode| return mode.size == wanted.size)
        .copied()
        .collect::<Vec<_>>();
    if matching.is_empty() {
        return Err(format!("no mode {}x{}", wanted.size.w, wanted.size.h));
    }

    // Without an explicit refresh rate, prefer the output's preferred mode and
    // otherwise the fastest timing of that size.
    let mode = match wanted.refresh {
        0 => matching
            .iter()
            .copied()
            .find(|mode| return Some(*mode) == output.preferred_mode())
            .or_else(|| {
                return matching
                    .iter()
                    .copied()
                    .max_by_key(|mode| return mode.refresh);
            }),
        refresh => matching
            .iter()
            .copied()
            .min_by_key(|mode| return mode.refresh.abs_diff(refresh)),
    };
    return mode.ok_or_else(|| {
        return format!(
            "no mode {}x{}@{}",
            wanted.size.w, wanted.size.h, wanted.refresh
        );
    });
}

/// Apply one runtime output change and report what happened.
pub fn apply(state: &mut State, name: &str, action: OutputAction) -> OutputConfigChanged {
    let Some(output) = state
        .core
        .space
        .outputs()
        .find(|output| return output.name() == name)
        .cloned()
    else {
        debug!("no output named {name:?} is connected");
        return OutputConfigChanged::OutputWasMissing;
    };

    return match action {
        OutputAction::Scale(scale) => set_scale(state, &output, scale),
        OutputAction::Position { x, y } => set_position(state, &output, x, y),
        OutputAction::Mode {
            width,
            height,
            refresh,
        } => set_mode(state, &output, width, height, refresh),
    };
}

/// Change the fractional scale.
fn set_scale(state: &mut State, output: &Output, scale: f64) -> OutputConfigChanged {
    // A guard rail, not a policy: 0 or a NaN would make every size a division
    // by zero in the render path.
    if !scale.is_finite() || !(0.1..=4.0).contains(&scale) {
        warn!("refusing to set scale {scale} on {}", output.name());
        return OutputConfigChanged::Rejected;
    }

    output.change_current_state(None, None, Some(Scale::Fractional(scale)), None);
    info!("output {} now uses scale {scale}", output.name());
    settle(state, output);
    propagate_scale(state, output);
    return OutputConfigChanged::Applied;
}

/// Move the output in compositor space.
fn set_position(state: &mut State, output: &Output, x: i32, y: i32) -> OutputConfigChanged {
    output.change_current_state(None, None, None, Some((x, y).into()));
    state.core.space.map_output(output, (x, y));
    info!("output {} moved to {x},{y}", output.name());
    settle(state, output);
    return OutputConfigChanged::Applied;
}

/// Switch the output to another mode out of its own mode list.
fn set_mode(
    state: &mut State,
    output: &Output,
    width: u16,
    height: u16,
    refresh: Option<u32>,
) -> OutputConfigChanged {
    let wanted = OutputMode::Size {
        width,
        height,
        refresh,
    };
    let mode = match resolve_mode(output, &wanted) {
        Ok(mode) => mode,
        Err(message) => {
            warn!("output {}: {message}", output.name());
            return OutputConfigChanged::Rejected;
        }
    };

    // The backend has to switch the hardware first: for DRM that is a KMS mode
    // set, for winit the size comes from the host window.
    if let Err(err) = state.backend.set_output_mode(output, mode) {
        warn!("cannot switch {} to mode {mode:?}: {err}", output.name());
        return OutputConfigChanged::Rejected;
    }

    output.change_current_state(Some(mode), None, None, None);
    output.set_preferred(mode);
    info!(
        "output {} now uses {}x{}@{}",
        output.name(),
        mode.size.w,
        mode.size.h,
        mode.refresh
    );
    settle(state, output);
    propagate_scale(state, output);
    return OutputConfigChanged::Applied;
}

/// Everything that has to happen after an output changed size, scale or
/// transform.
fn settle(state: &mut State, output: &Output) {
    // `LayerMap` caches its non-exclusive zone from the old mode, and the layout
    // work area reads that cache.
    layer_map_for_output(output).arrange();
    crate::core::layout::LayoutManager::relayout(state);
    crate::core::handlers::session_lock::reconfigure_lock_surfaces(state);
    state.core.queue_redraw(output);
}

/// Tell every surface on `output` the scale it should render at.
///
/// Poly sends neither `wl_surface.preferred_buffer_scale` nor the fractional
/// preferred scale anywhere else yet, so a client that maps before the first
/// scale change keeps rendering at 1x. Sending it here is what makes a runtime
/// scale change visible in client content instead of only in the compositor
/// chrome.
pub fn propagate_scale(state: &mut State, output: &Output) {
    let scale = output.current_scale();
    for window in state.core.space.elements() {
        let Some(surface) = window.wl_surface() else {
            continue;
        };
        if state
            .core
            .space
            .outputs_for_element(window)
            .contains(output)
        {
            crate::core::window::send_scale(&surface, scale);
        }
    }
    for surface in layer_map_for_output(output)
        .layers()
        .map(|layer| return layer.wl_surface().clone())
        .collect::<Vec<_>>()
    {
        crate::core::window::send_scale(&surface, scale);
    }
    for (lock_output, surface) in &state.core.lock_surfaces {
        if lock_output == output {
            crate::core::window::send_scale(surface.wl_surface(), scale);
        }
    }
}
