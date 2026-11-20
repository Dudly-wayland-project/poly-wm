// Ported from niri (GPL-3.0-or-later): src/input/mod.rs
// Ported from anvil (GPL-3.0-or-later): anvil/src/input_handler.rs

//! Input event processing.
//!
//! Both the winit and the udev backend funnel their events through
//! [`process_input_event`], which mirrors them into the seat, dispatches the
//! configured key, mouse and wheel bindings, and keeps the compositor-level
//! focus state in sync.
//!
//! Bindings match the *raw* (unmodified) keysym — the same choice niri makes —
//! so `Mod+Shift+Q` fires on the physical key `q`: users write the base letter
//! and the reported Shift is what the binding says.

pub mod click_grab;
pub mod move_grab;
pub mod resize_grab;
pub mod scroll_tracker;

use poly_config::{Keybind, ModKey, Modifiers, Trigger};
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Device as _, DeviceCapability,
    Event as _, InputBackend, InputEvent, InputTime, KeyState, KeyboardKeyEvent as _, MouseButton,
    PointerAxisEvent as _, PointerButtonEvent as _, PointerMotionEvent as _, ProximityState,
    TabletToolButtonEvent as _, TabletToolEvent, TabletToolProximityEvent as _,
    TabletToolTipEvent as _, TabletToolTipState, TouchEvent as _,
};
use smithay::backend::libinput::LibinputInputBackend;
use smithay::input::keyboard::{FilterResult, Keysym, ModifiersState};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::input::pointer::{CursorIcon, CursorImageStatus, PointerHandle, RelativeMotionEvent};
use smithay::input::tablet::{self, TabletDescriptor, TabletSeatTrait};
use smithay::input::touch::{DownEvent, UpEvent};
use smithay::reexports::input as libinput;
use smithay::utils::{Coordinate as _, Logical, Point, SERIAL_COUNTER, Serial};
use smithay::wayland::seat::WaylandFocus as _;
use smithay::wayland::shell::wlr_layer::{KeyboardInteractivity, Layer};
use tracing::{debug, trace, warn};

use self::move_grab::MoveGrab;
use self::resize_grab::ResizeGrab;
use crate::core::actions::do_action;
use crate::core::focus::{focus_layer_surface, set_window_focus, update_keyboard_focus};
use crate::core::handlers::layer_shell::{effective_keyboard_interactivity, layer_at};
use crate::core::layout::LayoutManager;
use crate::state::State;

/// `XF86Switch_VT_1` and `XF86Switch_VT_12` from the X11 keysym range.
const XF86_SWITCH_VT_1: u32 = 0x1008_fe01;
const XF86_SWITCH_VT_12: u32 = 0x1008_fe0c;

/// What the udev backend hands us: a libinput device.
type LibinputDevice = <LibinputInputBackend as InputBackend>::Device;

/// Feed one backend event into the seat and the compositor state.
pub fn process_input_event<B: InputBackend>(state: &mut State, event: InputEvent<B>) {
    // Any input counts as user activity for ext-idle-notify-v1.
    state.notify_activity();

    match event {
        InputEvent::Keyboard { event, .. } => on_keyboard::<B>(state, &event),
        InputEvent::PointerMotion { event } => on_pointer_motion::<B>(state, &event),
        InputEvent::PointerMotionAbsolute { event, .. } => {
            on_pointer_motion_absolute::<B>(state, &event)
        }
        InputEvent::PointerButton { event } => on_pointer_button::<B>(state, &event),
        InputEvent::PointerAxis { event } => on_pointer_axis::<B>(state, &event),
        InputEvent::TabletToolAxis { event } => on_tablet_tool_axis::<B>(state, &event),
        InputEvent::TabletToolTip { event } => on_tablet_tool_tip::<B>(state, &event),
        InputEvent::TabletToolProximity { event } => on_tablet_tool_proximity::<B>(state, &event),
        InputEvent::TabletToolButton { event } => on_tablet_tool_button::<B>(state, &event),
        InputEvent::TouchDown { event } => on_touch_down::<B>(state, &event),
        InputEvent::TouchMotion { event } => on_touch_motion::<B>(state, &event),
        InputEvent::TouchUp { event } => on_touch_up::<B>(state, &event),
        InputEvent::TouchFrame { event: _ } => on_touch_frame(state),
        InputEvent::TouchCancel { event: _ } => on_touch_cancel(state),
        InputEvent::DeviceAdded { device } => {
            debug!("input device added: {:?}", device.name());
            // The seat only advertises touch to clients once such a device
            // exists (anvil does the same).
            if device.has_capability(DeviceCapability::Touch)
                && state.core.seat.get_touch().is_none()
            {
                state.core.seat.add_touch();
            }
            if device.has_capability(DeviceCapability::TabletTool) {
                let descriptor = TabletDescriptor::from(&device);
                state
                    .core
                    .seat
                    .tablet_seat()
                    .add_wp_tablet(&state.core.display_handle, &descriptor);
            }
        }
        InputEvent::DeviceRemoved { device } => {
            debug!("input device removed: {:?}", device.name());
            if device.has_capability(DeviceCapability::TabletTool) {
                let descriptor = TabletDescriptor::from(&device);
                let tablet_seat = state.core.seat.tablet_seat();
                tablet_seat.remove_tablet(&descriptor);
                // Tools belong to tablets; drop them once the last one is gone.
                if tablet_seat.count_tablets() == 0 {
                    tablet_seat.clear_tools();
                }
            }
        }
        _ => (),
    }
}

/// The udev backend routes its libinput events through here: device *settings*
/// (`input.devices`) only exist for libinput devices, so they are applied
/// before the backend-agnostic handling.
pub fn process_libinput_event(state: &mut State, mut event: InputEvent<LibinputInputBackend>) {
    if let InputEvent::DeviceAdded { device } = &mut event {
        apply_device_settings(state, device);
    }
    process_input_event(state, event);
}

/// Apply the first `input.devices` entry whose glob matches the device name.
fn apply_device_settings(state: &State, device: &mut LibinputDevice) {
    // `name()` returns a `Cow` borrowed from the device; keep an owned copy so
    // the settings below can borrow the device mutably.
    let name = device.name().into_owned();
    let Some((pattern, settings)) = state
        .core
        .config
        .input
        .devices
        .iter()
        .find(|(pattern, _)| return poly_config::device_matches(pattern, &name))
    else {
        return;
    };

    debug!("applying the {pattern:?} settings to {name:?}");

    let report = |setting: &str, result: libinput::DeviceConfigResult| {
        if let Err(err) = result {
            warn!("cannot set {setting} on {name:?}: {err:?}");
        }
    };

    if let Some(tap) = settings.tap {
        report("tap", device.config_tap_set_enabled(tap));
    }
    if let Some(drag) = settings.drag {
        report("drag", device.config_tap_set_drag_enabled(drag));
    }
    if let Some(drag_lock) = settings.drag_lock {
        let lock = if drag_lock {
            libinput::DragLockState::EnabledTimeout
        } else {
            libinput::DragLockState::Disabled
        };
        report("drag-lock", device.config_tap_set_drag_lock_enabled(lock));
    }
    if let Some(dwt) = settings.dwt {
        report("disable-while-typing", device.config_dwt_set_enabled(dwt));
    }
    if let Some(natural_scroll) = settings.natural_scroll {
        report(
            "natural-scroll",
            device.config_scroll_set_natural_scroll_enabled(natural_scroll),
        );
    }
    if let Some(left_handed) = settings.left_handed {
        report("left-handed", device.config_left_handed_set(left_handed));
    }
    if let Some(click_method) = settings.click_method {
        let method = match click_method {
            poly_config::ClickMethod::ButtonAreas => libinput::ClickMethod::ButtonAreas,
            poly_config::ClickMethod::Clickfinger => libinput::ClickMethod::Clickfinger,
        };
        report("click-method", device.config_click_set_method(method));
    }
    if let Some(scroll_method) = settings.scroll_method {
        let method = match scroll_method {
            poly_config::ScrollMethod::NoScroll => libinput::ScrollMethod::NoScroll,
            poly_config::ScrollMethod::TwoFinger => libinput::ScrollMethod::TwoFinger,
            poly_config::ScrollMethod::Edge => libinput::ScrollMethod::Edge,
            poly_config::ScrollMethod::OnButtonDown => libinput::ScrollMethod::OnButtonDown,
        };
        report("scroll-method", device.config_scroll_set_method(method));
    }
    if let Some(profile) = settings.accel_profile {
        let profile = match profile {
            poly_config::AccelProfile::Adaptive => libinput::AccelProfile::Adaptive,
            poly_config::AccelProfile::Flat => libinput::AccelProfile::Flat,
        };
        report("accel-profile", device.config_accel_set_profile(profile));
    }
    if let Some(speed) = settings.accel_speed {
        report("accel-speed", device.config_accel_set_speed(speed));
    }
    if let Some(matrix) = &settings.calibration {
        // libinput takes the affine 3x2 part of the 3x3 matrix we document.
        let mut affine = [0.0_f32; 6];
        for (slot, value) in affine.iter_mut().zip(matrix.iter()) {
            *slot = *value;
        }
        report("calibration", device.config_calibration_set_matrix(affine));
    }
    if settings.map_to_output.is_some() {
        warn!("input.devices.*.map_to_output needs more than one output and is ignored for now");
    }
}

fn on_keyboard<B: InputBackend>(state: &mut State, event: &B::KeyboardKeyEvent) {
    let Some(keyboard) = state.core.seat.get_keyboard() else {
        return;
    };
    let serial = SERIAL_COUNTER.next_serial();
    let time = event.time();
    let keycode = event.key_code();
    let key_state = event.state();
    let pressed = key_state == KeyState::Pressed;

    // Stop the binding repeat on any release (niri does the same; it is good
    // enough unless several bound keys are held at once).
    if !pressed {
        state.core.stop_bind_repeat();
    }

    let locked = state.core.is_locked();
    let shortcuts_inhibited = keyboard.current_focus().is_some_and(|surface| {
        return state.shortcuts_inhibited_for(&surface);
    });

    keyboard.input::<(), _>(
        state,
        keycode,
        key_state,
        serial,
        time,
        |state, modifiers, keysym| {
            trace!(
                ?keycode,
                ?key_state,
                ?modifiers,
                raw = keysym
                    .raw_latin_sym_or_raw_current_sym()
                    .map(|sym| return sym.raw()),
                "key event"
            );
            // While locked, the lock client owns the keyboard: no compositor
            // bindings fire, everything is forwarded to the lock surface.
            if locked {
                return FilterResult::Forward;
            }

            // Compositor-level VT switching comes before user bindings.
            if pressed && modifiers.ctrl && modifiers.alt {
                let raw = keysym.modified_sym().raw();
                if (XF86_SWITCH_VT_1..=XF86_SWITCH_VT_12).contains(&raw) {
                    let vt = (raw - XF86_SWITCH_VT_1) as i32 + 1;
                    debug!("switching to VT {vt}");
                    state.backend.change_vt(vt);
                    return FilterResult::Intercept(());
                }
            }

            // `zwp_keyboard_shortcuts_inhibit_v1`: the focused client asked
            // to receive the keys itself (games, remote desktop).
            if shortcuts_inhibited {
                return FilterResult::Forward;
            }

            // Bindings match the raw (unmodified) keysym, so `Mod+Shift+Q` and
            // `Mod+q` are the same physical key.
            let Some(raw) = keysym.raw_latin_sym_or_raw_current_sym() else {
                return FilterResult::Forward;
            };

            if pressed && raw == Keysym::Escape && cancel_pointer_grab(state, serial, time) {
                return FilterResult::Intercept(());
            }

            let bind = find_active_bind(state, Trigger::Keysym(raw), *modifiers).cloned();
            let Some(bind) = bind else {
                return FilterResult::Forward;
            };

            if pressed {
                debug!("key binding fired: {:?}", bind.action);
                do_action(state, bind.action.clone(), None);
                state.core.start_bind_repeat(&bind);
            }
            // A bound key never leaks to the client, not even its release.
            return FilterResult::Intercept(());
        },
    );
}

fn on_pointer_motion<B: InputBackend>(state: &mut State, event: &B::PointerMotionEvent) {
    let Some(pointer) = state.core.seat.get_pointer() else {
        return;
    };
    let delta = event.delta();
    let delta_unaccel = event.delta_unaccel();
    let location = clamp_to_space(state, pointer.current_location() + delta);
    let serial = SERIAL_COUNTER.next_serial();
    release_preferred_output(state, location);

    let under = state.core.surface_under(location);
    pointer.motion(
        state,
        under.clone(),
        &MotionEvent {
            location,
            serial,
            time: event.time(),
        },
    );
    pointer.relative_motion(
        state,
        under,
        &RelativeMotionEvent {
            delta,
            delta_unaccel,
            time: event.time(),
        },
    );
    pointer.frame(state);
    reset_cursor_off_surface(state, &pointer);

    // We draw the cursor ourselves, so a move repaints every output: the old
    // position has to be erased and the new one drawn.
    state.core.queue_redraw_all();
}

/// Show the default arrow once the pointer is over no surface at all.
///
/// A client's cursor (a link's hand, an entry's I-beam, its own cursor
/// surface) only stays meaningful while that client has pointer focus. Smithay
/// does not reset it on leave, so without this the desktop and the gaps would
/// keep whatever shape the last client set. Grabs set their own cursor and
/// keep it.
fn reset_cursor_off_surface(state: &mut State, pointer: &PointerHandle<State>) {
    if pointer.is_grabbed() || pointer.current_focus().is_some() {
        return;
    }
    let default = matches!(
        state.core.cursor_status,
        CursorImageStatus::Named(CursorIcon::Default)
    );
    if !default {
        state.core.cursor_status = CursorImageStatus::default_named();
    }
}

fn on_pointer_motion_absolute<B: InputBackend>(
    state: &mut State,
    event: &B::PointerMotionAbsoluteEvent,
) {
    let Some(pointer) = state.core.seat.get_pointer() else {
        return;
    };
    let Some(position) = absolute_position_transformed(state, event) else {
        return;
    };
    trace!("pointer motion {position:?}");
    let serial = SERIAL_COUNTER.next_serial();
    release_preferred_output(state, position);

    let under = state.core.surface_under(position);
    pointer.motion(
        state,
        under,
        &MotionEvent {
            location: position,
            serial,
            time: event.time(),
        },
    );
    pointer.frame(state);
    reset_cursor_off_surface(state, &pointer);
    state.core.queue_redraw_all();
}

/// Drop `outputs.<name>.focus` once the pointer reaches another output.
///
/// The configuration only decides the focused output while nothing else does;
/// the mouse is the strongest signal there is, so it wins immediately. Without
/// this, the configured output would keep claiming `focused-output` (and the
/// window actions that follow it) no matter where the user pointed.
fn release_preferred_output(state: &mut State, location: Point<f64, Logical>) {
    if state.core.preferred_output.is_none() {
        return;
    }
    let Some(output) = state.core.output_at(location) else {
        return;
    };
    if state.core.preferred_output.as_ref() == Some(&output) {
        return;
    }
    debug!(
        "the pointer moved to {}; the configured focus output steps back",
        output.name()
    );
    state.core.preferred_output = None;
}

fn on_pointer_button<B: InputBackend>(state: &mut State, event: &B::PointerButtonEvent) {
    let Some(pointer) = state.core.seat.get_pointer() else {
        return;
    };
    let serial = SERIAL_COUNTER.next_serial();
    let time = event.time();
    let button_code = event.button_code();
    let button_state = event.state();
    let pressed = button_state == ButtonState::Pressed;
    trace!("pointer button {button_code} {button_state:?}");

    // The release of a button whose press fired a binding: the press was
    // swallowed, so the release is too. A move/resize grab the binding started
    // ends here — the release never reaches the grab itself.
    if !pressed && state.core.suppressed_buttons.remove(&button_code) {
        end_drag_grab(state, serial, time);
        pointer.frame(state);
        return;
    }

    if pressed && !pointer.is_grabbed() {
        focus_under_location(state, pointer.current_location(), serial);
    }

    if pressed {
        let bind = state
            .core
            .seat
            .get_keyboard()
            .map(|keyboard| return keyboard.modifier_state())
            .and_then(|modifiers| {
                let trigger = event.button().and_then(trigger_from_button)?;
                return find_pointer_bind(state, trigger, modifiers);
            });
        if let Some(bind) = bind {
            debug!("mouse binding fired: {:?}", bind.action);
            state.core.suppressed_buttons.insert(button_code);
            do_action(state, bind.action.clone(), Some(button_code));
            pointer.frame(state);
            return;
        }
    }

    pointer.button(
        state,
        &ButtonEvent {
            button: button_code,
            state: button_state,
            serial,
            time,
        },
    );
    pointer.frame(state);
}

fn on_pointer_axis<B: InputBackend>(state: &mut State, event: &B::PointerAxisEvent) {
    let Some(pointer) = state.core.seat.get_pointer() else {
        return;
    };

    let modifiers = state
        .core
        .seat
        .get_keyboard()
        .map(|keyboard| return keyboard.modifier_state());

    // Wheel bindings: with the matching modifiers held, ticks fire bindings
    // instead of scrolling the focused client. The accumulators carry the
    // fractional amounts of high-resolution wheels and touchpads.
    if let Some((modifiers, true)) = modifiers.map(|modifiers| {
        return (modifiers, active_wheel_binds_armed(state, modifiers));
    }) {
        let horizontal = event.amount_v120(Axis::Horizontal).unwrap_or(0.0);
        let vertical = event.amount_v120(Axis::Vertical).unwrap_or(0.0);
        if event.source() == AxisSource::Wheel {
            dispatch_wheel_ticks(state, modifiers, horizontal, vertical);
            pointer.frame(state);
            return;
        }
        // Continuous sources (touchpad fingers) do not have ticks; without a
        // `TouchpadScroll*` binding the event belongs to the client.
    }

    state.core.scroll_tracker_horizontal.reset();
    state.core.scroll_tracker_vertical.reset();

    let horizontal = event
        .amount(Axis::Horizontal)
        .or_else(|| {
            return event
                .amount_v120(Axis::Horizontal)
                .map(|v| return v * 15.0 / 120.0);
        })
        .unwrap_or(0.0);
    let vertical = event
        .amount(Axis::Vertical)
        .or_else(|| {
            return event
                .amount_v120(Axis::Vertical)
                .map(|v| return v * 15.0 / 120.0);
        })
        .unwrap_or(0.0);

    let mut frame = AxisFrame::new(event.time()).source(event.source());
    if horizontal != 0.0 {
        frame =
            frame.relative_direction(Axis::Horizontal, event.relative_direction(Axis::Horizontal));
        frame = frame.value(Axis::Horizontal, horizontal);
        if let Some(discrete) = event.amount_v120(Axis::Horizontal) {
            frame = frame.v120(Axis::Horizontal, discrete as i32);
        }
    }
    if vertical != 0.0 {
        frame = frame.relative_direction(Axis::Vertical, event.relative_direction(Axis::Vertical));
        frame = frame.value(Axis::Vertical, vertical);
        if let Some(discrete) = event.amount_v120(Axis::Vertical) {
            frame = frame.v120(Axis::Vertical, discrete as i32);
        }
    }
    if event.source() == AxisSource::Finger {
        if event
            .amount(Axis::Horizontal)
            .is_some_and(|amount| return amount == 0.0)
        {
            frame = frame.stop(Axis::Horizontal);
        }
        if event
            .amount(Axis::Vertical)
            .is_some_and(|amount| return amount == 0.0)
        {
            frame = frame.stop(Axis::Vertical);
        }
    }

    pointer.axis(state, frame);
    pointer.frame(state);
}

fn on_touch_down<B: InputBackend>(state: &mut State, event: &B::TouchDownEvent) {
    let Some(touch) = state.core.seat.get_touch() else {
        return;
    };
    let Some(location) = absolute_position_transformed(state, event) else {
        return;
    };

    let serial = SERIAL_COUNTER.next_serial();
    focus_under_location(state, location, serial);

    let under = state.core.surface_under(location);
    touch.down(
        state,
        under,
        &DownEvent {
            slot: event.slot(),
            location,
            serial,
            time: event.time(),
        },
    );
}

fn on_touch_motion<B: InputBackend>(state: &mut State, event: &B::TouchMotionEvent) {
    let Some(touch) = state.core.seat.get_touch() else {
        return;
    };
    let Some(location) = absolute_position_transformed(state, event) else {
        return;
    };

    let under = state.core.surface_under(location);
    touch.motion(
        state,
        under,
        &smithay::input::touch::MotionEvent {
            slot: event.slot(),
            location,
            time: event.time(),
        },
    );
}

fn on_touch_up<B: InputBackend>(state: &mut State, event: &B::TouchUpEvent) {
    let Some(touch) = state.core.seat.get_touch() else {
        return;
    };
    let serial = SERIAL_COUNTER.next_serial();
    touch.up(
        state,
        &UpEvent {
            slot: event.slot(),
            serial,
            time: event.time(),
        },
    );
}

fn on_touch_frame(state: &mut State) {
    let Some(touch) = state.core.seat.get_touch() else {
        return;
    };
    touch.frame(state);
}

fn on_touch_cancel(state: &mut State) {
    let Some(touch) = state.core.seat.get_touch() else {
        return;
    };
    touch.cancel(state);
}

fn on_tablet_tool_axis<B: InputBackend>(state: &mut State, event: &B::TabletToolAxisEvent) {
    // Axis events are batched by libinput; forward them as one frame.
    update_tablet_tool::<B>(state, event, true);
}

/// Move the tool and update the client surface under it.
///
/// Tool positions are absolute, so they use the same transformed-position
/// mapping as absolute pointer events (single output for now).
fn update_tablet_tool<B: InputBackend>(
    state: &mut State,
    event: &(impl smithay::backend::input::Event<B> + TabletToolEvent<B>),
    send_frame: bool,
) {
    let Some(location) = absolute_position_transformed(state, event) else {
        return;
    };
    let Some(tool) = state.core.seat.tablet_seat().get_tool(&event.tool()) else {
        debug!("tablet axis event for an unknown tool");
        return;
    };

    let focus = state.core.surface_under(location);
    let time = event.time();
    let serial = SERIAL_COUNTER.next_serial();
    tool.motion(
        state,
        focus,
        &tablet::tool::MotionEvent {
            location,
            serial,
            time,
        },
    );
    if send_frame {
        tool.frame(state, time);
    }
    state.core.queue_redraw_all();
}

fn on_tablet_tool_tip<B: InputBackend>(state: &mut State, event: &B::TabletToolTipEvent) {
    let Some(tool) = state.core.seat.tablet_seat().get_tool(&event.tool()) else {
        debug!("tablet tip event for an unknown tool");
        return;
    };
    let tip_state = event.tip_state();
    let time = event.time();

    // Tip events carry the final axis data of the touch, so push a position
    // update first (niri does the same).
    if tip_state == TabletToolTipState::Down {
        update_tablet_tool::<B>(state, event, false);
    }

    let serial = SERIAL_COUNTER.next_serial();
    match tip_state {
        TabletToolTipState::Down => tool.down(state, &tablet::tool::DownEvent { serial, time }),
        TabletToolTipState::Up => tool.up(state, &tablet::tool::UpEvent { serial, time }),
    }
    tool.frame(state, time);
    state.core.queue_redraw_all();
}

fn on_tablet_tool_proximity<B: InputBackend>(
    state: &mut State,
    event: &B::TabletToolProximityEvent,
) {
    let Some(location) = absolute_position_transformed(state, event) else {
        return;
    };

    let focus = state.core.surface_under(location);
    let tablet_seat = state.core.seat.tablet_seat();
    let display_handle = state.core.display_handle.clone();
    // The tool is announced on its first proximity event, like niri.
    let tool = tablet_seat
        .get_tool(&event.tool())
        .unwrap_or_else(|| tablet_seat.add_wp_tool(state, &display_handle, &event.tool()));

    let Some(tablet) = tablet_seat.get_tablet(&TabletDescriptor::from(&event.device())) else {
        debug!("tablet proximity event for an unknown tablet");
        return;
    };

    let serial = SERIAL_COUNTER.next_serial();
    let time = event.time();
    match event.state() {
        ProximityState::In => {
            let axis = tablet::tool::AxisFrame {
                pressure: event.pressure_has_changed().then_some(event.pressure()),
                distance: event.distance_has_changed().then_some(event.distance()),
                tilt: event.tilt_has_changed().then_some(event.tilt()),
                rotation: event.rotation_has_changed().then_some(event.rotation()),
                slider: event
                    .slider_has_changed()
                    .then_some(event.slider_position()),
                wheel: event
                    .wheel_has_changed()
                    .then_some((event.wheel_delta(), event.wheel_delta_discrete())),
            };
            tool.proximity_in(
                state,
                focus,
                tablet,
                &tablet::tool::ProximityInEvent {
                    location,
                    axis: Some(axis),
                    serial,
                    time,
                },
            );
        }
        ProximityState::Out => {
            tool.proximity_out(state, &tablet::tool::ProximityOutEvent { serial, time });
        }
    }
    tool.frame(state, time);
    state.core.queue_redraw_all();
}

fn on_tablet_tool_button<B: InputBackend>(state: &mut State, event: &B::TabletToolButtonEvent) {
    let Some(tool) = state.core.seat.tablet_seat().get_tool(&event.tool()) else {
        debug!("tablet button event for an unknown tool");
        return;
    };
    let time = event.time();
    tool.button(
        state,
        &tablet::tool::ButtonEvent {
            serial: SERIAL_COUNTER.next_serial(),
            button: event.button(),
            state: event.button_state(),
            time,
        },
    );
    tool.frame(state, time);
    state.core.queue_redraw_all();
}

/// Map a pointer button to the binding trigger it activates.
fn trigger_from_button(button: MouseButton) -> Option<Trigger> {
    return match button {
        MouseButton::Left => Some(Trigger::MouseLeft),
        MouseButton::Right => Some(Trigger::MouseRight),
        MouseButton::Middle => Some(Trigger::MouseMiddle),
        MouseButton::Back => Some(Trigger::MouseBack),
        MouseButton::Forward => Some(Trigger::MouseForward),
        _ => None,
    };
}

/// Whether a wheel binding could fire with exactly `modifiers` held.
///
/// The wheel triggers are looked up through [`find_bind`] so that the `Mod`
/// resolution matches whatever the actual dispatch would do.
fn wheel_binds_armed(keybinds: &[Keybind], mod_key: ModKey, modifiers: ModifiersState) -> bool {
    return WHEEL_TRIGGERS
        .into_iter()
        .any(|trigger| return find_bind(keybinds, mod_key, trigger, modifiers).is_some());
}

/// Whether either the active layout or the global bindings would fire for a
/// wheel tick with these modifiers.
fn active_wheel_binds_armed(state: &State, modifiers: ModifiersState) -> bool {
    return wheel_binds_armed(&state.core.config.keybinds, state.core.mod_key, modifiers)
        || wheel_binds_armed(active_layout_keybinds(state), state.core.mod_key, modifiers);
}

const WHEEL_TRIGGERS: [Trigger; 4] = [
    Trigger::WheelScrollUp,
    Trigger::WheelScrollDown,
    Trigger::WheelScrollLeft,
    Trigger::WheelScrollRight,
];

/// Fire the wheel binding for each completed tick, if one matches.
fn dispatch_wheel_ticks(
    state: &mut State,
    modifiers: ModifiersState,
    horizontal: f64,
    vertical: f64,
) {
    let horizontal_ticks = state.core.scroll_tracker_horizontal.accumulate(horizontal);
    let vertical_ticks = state.core.scroll_tracker_vertical.accumulate(vertical);

    for (ticks, negative, positive) in [
        (
            horizontal_ticks,
            Trigger::WheelScrollLeft,
            Trigger::WheelScrollRight,
        ),
        (
            vertical_ticks,
            Trigger::WheelScrollUp,
            Trigger::WheelScrollDown,
        ),
    ] {
        if ticks == 0 {
            continue;
        }
        let trigger = if ticks > 0 { positive } else { negative };
        let bind = find_active_bind(state, trigger, modifiers).cloned();
        let Some(bind) = bind else {
            continue;
        };
        for _ in 0..ticks.unsigned_abs() {
            do_action(state, bind.action.clone(), None);
        }
    }
}

/// The modifiers of an event, in our vocabulary.
fn modifiers_from_state(state: ModifiersState) -> Modifiers {
    let mut modifiers = Modifiers::empty();
    if state.ctrl {
        modifiers |= Modifiers::CTRL;
    }
    if state.alt {
        modifiers |= Modifiers::ALT;
    }
    if state.shift {
        modifiers |= Modifiers::SHIFT;
    }
    if state.logo {
        modifiers |= Modifiers::SUPER;
    }
    if state.iso_level3_shift {
        modifiers |= Modifiers::ISO_LEVEL3_SHIFT;
    }
    if state.iso_level5_shift {
        modifiers |= Modifiers::ISO_LEVEL5_SHIFT;
    }
    return modifiers;
}

/// A binding's modifiers with `Mod` resolved to the compositor's mod key, and
/// vice versa, so both `Mod+Q` and the mod key held on its own compare equal to
/// the event (ported from niri's `find_configured_bind`).
fn bind_matches(bind: &Keybind, mod_key: ModKey, modifiers: Modifiers) -> bool {
    let mut bind_modifiers = bind.key.modifiers;
    if bind_modifiers.contains(Modifiers::COMPOSITOR) {
        bind_modifiers |= mod_key.to_modifiers();
    } else if bind_modifiers.contains(mod_key.to_modifiers()) {
        bind_modifiers |= Modifiers::COMPOSITOR;
    }
    return bind_modifiers == modifiers;
}

/// The binding that fires for this trigger and these modifiers, if any.
pub fn find_bind(
    keybinds: &[Keybind],
    mod_key: ModKey,
    trigger: Trigger,
    modifiers: ModifiersState,
) -> Option<&Keybind> {
    let mut modifiers = modifiers_from_state(modifiers);
    if modifiers.contains(mod_key.to_modifiers()) {
        modifiers |= Modifiers::COMPOSITOR;
    }
    return keybinds
        .iter()
        .find(|bind| return bind.key.trigger == trigger && bind_matches(bind, mod_key, modifiers));
}

/// Key bindings scoped to the active workspace's layout.
fn active_layout_keybinds(state: &State) -> &[Keybind] {
    let Some(output) = LayoutManager::active_output(state) else {
        return &[];
    };
    let Some(name) = state.core.layout.layout_name(&output) else {
        return &[];
    };
    return state
        .core
        .config
        .layouts
        .iter()
        .find(|layout| return layout.name == name)
        .map(|layout| return layout.keybinds.as_slice())
        .unwrap_or(&[]);
}

/// Prefer a layout binding over a global one for the same trigger.
fn find_preferred_bind<'a>(
    layout: &'a [Keybind],
    global: &'a [Keybind],
    mod_key: ModKey,
    trigger: Trigger,
    modifiers: ModifiersState,
) -> Option<&'a Keybind> {
    return find_bind(layout, mod_key, trigger, modifiers)
        .or_else(|| return find_bind(global, mod_key, trigger, modifiers));
}

/// The binding for this trigger, preferring the active layout's bindings over
/// the global ones.
pub fn find_active_bind(
    state: &State,
    trigger: Trigger,
    modifiers: ModifiersState,
) -> Option<&Keybind> {
    return find_preferred_bind(
        active_layout_keybinds(state),
        &state.core.config.keybinds,
        state.core.mod_key,
        trigger,
        modifiers,
    );
}

/// Pointer bindings also match while the `window_drag.axis_lock` modifier is
/// held: it is not part of the binding, but the drag start must still see the
/// button press. Exact matches win, so a user can still
/// define a distinct `Mod+Shift+MouseLeft` binding.
fn find_pointer_bind(
    state: &State,
    trigger: Trigger,
    modifiers: ModifiersState,
) -> Option<Keybind> {
    if let Some(bind) = find_active_bind(state, trigger, modifiers) {
        return Some(bind.clone());
    }
    let lock = state.core.config.window_drag.axis_lock?;
    // Locking on the compositor modifier itself would make `Mod+MouseLeft`
    // unmatchable (the compositor modifier marks the binding); the config
    // validator rejects that combination.
    if lock == state.core.mod_key || !modifier_held(modifiers, lock) {
        return None;
    }
    let modifiers = without_modifier(modifiers, lock);
    return find_active_bind(state, trigger, modifiers).cloned();
}

fn modifier_held(modifiers: ModifiersState, key: ModKey) -> bool {
    return match key {
        ModKey::Ctrl => modifiers.ctrl,
        ModKey::Shift => modifiers.shift,
        ModKey::Alt => modifiers.alt,
        ModKey::Super => modifiers.logo,
        ModKey::IsoLevel3Shift => modifiers.iso_level3_shift,
        ModKey::IsoLevel5Shift => modifiers.iso_level5_shift,
    };
}

fn without_modifier(mut modifiers: ModifiersState, key: ModKey) -> ModifiersState {
    match key {
        ModKey::Ctrl => modifiers.ctrl = false,
        ModKey::Shift => modifiers.shift = false,
        ModKey::Alt => modifiers.alt = false,
        ModKey::Super => modifiers.logo = false,
        ModKey::IsoLevel3Shift => modifiers.iso_level3_shift = false,
        ModKey::IsoLevel5Shift => modifiers.iso_level5_shift = false,
    }
    return modifiers;
}

/// Focus and raise whatever is under `location`: a window, an on-demand layer
/// surface, or nothing (a click on empty space, or a tap).
fn focus_under_location(state: &mut State, location: Point<f64, Logical>, serial: Serial) {
    // Running lock surfaces already own the focus; clicks only reach them.
    if state.core.is_locked() {
        return;
    }

    // The search order mirrors `Core::surface_under` and the rendering order:
    // Overlay/Top layers, then windows, then Bottom/Background layers.
    let covers_top = state.core.fullscreen_covers_top(location);
    let upper_layers: &[Layer] = if covers_top {
        &[Layer::Overlay]
    } else {
        &[Layer::Overlay, Layer::Top]
    };
    if let Some(layer_surface) = layer_at(state, location, upper_layers) {
        focus_layer_click(state, &layer_surface);
        return;
    }

    let window = state
        .core
        .space
        .element_under(location)
        .map(|(window, _)| return window.clone());
    if let Some(window) = window {
        let surface = window
            .wl_surface()
            .map(|surface| return surface.into_owned());
        // Do not raise a tiled window: the layout owns the stacking order.
        // Raising it here would make pointer hit testing disagree with what
        // the renderer draws.
        crate::core::window::focus_window(state, &window, surface, serial);
        if let Some(managed) = state.core.layout.by_window(&window) {
            trace!("click focused window {} at {location:?}", managed.id);
        }
        state.core.queue_redraw_all();
        return;
    }

    if covers_top {
        if let Some(layer_surface) = layer_at(state, location, &[Layer::Top]) {
            focus_layer_click(state, &layer_surface);
            return;
        }
    }

    if let Some(layer_surface) = layer_at(state, location, &[Layer::Bottom, Layer::Background]) {
        focus_layer_click(state, &layer_surface);
        return;
    }

    // A click on empty space drops both the window and layer focus.
    set_window_focus(state, None);
    update_keyboard_focus(state);
}

/// Apply the click rules of a layer surface: on-demand layers take the
/// keyboard, exclusive ones already have it, non-interactive ones leave it.
fn focus_layer_click(state: &mut State, layer_surface: &smithay::desktop::LayerSurface) {
    match effective_keyboard_interactivity(state, layer_surface) {
        KeyboardInteractivity::OnDemand => {
            focus_layer_surface(state, layer_surface.wl_surface());
        }
        KeyboardInteractivity::Exclusive => {
            state.core.layer_focus = None;
            update_keyboard_focus(state);
        }
        // Bars and notifications must not steal the keyboard from windows.
        KeyboardInteractivity::None => {}
    }
}

/// Whether the pointer currently has one of our drag grabs.
fn drag_grab_active(state: &State) -> bool {
    let Some(pointer) = state.core.seat.get_pointer() else {
        return false;
    };
    return pointer
        .with_grab(|_, grab| {
            return grab.as_any().is::<MoveGrab>() || grab.as_any().is::<ResizeGrab>();
        })
        .unwrap_or(false);
}

/// End a drag grab from the input layer (a swallowed button release).
fn end_drag_grab(state: &mut State, serial: Serial, time: InputTime) {
    if !drag_grab_active(state) {
        return;
    }
    let Some(pointer) = state.core.seat.get_pointer() else {
        return;
    };
    pointer.unset_grab(state, serial, time);
}

/// Escape cancels an interactive move/resize (`window_drag.escape_cancels`):
/// the grab's `unset` puts the window back. Returns whether a drag was
/// cancelled, so the key can be swallowed.
fn cancel_pointer_grab(state: &mut State, serial: Serial, time: InputTime) -> bool {
    if !state.core.config.window_drag.escape_cancels {
        return false;
    }
    if !drag_grab_active(state) {
        return false;
    }
    let Some(pointer) = state.core.seat.get_pointer() else {
        return false;
    };
    debug!("cancelling the interactive move/resize");
    state.core.grab_cancelled = true;
    pointer.unset_grab(state, serial, time);
    return true;
}

/// Keep the pointer inside the layout of connected outputs.
fn clamp_to_space(state: &State, location: Point<f64, Logical>) -> Point<f64, Logical> {
    let Some(bounding) = state
        .core
        .space
        .outputs()
        .filter_map(|output| return state.core.space.output_geometry(output))
        .reduce(|acc, geo| return acc.merge(geo))
    else {
        return location;
    };

    return Point::from((
        location.x.clamp(
            bounding.loc.x.to_f64(),
            (bounding.loc.x + bounding.size.w) as f64 - 1.0,
        ),
        location.y.clamp(
            bounding.loc.y.to_f64(),
            (bounding.loc.y + bounding.size.h) as f64 - 1.0,
        ),
    ));
}

/// Map an absolute device position through the first output's geometry.
///
/// Absolute devices are mapped through the first output's geometry;
/// multi-output absolute positioning is not implemented yet.
fn absolute_position_transformed<B: InputBackend>(
    state: &State,
    event: &impl AbsolutePositionEvent<B>,
) -> Option<Point<f64, Logical>> {
    let output = state.core.space.outputs().next().cloned()?;
    let output_geometry = state.core.space.output_geometry(&output)?;

    let position = event.position_transformed(output_geometry.size) + output_geometry.loc.to_f64();
    return Some(clamp_to_space(state, position));
}

#[cfg(test)]
mod tests {
    use super::*;
    use poly_ipc::Action;

    fn keybind(spec: &str) -> Keybind {
        return Keybind {
            key: spec.parse().unwrap_or_else(|err| panic!("{spec}: {err}")),
            action: Action::CloseWindow,
            repeat: true,
        };
    }

    fn mods(ctrl: bool, shift: bool, logo: bool) -> ModifiersState {
        return ModifiersState {
            ctrl,
            shift,
            logo,
            ..Default::default()
        };
    }

    #[test]
    fn compositor_mod_matches_the_mod_key() {
        let keybinds = [keybind("Mod+Q")];

        // Mod resolved to Super: pressing Super+Q matches.
        let bind = find_bind(
            &keybinds,
            ModKey::Super,
            Trigger::Keysym(Keysym::q),
            mods(false, false, true),
        );
        assert!(bind.is_some());

        // ... and so does the same binding with Super spelled out, when the mod
        // key is Alt.
        let keybinds = [keybind("Super+Q")];
        let bind = find_bind(
            &keybinds,
            ModKey::Alt,
            Trigger::Keysym(Keysym::q),
            mods(false, false, true),
        );
        assert!(bind.is_some());
        // But not with Alt held instead.
        let bind = find_bind(
            &keybinds,
            ModKey::Alt,
            Trigger::Keysym(Keysym::q),
            mods(false, false, false),
        );
        assert!(bind.is_none());
    }

    #[test]
    fn modifiers_have_to_match_exactly() {
        let keybinds = [keybind("Mod+Shift+Q")];
        assert!(
            find_bind(
                &keybinds,
                ModKey::Super,
                Trigger::Keysym(Keysym::q),
                mods(false, true, true),
            )
            .is_some()
        );
        assert!(
            find_bind(
                &keybinds,
                ModKey::Super,
                Trigger::Keysym(Keysym::q),
                mods(false, false, true),
            )
            .is_none()
        );
    }

    #[test]
    fn wheel_binds_are_armed_for_their_modifiers() {
        let alt = ModifiersState {
            alt: true,
            ..Default::default()
        };
        let super_ = ModifiersState {
            logo: true,
            ..Default::default()
        };

        // The mod key spelled out (here Alt is the configured mod key).
        let keybinds = [keybind("Alt+WheelScrollDown")];
        assert!(wheel_binds_armed(&keybinds, ModKey::Alt, alt));
        assert!(!wheel_binds_armed(
            &keybinds,
            ModKey::Alt,
            ModifiersState::default()
        ));
        assert!(!wheel_binds_armed(&keybinds, ModKey::Alt, super_));

        // ... and written as `Mod`, for whatever the mod key is.
        let keybinds = [keybind("Mod+WheelScrollUp")];
        assert!(wheel_binds_armed(&keybinds, ModKey::Alt, alt));
        assert!(wheel_binds_armed(&keybinds, ModKey::Super, super_));
        assert!(!wheel_binds_armed(&keybinds, ModKey::Super, alt));

        // Keyboard bindings do not arm the wheel handling.
        let keybinds = [keybind("Alt+Q")];
        assert!(!wheel_binds_armed(&keybinds, ModKey::Alt, alt));
    }

    #[test]
    fn layout_bindings_win_over_global_bindings() {
        let mut layout = keybind("Mod+Q");
        layout.action = Action::LayoutAction {
            name: "focus-left".into(),
            args: Vec::new(),
        };
        let action = layout.action.clone();
        let layout_bindings = [layout];
        let global_bindings = [keybind("Mod+Q")];
        let modifiers = mods(false, false, true);
        let trigger = Trigger::Keysym(Keysym::q);

        let bind = find_preferred_bind(
            &layout_bindings,
            &global_bindings,
            ModKey::Super,
            trigger,
            modifiers,
        )
        .expect("the layout binding should match");
        assert_eq!(bind.action, action);

        // A trigger neither side covers does nothing.
        let bind = find_preferred_bind(
            &layout_bindings,
            &global_bindings,
            ModKey::Super,
            Trigger::Keysym(Keysym::w),
            modifiers,
        );
        assert!(bind.is_none());

        // With no layout binding for the trigger, the global one fires.
        let bind = find_preferred_bind(&[], &global_bindings, ModKey::Super, trigger, modifiers)
            .expect("the global binding should match when the layout has none");
        assert_eq!(bind.action, Action::CloseWindow);
    }

    #[test]
    fn caps_lock_does_not_disturb_bindings() {
        // CapsLock/NumLock are not part of the binding vocabulary.
        let keybinds = [keybind("Mod+Q")];
        let mut modifiers = mods(false, false, true);
        modifiers.caps_lock = true;
        modifiers.num_lock = true;
        assert!(
            find_bind(
                &keybinds,
                ModKey::Super,
                Trigger::Keysym(Keysym::q),
                modifiers
            )
            .is_some()
        );
    }
}
