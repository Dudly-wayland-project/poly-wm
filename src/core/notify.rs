//! Desktop notifications for configuration problems (`docs/plan.md` §14).
//!
//! Errors are always drawn as Toasts on screen. `errors = { dbus = true }` also
//! forwards them to the session's notification daemon
//! (`org.freedesktop.Notifications`), so a user who is not looking at the
//! screen still learns that a reload was rejected — the common case when an
//! editor saves a half-written file.
//!
//! It is opt-in, off by default, and needs a build with the `dbus` cargo
//! feature: the compositor must not depend on a session bus to start.

#[cfg(feature = "dbus")]
use std::sync::Mutex;
#[cfg(feature = "dbus")]
use std::time::{Duration, Instant};

#[cfg(feature = "dbus")]
use tracing::debug;
use tracing::warn;

use crate::state::State;

/// How long the same report is not repeated. The watcher retries on every save,
/// so a user editing a broken file would otherwise get one notification per
/// keystroke-triggered save.
#[cfg(feature = "dbus")]
const DEDUPE: Duration = Duration::from_secs(30);
/// Longest body a daemon is asked to display. Notification daemons truncate
/// anyway, and a Lua traceback can be long.
#[cfg(feature = "dbus")]
const MAX_BODY: usize = 4000;

/// Forward configuration errors to the desktop, when the user asked for it.
///
/// `requested` is the union of the running configuration's `errors.dbus` and
/// the value the file just loaded carries (the latter also exists when the
/// file has errors, which is the interesting case). Called next to every place
/// that pushes Toasts, so the two never disagree.
pub fn configuration_errors(state: &State, requested: bool, messages: &[String]) {
    if messages.is_empty() || !(requested || state.core.config.errors.dbus) {
        return;
    }
    send(messages);
}

/// One notification per report: the Toasts of a failing reload are lines of a
/// single problem, and a notification per line would be a storm.
#[cfg(feature = "dbus")]
fn send(messages: &[String]) {
    use std::sync::atomic::Ordering;

    /// The last body sent, so an unchanged report is not repeated.
    static LAST: Mutex<Option<(String, Instant)>> = Mutex::new(None);
    /// The notification to replace, so a new report updates the old one
    /// instead of stacking in the notification list.
    static REPLACES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    let mut body = messages.join("\n");
    if body.len() > MAX_BODY {
        let mut end = MAX_BODY;
        while end > 0 && !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
        body.push_str("\n…");
    }

    {
        let Ok(mut last) = LAST.lock() else {
            return;
        };
        if let Some((text, at)) = last.as_ref()
            && text == &body
            && at.elapsed() < DEDUPE
        {
            return;
        }
        *last = Some((body.clone(), Instant::now()));
    }

    // Asking the daemon blocks until it answers, which a busy or missing
    // daemon can stretch for seconds: never on the event loop thread.
    let replaces = REPLACES.load(Ordering::Relaxed);
    let spawned = std::thread::Builder::new()
        .name("poly-notify".to_owned())
        .spawn(move || {
            if let Some(id) = notify(&body, replaces) {
                REPLACES.store(id, Ordering::Relaxed);
            }
        });
    if let Err(err) = spawned {
        debug!("could not spawn the notification thread: {err}");
    }
}

/// Show `body`, replacing notification `replaces` (0 means "a new one").
///
/// Returns the id the daemon assigned, which the next report replaces.
#[cfg(feature = "dbus")]
fn notify(body: &str, replaces: u32) -> Option<u32> {
    use std::collections::HashMap;

    use zbus::zvariant::Value;

    let connection = match zbus::blocking::Connection::session() {
        Ok(connection) => connection,
        Err(err) => {
            warn!("errors.dbus is enabled, but there is no session bus: {err}");
            return None;
        }
    };

    // `Notify(app_name, replaces_id, app_icon, summary, body, actions, hints,
    // expire_timeout)`, with the daemon's default timeout.
    let hints: HashMap<&str, Value<'static>> = HashMap::new();
    let reply = connection.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &(
            "poly",
            replaces,
            "dialog-error",
            "poly: the configuration has errors",
            body,
            Vec::<&str>::new(),
            hints,
            -1i32,
        ),
    );

    let reply = match reply {
        Ok(reply) => reply,
        Err(err) => {
            warn!("could not show a desktop notification: {err}");
            return None;
        }
    };
    match reply.body().deserialize::<u32>() {
        Ok(id) => return Some(id),
        Err(err) => {
            debug!("the notification daemon answered with no id: {err}");
            return None;
        }
    }
}

/// Without the `dbus` feature the setting cannot be honoured; say so once
/// instead of failing silently.
#[cfg(not(feature = "dbus"))]
fn send(_messages: &[String]) {
    use std::sync::atomic::{AtomicBool, Ordering};

    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    warn!(
        "errors.dbus is enabled, but this build has no D-Bus support; configuration errors stay \
         on screen only (rebuild with --features dbus)"
    );
}
