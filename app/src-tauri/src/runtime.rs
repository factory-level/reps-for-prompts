//! Startup policy, separate from workout phases. Debug never shares workout data.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppMode { #[default] Debug, Workout }

pub struct Runtime {
    pub mode: AppMode,
    pub normal_home: PathBuf,
    pub session_home: PathBuf,
    pub stopping: AtomicBool,
    pub lock_mode: AtomicBool,
    pub detector_generation: AtomicU64,
}

impl Runtime {
    pub fn load(normal_home: PathBuf) -> Result<Self, String> {
        // A missing or unreadable preference must never unexpectedly lock a desktop.
        let mode = std::fs::read(normal_home.join("mode.json")).ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        let session_home = if mode == AppMode::Debug {
            let path = std::env::temp_dir().join(format!("reps-debug-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).map_err(|e| e.to_string())?;
            if normal_home.join("routine.json").is_file() {
                if let Err(error) = std::fs::copy(normal_home.join("routine.json"), path.join("routine.json")) {
                    let _ = std::fs::remove_dir_all(&path);
                    return Err(error.to_string());
                }
            }
            path
        } else { normal_home.clone() };
        Ok(Self {mode, normal_home, session_home, stopping: AtomicBool::new(false), lock_mode: AtomicBool::new(false), detector_generation: AtomicU64::new(0)})
    }

    pub fn is_debug(&self) -> bool { self.mode == AppMode::Debug }
    pub fn is_stopping(&self) -> bool { self.stopping.load(Ordering::SeqCst) }
    pub fn enforces_windows(&self) -> bool { !self.is_debug() && !self.is_stopping() && self.lock_mode.load(Ordering::SeqCst) }
    pub fn require_debug(&self) -> Result<(), String> {
        if self.is_debug() && !self.is_stopping() { Ok(()) }
        else { Err("Manual testing requires Debug mode".into()) }
    }
    pub fn begin_stop(&self) -> bool { !self.stopping.swap(true, Ordering::SeqCst) }
    pub fn save_mode(&self, mode: AppMode) -> Result<(), String> { save_mode(&self.normal_home, mode) }
    pub fn cleanup(&self) {
        if self.is_debug() { let _ = std::fs::remove_dir_all(&self.session_home); }
    }
}

fn save_mode(home: &Path, mode: AppMode) -> Result<(), String> {
    std::fs::create_dir_all(home).map_err(|e| e.to_string())?;
    let temporary = home.join(format!("mode-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        use std::io::Write;
        let mut file = std::fs::File::create(&temporary).map_err(|e| e.to_string())?;
        file.write_all(&serde_json::to_vec(&mode).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        std::fs::rename(&temporary, home.join("mode.json")).map_err(|e| e.to_string())
    })();
    if result.is_err() { let _ = std::fs::remove_file(temporary); }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn home() -> PathBuf { std::env::temp_dir().join(format!("reps-mode-test-{}", uuid::Uuid::new_v4())) }
    #[test]
    fn defaults_to_debug_and_persists_both_choices() {
        let home = home();
        let runtime = Runtime::load(home.clone()).unwrap();
        assert!(runtime.is_debug()); assert!(!runtime.enforces_windows());
        assert_ne!(runtime.session_home, home);
        assert!(!home.exists()); // Opening Debug did not create a real workout DB.
        runtime.save_mode(AppMode::Workout).unwrap(); runtime.cleanup();
        let normal = Runtime::load(home.clone()).unwrap();
        assert_eq!(normal.session_home, home); assert!(!normal.enforces_windows());
        normal.lock_mode.store(true, Ordering::SeqCst);
        assert!(normal.enforces_windows());
        assert!(normal.require_debug().is_err());
        assert!(normal.begin_stop()); assert!(!normal.begin_stop());
        assert!(!normal.enforces_windows());
        normal.save_mode(AppMode::Debug).unwrap();
        let debug = Runtime::load(home.clone()).unwrap();
        assert!(debug.require_debug().is_ok());
        debug.begin_stop(); assert!(debug.require_debug().is_err());
        debug.cleanup(); std::fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn invalid_preference_is_windowed_and_debug_does_not_copy_history() {
        let home = home(); std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("mode.json"), "broken").unwrap();
        std::fs::write(home.join("reps.sqlite"), "real-history").unwrap();
        let runtime = Runtime::load(home.clone()).unwrap();
        assert!(!runtime.enforces_windows());
        assert!(!runtime.session_home.join("reps.sqlite").exists());
        runtime.cleanup();
        assert_eq!(std::fs::read_to_string(home.join("reps.sqlite")).unwrap(), "real-history");
        std::fs::remove_dir_all(home).unwrap();
    }
}

#[cfg(test)]
mod isolation_tests {
    use super::*;
    use engine::{clock::{Clock, SystemClock}, types::Progress};
    #[test]
    fn completed_simulation_only_writes_temporary_history() {
        let home = std::env::temp_dir().join(format!("reps-isolation-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&home).unwrap();
        let normal = crate::build_core(&home);
        normal.store.set_setting("plan_state", "real-plan-sentinel").unwrap();
        let before = normal.store.history(100).unwrap().len();
        let runtime = Runtime::load(home.clone()).unwrap();
        let mut debug = crate::build_core(&runtime.session_home);
        let _ = debug.session.start_workout(&SystemClock.today());
        let _ = debug.session.report_progress(Progress { value: 999.0, unit: "reps".into(), satisfied: true });
        let _ = debug.session.confirm_weight(25.0, &SystemClock.today());
        crate::lifecycle::persist_and_snapshot(&mut debug, SystemClock.now());
        assert_eq!(debug.store.history(100).unwrap().len(), 1);
        assert_eq!(normal.store.history(100).unwrap().len(), before);
        assert_eq!(normal.store.setting("plan_state", ""), "real-plan-sentinel");
        drop(debug); runtime.cleanup();
        assert!(!runtime.session_home.exists());
        drop(normal); std::fs::remove_dir_all(home).unwrap();
    }
}
