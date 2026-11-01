// Ported from niri (GPL-3.0-or-later): niri-config/src/binds.rs
// Only the trigger half: the v1 action vocabulary lives in `poly-ipc`
// (`poly-ipc::Action`) so Lua, IPC and `poly msg` share one set of names.

//! Key bindings: which key (or mouse button, or scroll wheel tick) with which
//! modifiers triggers an action.
//!
//! The spelling follows niri's: `"Mod+Shift+Q"`,
//! `"XF86AudioRaiseVolume"`, `"Mod+MouseLeft"`, `"Mod+WheelScrollDown"`.
//! `Mod` is resolved to the backend's modifier key at event time, not here.

use std::str::FromStr;

use bitflags::bitflags;
use poly_ipc::Action;
use xkbcommon::xkb::keysyms::KEY_NoSymbol;
use xkbcommon::xkb::{KEYSYM_CASE_INSENSITIVE, KEYSYM_NO_FLAGS, Keysym, keysym_from_name};

bitflags! {
    /// The modifiers a binding can require.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct Modifiers: u8 {
        /// Ctrl.
        const CTRL = 1;
        /// Shift.
        const SHIFT = 1 << 1;
        /// Alt.
        const ALT = 1 << 2;
        /// Super / Windows key.
        const SUPER = 1 << 3;
        /// ISO level 3 shift (AltGr).
        const ISO_LEVEL3_SHIFT = 1 << 4;
        /// ISO level 5 shift.
        const ISO_LEVEL5_SHIFT = 1 << 5;
        /// `Mod` in a binding: the compositor's modifier key (`input.mod_key`,
        /// default Super on udev, Alt when nested in winit).
        const COMPOSITOR = 1 << 6;
    }
}

/// What has to be pressed (or scrolled) for a binding to fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Trigger {
    /// A keyboard key, by keysym.
    Keysym(Keysym),
    /// Pointer buttons.
    MouseLeft,
    /// Pointer buttons.
    MouseRight,
    /// Pointer buttons.
    MouseMiddle,
    /// Pointer buttons.
    MouseBack,
    /// Pointer buttons.
    MouseForward,
    /// Mouse wheel ticks.
    WheelScrollDown,
    /// Mouse wheel ticks.
    WheelScrollUp,
    /// Mouse wheel ticks.
    WheelScrollLeft,
    /// Mouse wheel ticks.
    WheelScrollRight,
    /// Continuous touchpad scroll (fingers) ticks.
    TouchpadScrollDown,
    /// Continuous touchpad scroll (fingers) ticks.
    TouchpadScrollUp,
    /// Continuous touchpad scroll (fingers) ticks.
    TouchpadScrollLeft,
    /// Continuous touchpad scroll (fingers) ticks.
    TouchpadScrollRight,
}

/// A trigger plus the modifiers that must be held with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    /// The key/button/scroll trigger.
    pub trigger: Trigger,
    /// Modifiers that must be held.
    pub modifiers: Modifiers,
}

/// One configured binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keybind {
    /// The trigger.
    pub key: Key,
    /// What to do when it fires.
    pub action: Action,
    /// Whether holding the key repeats the action.
    pub repeat: bool,
}

impl FromStr for Key {
    type Err = KeyParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut modifiers = Modifiers::empty();

        let mut split = s.split('+');
        // The last '+' segment is the key itself; the earlier ones are
        // modifiers. `split_next_back` is guaranteed to yield at least one
        // element (`""` for an empty string, which then fails as a key name).
        let key = split.next_back().unwrap_or("");
        let key = key.trim();

        for part in split {
            let part = part.trim();
            let modifier = match part.to_ascii_lowercase().as_str() {
                "mod" => Modifiers::COMPOSITOR,
                "ctrl" | "control" => Modifiers::CTRL,
                "shift" => Modifiers::SHIFT,
                "alt" => Modifiers::ALT,
                "super" | "win" => Modifiers::SUPER,
                "iso_level3_shift" | "mod5" => Modifiers::ISO_LEVEL3_SHIFT,
                "iso_level5_shift" | "mod3" => Modifiers::ISO_LEVEL5_SHIFT,
                _ => {
                    return Err(KeyParseError::Modifier(part.to_owned()));
                }
            };
            modifiers |= modifier;
        }

        let trigger = parse_trigger(key)?;
        return Ok(Key { trigger, modifiers });
    }
}

/// Turn a single key name into a trigger.
fn parse_trigger(name: &str) -> Result<Trigger, KeyParseError> {
    let lowered = name.to_ascii_lowercase();
    let trigger = match lowered.as_str() {
        "mouseleft" => Trigger::MouseLeft,
        "mouseright" => Trigger::MouseRight,
        "mousemiddle" => Trigger::MouseMiddle,
        "mouseback" => Trigger::MouseBack,
        "mouseforward" => Trigger::MouseForward,
        "wheelscrolldown" => Trigger::WheelScrollDown,
        "wheelscrollup" => Trigger::WheelScrollUp,
        "wheelscrollleft" => Trigger::WheelScrollLeft,
        "wheelscrollright" => Trigger::WheelScrollRight,
        "touchpadscrolldown" => Trigger::TouchpadScrollDown,
        "touchpadscrollup" => Trigger::TouchpadScrollUp,
        "touchpadscrollleft" => Trigger::TouchpadScrollLeft,
        "touchpadscrollright" => Trigger::TouchpadScrollRight,
        _ => {
            let mut keysym = keysym_from_name(name, KEYSYM_CASE_INSENSITIVE);
            // `keysym_from_name` is case-insensitive, but the XF86 keysyms are
            // spelled inconsistently ("XF86Screensaver" vs "XF86_ScreenSaver"):
            // fall back to the other spelling before giving up.
            if keysym == Keysym::XF86_Screensaver {
                keysym = keysym_from_name(name, KEYSYM_NO_FLAGS);
                if keysym.raw() == KEY_NoSymbol {
                    keysym = Keysym::XF86_ScreenSaver;
                }
            }
            if keysym.raw() == KEY_NoSymbol {
                return Err(KeyParseError::Key(name.to_owned()));
            }
            return Ok(Trigger::Keysym(keysym));
        }
    };
    return Ok(trigger);
}

/// A key name or modifier that could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyParseError {
    /// An unknown modifier name.
    #[error("invalid modifier: {0}")]
    Modifier(String),
    /// An unknown key name.
    #[error("invalid key: {0}")]
    Key(String),
}

impl Key {
    /// Whether this trigger is a keyboard key.
    pub fn is_keyboard(&self) -> bool {
        return matches!(self.trigger, Trigger::Keysym(_));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> Key {
        return s.parse().unwrap_or_else(|err| panic!("{s}: {err}"));
    }

    #[test]
    fn parses_modifiers() {
        let k = key("Mod+Shift+Q");
        assert_eq!(
            k.modifiers,
            Modifiers::COMPOSITOR | Modifiers::SHIFT,
            "Mod and Shift must be set"
        );
        assert_eq!(k.trigger, Trigger::Keysym(Keysym::q));

        assert_eq!(
            key("ctrl+ALT+super+x").modifiers,
            Modifiers::CTRL | Modifiers::ALT | Modifiers::SUPER
        );
        assert_eq!(key("win+x").modifiers, Modifiers::SUPER);
        assert_eq!(key("mod5+x").modifiers, Modifiers::ISO_LEVEL3_SHIFT);
        assert_eq!(key("mod3+x").modifiers, Modifiers::ISO_LEVEL5_SHIFT);
    }

    #[test]
    fn parses_keysyms_case_insensitively() {
        assert_eq!(
            key("XF86AudioRaiseVolume").trigger,
            Trigger::Keysym(Keysym::XF86_AudioRaiseVolume)
        );
        assert_eq!(key("F11").trigger, Trigger::Keysym(Keysym::F11));
        // Case-insensitive lookup prefers the lowercase keysym, which is what
        // we match against: events report the raw (unmodified) keysym, so that
        // "Mod+Shift+Q" fires on the same physical key as "Mod+q".
        assert_eq!(key("A").trigger, Trigger::Keysym(Keysym::a));
        assert_eq!(key("q").trigger, Trigger::Keysym(Keysym::q));
    }

    #[test]
    fn parses_mouse_and_scroll() {
        assert_eq!(key("Mod+MouseLeft").trigger, Trigger::MouseLeft);
        assert_eq!(key("Mod+MouseRight").trigger, Trigger::MouseRight);
        assert_eq!(key("Mod+WheelScrollDown").trigger, Trigger::WheelScrollDown);
        assert_eq!(
            key("Mod+TouchpadScrollUp").trigger,
            Trigger::TouchpadScrollUp
        );
    }

    #[test]
    fn rejects_bad_bindings() {
        assert_eq!(
            "Frob+Q".parse::<Key>().unwrap_err(),
            KeyParseError::Modifier("Frob".into())
        );
        assert_eq!(
            "Mod+NotAKey".parse::<Key>().unwrap_err(),
            KeyParseError::Key("NotAKey".into())
        );
        assert!(matches!(
            "".parse::<Key>().unwrap_err(),
            KeyParseError::Key(_)
        ));
    }
}
