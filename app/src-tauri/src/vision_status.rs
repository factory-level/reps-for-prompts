//! "Is the camera/vision path working right now?" — one pure reducer. hub.rs
//! feeds it signals; the display state carries `status()` to the windows and
//! the CLI. Storage/publication errors are deliberately not signals.

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum VisionStatus {
    #[default]
    Starting,
    Available,
    Unavailable { reason: String },
}

pub enum Signal {
    /// The hub process could not start (it keeps retrying).
    HubStartFailed(String),
    /// The hub answered: started, respawned, or accepted an enable.
    HubUp,
    /// The supervised hub died and will not be restarted.
    HubDown,
    ConnectionLost,
    /// This workout's detector never started.
    EnableFailed(String),
    /// This workout's detector stopped (detector_error / stream_ended).
    DetectorFailed(String),
    CameraDisconnected,
    /// A real progress frame for the current workout.
    LiveFrame,
    /// Detector disabled / workout over.
    DetectorOff,
}

/// Two levels: the hub process, and this workout's detector. A detector-level
/// error outlives `HubUp` until a live frame or the end of the workout; only a
/// set whose running detector lost the hub is re-enabled when it comes back.
#[derive(Default)]
pub struct Vision {
    hub_up: bool,
    hub_error: Option<String>,
    detector_error: Option<String>,
}

impl Vision {
    /// Returns whether the status changed.
    pub fn apply(&mut self, signal: Signal) -> bool {
        let before = self.status();
        match signal {
            Signal::HubStartFailed(error) => self.hub_error = Some(error),
            Signal::HubUp => { self.hub_up = true; self.hub_error = None; }
            Signal::HubDown => self.hub_error = Some("Vision hub is down".into()),
            Signal::ConnectionLost => self.hub_error = Some("Vision hub connection lost".into()),
            Signal::EnableFailed(error) | Signal::DetectorFailed(error) => self.detector_error = Some(error),
            Signal::CameraDisconnected => self.detector_error = Some("Camera disconnected".into()),
            Signal::LiveFrame => { self.hub_up = true; self.hub_error = None; self.detector_error = None; }
            Signal::DetectorOff => self.detector_error = None,
        }
        self.status() != before
    }

    pub fn status(&self) -> VisionStatus {
        match self.hub_error.as_ref().or(self.detector_error.as_ref()) {
            Some(reason) => VisionStatus::Unavailable { reason: reason.clone() },
            None if self.hub_up => VisionStatus::Available,
            None => VisionStatus::Starting,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn after(signals: impl IntoIterator<Item = Signal>) -> Vision {
        let mut vision = Vision::default();
        for signal in signals { vision.apply(signal); }
        vision
    }
    fn unavailable(vision: &Vision) -> bool { matches!(vision.status(), VisionStatus::Unavailable { .. }) }

    #[test]
    fn starts_as_starting() {
        assert_eq!(Vision::default().status(), VisionStatus::Starting);
    }

    #[test]
    fn a_start_failure_that_recovers_is_available() {
        let mut vision = after([Signal::HubStartFailed("port in use".into())]);
        assert_eq!(vision.status(), VisionStatus::Unavailable { reason: "port in use".into() });
        assert!(vision.apply(Signal::HubUp));
        assert_eq!(vision.status(), VisionStatus::Available);
    }

    #[test]
    fn a_reconnected_camera_is_available_again() {
        let mut vision = after([Signal::HubUp, Signal::CameraDisconnected]);
        assert!(unavailable(&vision));
        assert!(vision.apply(Signal::LiveFrame));
        assert_eq!(vision.status(), VisionStatus::Available);
    }

    #[test]
    fn an_enable_that_failed_while_the_hub_was_down_outlives_hub_up() {
        // This set's detector never ran, so nothing re-enables it.
        let vision = after([Signal::HubStartFailed("x".into()), Signal::EnableFailed("hub not running".into()), Signal::HubUp]);
        assert_eq!(vision.status(), VisionStatus::Unavailable { reason: "hub not running".into() });
    }

    #[test]
    fn detector_off_clears_the_detector_level_only() {
        let mut vision = after([Signal::HubUp, Signal::DetectorFailed("stream_ended".into())]);
        assert!(vision.apply(Signal::DetectorOff));
        assert_eq!(vision.status(), VisionStatus::Available);

        let mut vision = after([Signal::HubUp, Signal::ConnectionLost, Signal::EnableFailed("x".into())]);
        assert!(!vision.apply(Signal::DetectorOff));
        assert_eq!(vision.status(), VisionStatus::Unavailable { reason: "Vision hub connection lost".into() });
    }

    #[test]
    fn a_lost_or_dead_hub_is_unavailable() {
        assert!(unavailable(&after([Signal::HubUp, Signal::ConnectionLost])));
        assert!(unavailable(&after([Signal::HubUp, Signal::HubDown])));
    }

    #[test]
    fn apply_reports_no_change_when_the_status_is_the_same() {
        let mut vision = Vision::default();
        assert!(!vision.apply(Signal::DetectorOff));
        assert!(vision.apply(Signal::HubUp));
        assert!(!vision.apply(Signal::HubUp));
        assert!(!vision.apply(Signal::LiveFrame));
    }

    #[test]
    fn serializes_as_a_tagged_state() {
        assert_eq!(serde_json::to_value(VisionStatus::Starting).unwrap(), serde_json::json!({"state": "starting"}));
        assert_eq!(
            serde_json::to_value(VisionStatus::Unavailable { reason: "x".into() }).unwrap(),
            serde_json::json!({"state": "unavailable", "reason": "x"}),
        );
    }
}
