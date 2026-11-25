//! Window roles (V2 spec §10.2). `main` is the launcher/config window, a
//! normal app window. `gym` is the workout monitor on its own display:
//! placed on the remembered (else non-primary) monitor, fullscreen unless the
//! user turned that off. `prompt` is the small work-desktop reminder, centered
//! on a display that isn't the gym's. Nothing here touches other apps'
//! windows, locks input, or steals focus on a loop.
use tauri::{AppHandle, Manager, PhysicalPosition, Position, WindowEvent};

use crate::Lifecycle;

const GYM_MONITOR: &str = "gym_monitor";
const GYM_FULLSCREEN: &str = "gym_fullscreen";

/// Index of the monitor the gym window belongs on: the remembered name (or
/// `$REPS_GYM_MONITOR`) wins, else the first monitor that isn't the primary.
pub(crate) fn pick_gym_monitor(names: &[&str], primary: &str, want: Option<&str>) -> Option<usize> {
    want.and_then(|w| names.iter().position(|n| *n == w))
        .or_else(|| names.iter().position(|n| *n != primary))
}

fn setting(app: &AppHandle, key: &str) -> String {
    app.state::<Lifecycle>().core().store.setting(key, "")
}

fn remember(app: &AppHandle, key: &str, value: &str) {
    let _ = app.state::<Lifecycle>().core().store.set_setting(key, value);
}

/// Startup: show the launcher (unless started in the background) and watch
/// the gym window so wherever the user drags it is remembered.
pub(crate) fn place(app: &AppHandle) {
    if !std::env::args().any(|a| a == "--background") {
        if let Some(main) = app.get_webview_window("main") { center_on_primary(&main); let _ = main.show(); }
    }
    let Some(gym) = app.get_webview_window("gym") else { return };
    let handle = app.clone();
    let watched = gym.clone();
    gym.on_window_event(move |e| {
        if let WindowEvent::Moved(_) = e {
            if let Ok(Some(m)) = watched.current_monitor() {
                if let Some(name) = m.name() {
                    if setting(&handle, GYM_MONITOR) != *name { remember(&handle, GYM_MONITOR, name); }
                }
            }
        }
    });
}

/// Center a window on the primary monitor (`center()` can span the whole
/// virtual desktop on multi-monitor X), shrinking it to fit.
fn center_on_primary(w: &tauri::WebviewWindow) {
    let _ = w.show(); // X ignores geometry set before the first map
    let Some(m) = w.primary_monitor().ok().flatten().or_else(|| w.available_monitors().ok().and_then(|v| v.into_iter().next())) else { return };
    let (pos, area, scale) = (m.position(), m.size(), m.scale_factor());
    let width = ((960. * scale) as u32).min(area.width * 9 / 10);
    let height = ((680. * scale) as u32).min(area.height * 9 / 10);
    let _ = w.set_size(tauri::PhysicalSize { width, height });
    let _ = w.set_position(Position::Physical(PhysicalPosition {
        x: pos.x + (area.width - width) as i32 / 2, y: pos.y + (area.height - height) as i32 / 2 }));
}

/// Show the gym window on its monitor (remembered name, `$REPS_GYM_MONITOR`,
/// else the first non-primary one), fullscreen unless F11'd out. With one
/// display it stays a plain movable window: never cover the work screen.
pub(crate) fn put_gym(app: &AppHandle) -> Option<usize> {
    let gym = app.get_webview_window("gym")?;
    let _ = gym.show();
    // The gym display stays put on every workspace (it's on its own screen).
    let _ = gym.set_visible_on_all_workspaces(true);
    let monitors = gym.available_monitors().unwrap_or_default();
    let primary = gym.primary_monitor().ok().flatten();
    let names: Vec<&str> = monitors.iter().map(|m| m.name().map(String::as_str).unwrap_or("")).collect();
    let primary_name = primary.as_ref().and_then(|p| p.name()).map(String::as_str).unwrap_or("");
    let want = std::env::var("REPS_GYM_MONITOR").ok().or_else(|| Some(setting(app, GYM_MONITOR))).filter(|s| !s.is_empty());
    let gym_at = pick_gym_monitor(&names, primary_name, want.as_deref());
    match gym_at {
        Some(i) => {
            let _ = gym.set_position(Position::Physical(*monitors[i].position()));
            if gym_wants_fullscreen(app) { let _ = gym.set_fullscreen(true); } else { let _ = gym.maximize(); }
        }
        None => eprintln!("windows: no gym monitor (have {names:?}); gym stays a plain window"),
    }
    gym_at
}

/// Forget where the gym window lives.
pub(crate) fn reset_gym(app: &AppHandle) {
    remember(app, GYM_MONITOR, "");
    remember(app, GYM_FULLSCREEN, "");
    if let Some(gym) = app.get_webview_window("gym") { let _ = gym.set_fullscreen(false); let _ = gym.center(); }
}

/// Fullscreen unless the user F11'd out of it (to drag it somewhere).
fn gym_wants_fullscreen(app: &AppHandle) -> bool {
    setting(app, GYM_FULLSCREEN) != "0"
}

/// F11 / the on-screen button in the gym window: flip fullscreen and remember it.
pub(crate) fn toggle_gym_fullscreen(app: &AppHandle) -> bool {
    let Some(gym) = app.get_webview_window("gym") else { return false };
    let on = !gym.is_fullscreen().unwrap_or(false);
    remember(app, GYM_FULLSCREEN, if on { "1" } else { "0" });
    let _ = gym.set_fullscreen(on);
    on
}

/// Show the reminder, centered on the first display that isn't the gym's
/// (the only display when there is one). Asks for attention once; never
/// forces focus. Visible on every workspace where the WM allows it.
pub(crate) fn show_prompt(app: &AppHandle) {
    let Some(prompt) = app.get_webview_window("prompt") else { return };
    let gym_monitor = app.get_webview_window("gym")
        .filter(|g| g.is_visible().unwrap_or(false))
        .and_then(|g| g.current_monitor().ok().flatten())
        .and_then(|m| m.name().cloned());
    let _ = prompt.set_always_on_top(true);
    let _ = prompt.set_visible_on_all_workspaces(true);
    let _ = prompt.show(); // X ignores geometry set before the first map
    let monitors = prompt.available_monitors().unwrap_or_default();
    let target = monitors.iter().find(|m| m.name().cloned() != gym_monitor).or(monitors.first());
    if let Some(m) = target {
        let (pos, area, scale) = (m.position(), m.size(), m.scale_factor());
        let (w, h) = ((500. * scale) as i32, (360. * scale) as i32);
        let x = pos.x + (area.width as i32 - w).max(0) / 2;
        let y = pos.y + (area.height as i32 - h).max(0) / 2;
        let _ = prompt.set_position(Position::Physical(PhysicalPosition { x, y }));
    }
    let _ = prompt.request_user_attention(Some(tauri::UserAttentionType::Informational));
}

pub(crate) fn hide_prompt(app: &AppHandle) {
    if let Some(prompt) = app.get_webview_window("prompt") { let _ = prompt.hide(); }
}

pub(crate) fn show_main(app: &AppHandle) {
    if let Some(main) = app.get_webview_window("main") { if !main.is_visible().unwrap_or(true) { center_on_primary(&main); } let _ = main.show(); let _ = main.unminimize(); let _ = main.set_focus(); }
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

