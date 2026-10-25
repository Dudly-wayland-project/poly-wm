// Ported from niri (GPL-3.0-or-later): src/handlers/layer_shell.rs
//
//! wlr-layer-shell: status bars, notifications, launchers and lock screens.
//!
//! Layer surfaces live in Smithay's per-output [`smithay::desktop::LayerMap`],
//! which computes their geometry (anchors, size, exclusive zones) and hands
//! the result to the renderer through `Space` (`docs/plan.md` §7, §10).

use smithay::desktop::{LayerSurface, layer_map_for_output};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use smithay::wayland::compositor::{get_parent, with_states};
use smithay::wayland::shell::wlr_layer::{
    KeyboardInteractivity, Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData,
    WlrLayerShellHandler, WlrLayerShellState,
};
use smithay::wayland::shell::xdg::PopupSurface;
use tracing::{debug, warn};

use crate::core::focus::update_keyboard_focus;
use crate::core::window::is_mapped;
use crate::state::State;

impl WlrLayerShellHandler for State {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        return &mut self.core.layer_shell_state;
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        wl_output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        let output = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| return self.core.space.outputs().next().cloned());
        let Some(output) = output else {
            warn!("no output for a new layer surface, closing it");
            surface.send_close();
            return;
        };

        debug!(?namespace, output = %output.name(), "new layer surface");

        let root = surface.wl_surface().clone();
        self.core.unmapped_layer_surfaces.insert(root.clone());
        let layer = LayerSurface::new(surface, namespace);
        let mut map = layer_map_for_output(&output);
        if let Err(err) = map.map_layer(&layer) {
            warn!("error mapping a layer surface: {err:?}");
        }
        drop(map);

        // The exclusive zone may have changed: windows move out of the bar.
        reposition_windows(self, &output);
        self.core.queue_redraw(&output);
    }

    fn new_popup(&mut self, parent: WlrLayerSurface, popup: PopupSurface) {
        if let Err(err) = self
            .core
            .popups
            .track_popup(smithay::desktop::PopupKind::Xdg(popup.clone()))
        {
            // Popups of layer surfaces (menus) are tracked like window popups
            // so that rendering and grabs see them.
            debug!("error tracking a layer popup: {err:?}");
            return;
        }
        let _ = parent;
    }

    fn ack_configure(
        &mut self,
        _surface: WlSurface,
        _configure: smithay::wayland::shell::wlr_layer::LayerSurfaceConfigure,
    ) {
        // Geometry is recomputed by `arrange` on the next commit.
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let root = surface.wl_surface().clone();
        self.core.unmapped_layer_surfaces.remove(&root);

        let mut removed_output = None;
        for output in self.core.space.outputs() {
            let mut map = layer_map_for_output(output);
            let layer = map
                .layers()
                .find(|layer| return layer.layer_surface() == &surface)
                .cloned();
            if let Some(layer) = layer {
                map.unmap_layer(&layer);
                removed_output = Some(output.clone());
                break;
            }
        }

        if let Some(output) = removed_output {
            debug!(output = %output.name(), "layer surface destroyed");
            reposition_windows(self, &output);
            self.core.queue_redraw(&output);
        }

        if self.core.layer_focus.as_ref() == Some(&root) {
            self.core.layer_focus = None;
        }
        update_keyboard_focus(self);
    }
}

/// React to a commit of a layer surface (or one of its subsurfaces).
///
/// Returns `true` when the surface belongs to a layer, so the caller skips the
/// window bookkeeping.
pub fn on_layer_commit(state: &mut State, surface: &WlSurface) -> bool {
    let root = root_surface(surface);
    let Some((output, layer)) = layer_and_output(state, &root) else {
        return false;
    };

    // Subsurfaces only need a repaint.
    if &root != surface {
        state.core.queue_redraw(&output);
        return true;
    }

    let before = layer_map_for_output(&output).non_exclusive_zone();
    layer_map_for_output(&output).arrange();
    let after = layer_map_for_output(&output).non_exclusive_zone();

    if is_mapped(&root) {
        // Newly mapped: give launchers (on-demand) the keyboard, like niri
        // does — lxqt-runner and friends rely on it.
        let was_unmapped = state.core.unmapped_layer_surfaces.remove(&root);
        if was_unmapped {
            debug!(?layer, "layer surface mapped");
            if effective_keyboard_interactivity(state, &layer) == KeyboardInteractivity::OnDemand {
                state.core.layer_focus = Some(root.clone());
            }
        }
    } else {
        // Initial commit (before the first buffer) or a null commit unmapping
        // the surface: register the configure handshake either way.
        state.core.unmapped_layer_surfaces.insert(root.clone());
        let initial_configure_sent = with_states(&root, |states| {
            return states
                .data_map
                .get::<LayerSurfaceData>()
                .map(|data| {
                    return data
                        .lock()
                        .unwrap_or_else(|err| return err.into_inner())
                        .initial_configure_sent;
                })
                .unwrap_or(false);
        });
        if !initial_configure_sent {
            debug!("sending the initial layer configure");
            layer.layer_surface().send_configure();
        }
    }

    if before != after {
        // An exclusive zone appeared or disappeared: move windows.
        reposition_windows(state, &output);
    }
    update_keyboard_focus(state);
    state.core.queue_redraw(&output);
    return true;
}

/// The output and layer surface for a root surface, if it is a layer.
pub fn layer_and_output(state: &State, root: &WlSurface) -> Option<(Output, LayerSurface)> {
    for output in state.core.space.outputs() {
        let map = layer_map_for_output(output);
        let found = map
            .layers()
            .find(|layer| return layer.wl_surface() == root)
            .cloned();
        if let Some(layer) = found {
            return Some((output.clone(), layer));
        }
    }
    return None;
}

/// The keyboard interactivity a layer surface ends up with, after rules.
pub fn effective_keyboard_interactivity(
    state: &State,
    layer: &LayerSurface,
) -> KeyboardInteractivity {
    let requested = layer.cached_state().keyboard_interactivity;

    let rules = &state.core.config.layer_rules;
    if rules.is_empty() {
        return requested;
    }

    for rule in rules {
        if !rule.matches(layer.namespace()) {
            continue;
        }
        if let Some(interactivity) = rule.keyboard_interactivity {
            return match interactivity {
                poly_config::LayerInteractivity::None => KeyboardInteractivity::None,
                poly_config::LayerInteractivity::OnDemand => KeyboardInteractivity::OnDemand,
                poly_config::LayerInteractivity::Exclusive => KeyboardInteractivity::Exclusive,
            };
        }
    }

    return requested;
}

/// Walk up the subsurface tree to the surface the client created.
fn root_surface(surface: &WlSurface) -> WlSurface {
    let mut root = surface.clone();
    while let Some(parent) = get_parent(&root) {
        root = parent;
    }
    return root;
}

/// React to an exclusive-zone change (a bar appeared, resized or went away).
fn reposition_windows(state: &mut State, output: &Output) {
    crate::core::layout::LayoutManager::on_work_area_changed(state, output);
}

/// The topmost layer surface under a compositor-space point, searching `layers`
/// from front to back. Used to keep click focus in sync with what is drawn.
pub fn layer_at(
    state: &State,
    location: Point<f64, Logical>,
    layers: &[Layer],
) -> Option<LayerSurface> {
    let output = state.core.output_at(location)?;
    let output_geometry = state.core.space.output_geometry(&output)?.to_f64();
    let local = location - output_geometry.loc;
    let map = layer_map_for_output(&output);
    for layer in layers {
        if let Some(layer_surface) = map.layer_under(*layer, local) {
            return Some(layer_surface.clone());
        }
    }
    return None;
}

/// The area inside the output that is not reserved by exclusive zones.
pub fn work_area(
    state: &State,
    output: &Output,
) -> Option<smithay::utils::Rectangle<i32, Logical>> {
    let geometry = state.core.space.output_geometry(output)?;
    let map = layer_map_for_output(output);
    let zone = map.non_exclusive_zone();
    return Some(smithay::utils::Rectangle::new(
        geometry.loc + zone.loc,
        zone.size,
    ));
}

/// Serial used for the next configure of a layer surface (tracing helper).
#[allow(dead_code)]
fn next_serial() -> smithay::utils::Serial {
    return SERIAL_COUNTER.next_serial();
}
