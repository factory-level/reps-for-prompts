//! The system tray (V2 spec §4.3): the always-there way back in, and the
//! easy way out (pause, end, quit).
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use engine::types::{Phase, Snapshot};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager};

use crate::{ui, windows, Lifecycle};

static AVAILABLE: AtomicBool = AtomicBool::new(false);
static LAST: Mutex<String> = Mutex::new(String::new());

/// The tray icon exists, so closing every window must not quit.
pub fn available() -> bool { AVAILABLE.load(Ordering::SeqCst) }

pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let item = |id: &str, text: &str| MenuItem::with_id(app, id, text, true, None::<&str>);
    let menu = Menu::with_items(app, &[
        &item("open", "Open RFP")?,
        &item("monitor", "Show workout monitor")?,
        &item("history", "Progress & History")?,
        &PredefinedMenuItem::separator(app)?,
        &item("snooze", "Snooze")?,
        &item("pause", "Pause / Resume")?,
        &item("end", "End workout")?,
        &PredefinedMenuItem::separator(app)?,
        &item("quit", "Quit RFP")?,
    ])?;
    let mut tray = TrayIconBuilder::with_id("rfp").menu(&menu).tooltip("RFP · Ready")
        .on_menu_event(|app, event| {
            let life = app.state::<Lifecycle>();
            let result = match event.id().as_ref() {
                "open" => { ui::open(app, "home"); Ok(()) }
                "monitor" => { windows::put_gym(app); Ok(()) }
                "history" => { ui::open(app, "history"); Ok(()) }
                "snooze" => ui::snooze(app.clone()),
                "pause" => if life.snapshot().paused { ui::resume(app.clone()) } else { ui::pause(app.clone()) },
                "end" => { ui::end_workout(app.clone()); Ok(()) }
                // Exit with a code: the exit handler persists and stops capture.
                "quit" => { app.exit(0); Ok(()) }
                _ => Ok(()),
            };
            if let Err(error) = result { crate::daily::notify(error); }
        });
    if let Some(icon) = app.default_window_icon() { tray = tray.icon(icon.clone()); }
    tray.build(app)?;
    AVAILABLE.store(true, Ordering::SeqCst);
    Ok(())
}

/// Tooltip text for the state (text, not just an icon colour).
pub fn status(app: &AppHandle, snap: &Snapshot) {
    let text = if snap.paused { "Paused".to_string() } else {
        match snap.phase {
            Phase::Idle => "Ready".into(),
            Phase::Coding => format!("Coding · {}:{:02}", snap.remaining_seconds as u64 / 60, snap.remaining_seconds as u64 % 60),
            Phase::ExerciseRequired => "Set due".into(),
            Phase::WorkoutActive => "Exercising".into(),
            Phase::WeightConfirmation => "Weight pending".into(),
            Phase::Unlocked => "Set logged".into(),
        }
    };
    // Coding only refreshes once a minute: the countdown is on the monitor.
    let shown = if snap.phase == Phase::Coding && !snap.paused { "Coding".to_string() } else { text.clone() };
    let mut last = LAST.lock().unwrap();
    if *last == shown { return; }
    *last = shown;
    if let Some(tray) = app.tray_by_id("rfp") { let _ = tray.set_tooltip(Some(format!("RFP · {text}"))); }
}
