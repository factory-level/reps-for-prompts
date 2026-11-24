// Two windows, one controller. `main` is the programming monitor: it goes
// fullscreen + always-on-top while a workout is owed (soft lock — no OS lock,
// see spec §14). `gym` is the big display: a draggable maximized window placed
// on the remembered (else non-primary) monitor; F11 in it toggles fullscreen.
// Where it was left — monitor + fullscreen — is saved in settings, so it comes
// back in the same place next launch.
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use tauri::{AppHandle, Manager, Position, WindowEvent};

use crate::SharedCore;

static WINDOW_ACTIONS: Mutex<()> = Mutex::new(());

static LOCKED: AtomicBool = AtomicBool::new(false);
/// Other apps' windows we iconified for the lock; mapped back on unlock.
static HIDDEN: Mutex<Vec<u32>> = Mutex::new(Vec::new());
const X11_WINDOWS_PY: &str = include_str!("x11_windows.py");

/// Minimize (or restore) every other app's window via X11 — Tauri can't reach
/// them. Silent no-op without python3-xlib / outside X11.
// ponytail: shells out to python-xlib once a second while locked (~30ms);
// swap for x11rb if that ever shows up in a profile.
fn other_windows(mode: &str, ids: &[u32]) -> Vec<u32> {
    x11(mode, &ids.iter().map(u32::to_string).collect::<Vec<_>>())
}

fn x11(mode: &str, args: &[String]) -> Vec<u32> {
    let mut cmd = Command::new("python3");
    cmd.arg("-c").arg(X11_WINDOWS_PY).arg(mode).arg(std::process::id().to_string()).args(args);
    match cmd.output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout).split_whitespace().filter_map(|s| s.parse().ok()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Assume control: everything else out of the way, on every workspace.
fn take_over() {
    let got = other_windows("minimize", &[]);
    HIDDEN.lock().unwrap().extend(got);
}

/// Debt paid: bring back what we hid.
fn give_back() {
    let ids: Vec<u32> = std::mem::take(&mut *HIDDEN.lock().unwrap());
    if !ids.is_empty() {
        other_windows("restore", &ids);
    }
}
const GYM_MONITOR: &str = "gym_monitor";
const GYM_FULLSCREEN: &str = "gym_fullscreen";

/// Index of the monitor the gym window belongs on: the remembered name (or
/// `$REPS_GYM_MONITOR`) wins, else the first monitor that isn't the primary.
pub(crate) fn pick_gym_monitor(names: &[&str], primary: &str, want: Option<&str>) -> Option<usize> {
    want.and_then(|w| names.iter().position(|n| *n == w))
        .or_else(|| names.iter().position(|n| *n != primary))
}

fn setting(app: &AppHandle, key: &str) -> String {
    app.state::<SharedCore>().lock().unwrap().store.setting(key, "")
}

fn remember(app: &AppHandle, key: &str, value: &str) {
    let _ = app.state::<SharedCore>().lock().unwrap().store.set_setting(key, value);
}

/// Startup placement: main on the primary monitor, gym on its monitor.
pub(crate) fn place(app: &AppHandle) {
    if !app.state::<crate::Runtime>().enforces_windows() {
        if !std::env::args().any(|a| a == "--background") { if let Some(main) = app.get_webview_window("main") { let _ = main.show(); } }
        return;
    }
    let (Some(main), Some(gym)) = (app.get_webview_window("main"), app.get_webview_window("gym")) else {
        return;
    };
    let _ = main.show();
    let _ = gym.show();
    let monitors = main.available_monitors().unwrap_or_default();
    let primary = main.primary_monitor().ok().flatten();
    let names: Vec<&str> = monitors.iter().map(|m| m.name().map(String::as_str).unwrap_or("")).collect();
    let primary_name = primary.as_ref().and_then(|p| p.name()).map(String::as_str).unwrap_or("");
    let want = std::env::var("REPS_GYM_MONITOR").ok().or_else(|| Some(setting(app, GYM_MONITOR))).filter(|s| !s.is_empty());
    // The gym display is in every view: every workspace, every mode.
    let _ = gym.set_visible_on_all_workspaces(true);
    let gym_at = pick_gym_monitor(&names, primary_name, want.as_deref());
    match gym_at {
        Some(i) => {
            let _ = gym.set_position(Position::Physical(*monitors[i].position()));
            if gym_wants_fullscreen(app) {
                let _ = gym.set_fullscreen(true);
            } else {
                let _ = gym.maximize();
            }
        }
        None => eprintln!("windows: no gym monitor (have {names:?}); gym stays a plain window"),
    }
    // The lock lives on whichever monitor the gym isn't — X's idea of
    // "primary" may well be the gym TV.
    let main_at = (0..monitors.len()).find(|i| Some(*i) != gym_at).map(|i| *monitors[i].position());
    if let Some(pos) = main_at.or_else(|| primary.as_ref().map(|p| *p.position())) {
        let _ = main.set_position(Position::Physical(pos));
    }
    // Dragged to another monitor: remember it and snap back to fullscreen there.
    let handle = app.clone();
    let watched = gym.clone();
    gym.on_window_event(move |e| {
        if !handle.state::<crate::Runtime>().enforces_windows() { return; }
        if let WindowEvent::Moved(_) = e {
            if let Ok(Some(m)) = watched.current_monitor() {
                if let Some(name) = m.name() {
                    if setting(&handle, GYM_MONITOR) != *name {
                        remember(&handle, GYM_MONITOR, name);
                        remember(&handle, GYM_FULLSCREEN, "1");
                        let _ = watched.set_fullscreen(true);
                    }
                }
            }
        }
    });
}

/// Fullscreen unless the user F11'd out of it (to drag it somewhere).
fn gym_wants_fullscreen(app: &AppHandle) -> bool {
    setting(app, GYM_FULLSCREEN) != "0"
}

/// Once a second: whatever un-fullscreened the gym display (a workspace
/// switch, a stray key), put it back — unless the user F11'd out on purpose.
pub(crate) fn assert_gym(app: &AppHandle) {
    let _guard = WINDOW_ACTIONS.lock().unwrap();
    if !app.state::<crate::Runtime>().enforces_windows() { return; }
    let Some(gym) = app.get_webview_window("gym") else { return };
    if gym_wants_fullscreen(app) && !gym.is_fullscreen().unwrap_or(true) {
        let _ = gym.set_fullscreen(true);
    }
}

/// Lock/unlock the programming monitor. Idempotent; safe from any thread.
/// The gym window is never touched here.
pub(crate) fn apply_lock(app: &AppHandle, locked: bool) {
    let _guard = WINDOW_ACTIONS.lock().unwrap();
    if !app.state::<crate::Runtime>().enforces_windows() { return; }
    if LOCKED.swap(locked, Ordering::SeqCst) == locked {
        return;
    }
    let Some(main) = app.get_webview_window("main") else { return };
    let _ = main.set_decorations(!locked);
    let _ = main.set_fullscreen(locked);
    let _ = main.set_always_on_top(locked);
    // Sticky across virtual desktops: Ctrl+Alt+←/→ must not walk away from it.
    let _ = main.set_visible_on_all_workspaces(locked);
    if locked {
        take_over();
        let _ = main.unminimize();
        let _ = main.set_focus();
    } else {
        // Debt paid: give the desktop back.
        let _ = main.minimize();
        give_back();
    }
}

/// Take over the view: once a second while locked, un-minimize, raise and
/// focus the lock window — whichever workspace or app the user wandered to.
pub(crate) fn refocus(app: &AppHandle) {
    let _guard = WINDOW_ACTIONS.lock().unwrap();
    if !app.state::<crate::Runtime>().enforces_windows() { return; }
    if !LOCKED.load(Ordering::SeqCst) {
        return;
    }
    take_over();
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.unminimize();
        let _ = main.set_fullscreen(true);
        let _ = main.set_always_on_top(true);
        let _ = main.set_focus();
        // GTK's focus request loses to focus-stealing prevention once the
        // user has minimized us; a pager-sourced _NET_ACTIVE_WINDOW doesn't.
        x11("activate", &[main.title().unwrap_or_default()]);
    }
}

#[cfg(test)]
mod tests {
    use super::pick_gym_monitor;

    #[test]
    fn remembered_wins_else_first_non_primary_else_none() {
        let names = ["HDMI-0", "DP-1", "DP-2"];
        assert_eq!(pick_gym_monitor(&names, "HDMI-0", Some("DP-2")), Some(2));
        assert_eq!(pick_gym_monitor(&names, "HDMI-0", None), Some(1));
        assert_eq!(pick_gym_monitor(&names[..1], "HDMI-0", None), None);
        // A remembered monitor that's unplugged falls back to the other one.
        assert_eq!(pick_gym_monitor(&names, "HDMI-0", Some("gone")), Some(1));
    }
}

/// Release only windows this process hid; never minimize unrelated windows here.
pub(crate) fn release(app: &AppHandle) {
    let _guard = WINDOW_ACTIONS.lock().unwrap();
    LOCKED.store(false, Ordering::SeqCst);
    for label in ["main", "gym"] {
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.set_fullscreen(false);
            let _ = window.set_always_on_top(false);
            let _ = window.set_visible_on_all_workspaces(false);
            let _ = window.set_decorations(false);
        }
    }
    give_back();
}
