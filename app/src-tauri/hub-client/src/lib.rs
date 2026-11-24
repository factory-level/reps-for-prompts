//! Client for the usb-mcp-hub vision SDK (client API v1).
//!
//! The engine crate stays hub-free: `lib.rs` in the app crate pumps
//! [`VisionEvent`]s from a [`VisionHub`] implementation into the session
//! state machine. `FakeHub` serves tests; `HubClient` speaks the real
//! WebSocket API; `HubSupervisor` owns the bundled hubd process.

use std::sync::mpsc;

pub mod client;
pub mod fake;
pub mod supervisor;
pub mod outbox;

pub use client::HubClient;
pub use fake::FakeHub;
pub use supervisor::{HubSupervisor, HubSupervisorConfig};

pub const SUPPORTED_API_MAJOR: &str = "1";

#[derive(Debug, Clone, PartialEq)]
pub enum VisionEvent {
    /// Full landmark frame for the UI overlay (passed through untyped).
    Landmarks(serde_json::Value),
    CameraFrame { camera_id: String, data: serde_json::Value },
    /// Live activity progress — maps 1:1 onto `engine::types::Progress`.
    Progress {
        value: f64,
        unit: String,
        satisfied: bool,
        context: ProgressContext,
    },
    /// Semantic events: rep_completed, target_reached, …
    Semantic {
        kind: String,
        payload: serde_json::Value,
    },
    Health(HubHealth),
    ConnectionLost,
}

/// Preserve routing identity through the client instead of losing it at decode.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProgressContext {
    pub metric_id: Option<String>,
    pub session_id: Option<String>,
    pub consensus: Option<serde_json::Value>,
    pub camera_status: Option<String>,
}

impl ProgressContext {
    pub fn belongs_to(&self, metric_id: &str, session_id: Option<&str>) -> bool {
        self.metric_id.as_deref() == Some(metric_id)
            && session_id.is_some()
            && self.session_id.as_deref() == session_id
    }
}

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct HubHealth {
    #[serde(rename = "visionHost")]
    pub vision_host: String,
    pub camera: String,
    #[serde(rename = "enabledMetrics")]
    pub enabled_metrics: Vec<String>,
}

impl HubHealth {
    pub fn failed() -> Self {
        HubHealth {
            vision_host: "down".into(),
            camera: "closed".into(),
            enabled_metrics: vec![],
        }
    }
}

#[derive(Debug, Clone)]
pub struct EnableMetric {
    pub metric_id: String,
    pub plugin_id: String,
    /// Camera-set metrics (API 1.3): one stream per camera id, frames fused
    /// by the hub. Exclusive with a `camera` object inside `config`.
    pub cameras: Option<Vec<String>>,
    pub config: serde_json::Value,
}

#[derive(Debug)]
pub enum HubError {
    Io(String),
    Api(String),
    VersionMismatch { hub: String, supported: String },
    Timeout(String),
    Down,
}

impl std::fmt::Display for HubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HubError::Io(msg) => write!(f, "hub io error: {msg}"),
            HubError::Api(msg) => write!(f, "hub api error: {msg}"),
            HubError::VersionMismatch { hub, supported } => {
                write!(f, "hub api version {hub} unsupported (need {supported}.x)")
            }
            HubError::Timeout(what) => write!(f, "hub timeout: {what}"),
            HubError::Down => write!(f, "hub is down"),
        }
    }
}

impl std::error::Error for HubError {}

/// The vision backend the reps core drives. Lock → `enable_metric`,
/// unlock/abort → `disable_metric`; events arrive on the receiver.
pub trait VisionHub: Send {
    fn enable_metric(&mut self, req: &EnableMetric) -> Result<(), HubError>;
    fn disable_metric(&mut self, metric_id: &str) -> Result<(), HubError>;
    fn update_metric_config(
        &mut self,
        metric_id: &str,
        config: &serde_json::Value,
    ) -> Result<(), HubError>;
    fn simulate(&mut self, metric_id: &str, event: &serde_json::Value) -> Result<(), HubError>;
    /// Register a camera in the hub's registry (API 1.1+). Required — a
    /// silent default here would let a real client no-op registration.
    fn add_camera(&mut self, camera: &serde_json::Value) -> Result<(), HubError>;
    /// Per-camera config overlay on a camera-set metric (API 1.3).
    fn update_metric_config_for_camera(
        &mut self,
        metric_id: &str,
        camera_id: &str,
        config: &serde_json::Value,
    ) -> Result<(), HubError>;
    fn health(&mut self) -> Result<HubHealth, HubError>;
    /// The event stream; `None` after the first call.
    fn take_receiver(&mut self) -> Option<mpsc::Receiver<VisionEvent>>;

    // FieldLab V1 (API 1.4). Required, not defaulted — a silent default would
    // let a real client no-op the app's durable history (same reasoning as
    // add_camera above).

    /// Register the app manifest (event + command schemas) with the hub.
    fn register_application(
        &mut self,
        app_id: &str,
        version: &str,
        manifest: &serde_json::Value,
    ) -> Result<(), HubError>;
    /// Publish an app-defined semantic event into durable hub history.
    /// `params` is the wire shape: {appId, type, payload?, sessionId?, ...}.
    fn publish_event(&mut self, params: &serde_json::Value) -> Result<(), HubError>;
    /// Record a requested operation (distinct from its outcome).
    fn publish_command(&mut self, params: &serde_json::Value) -> Result<(), HubError>;
    /// Record a command/action outcome.
    fn report_action_result(&mut self, params: &serde_json::Value) -> Result<(), HubError>;
}

/// Convert a client-API push frame into a `VisionEvent`.
pub(crate) fn event_from_frame(frame: &serde_json::Value) -> Option<VisionEvent> {
    let stream = frame.get("stream")?.as_str()?;
    let data = frame.get("data").cloned().unwrap_or(serde_json::Value::Null);
    match stream {
        "landmarks" => Some(VisionEvent::Landmarks(data)),
        "frame" => Some(VisionEvent::CameraFrame { camera_id: frame.get("cameraId")?.as_str()?.to_string(), data }),
        "progress" => {
            let value = data.get("value")?.as_f64()?;
            let unit = data.get("unit")?.as_str()?;
            if !value.is_finite() || value < 0.0 || !matches!(unit, "reps" | "seconds") {
                return None;
            }
            Some(VisionEvent::Progress {
                value,
                unit: unit.to_string(),
                satisfied: data.get("satisfied")?.as_bool()?,
                context: ProgressContext {
                    consensus: data.get("consensus").cloned(),
                    metric_id: frame.get("metricId").and_then(|v| v.as_str()).map(str::to_string),
                    session_id: data.get("sessionId").and_then(|v| v.as_str()).map(str::to_string),
                    camera_status: data.get("cameraStatus").and_then(|v| v.as_str()).map(str::to_string),
                },
            })
        }
        "event" => Some(VisionEvent::Semantic {
            kind: data
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            payload: data,
        }),
        "health" => serde_json::from_value(data).ok().map(VisionEvent::Health),
        _ => None,
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preserves_identity_and_rejects_other_workouts() {
        let frame = json!({"stream":"progress", "metricId":"workout", "data":{
            "sessionId":"current", "value":2, "unit":"reps", "satisfied":false,
            "cameraStatus":"disconnected"
        }});
        let Some(VisionEvent::Progress { context, .. }) = event_from_frame(&frame) else { panic!() };
        assert_eq!(context.camera_status.as_deref(), Some("disconnected"));
        assert!(context.belongs_to("workout", Some("current")));
        assert!(!context.belongs_to("workout", Some("previous")));
        assert!(!context.belongs_to("preview", Some("current")));
        assert!(!context.belongs_to("workout", None));
        assert!(!ProgressContext::default().belongs_to("workout", None));
    }

    #[test]
    fn malformed_progress_never_becomes_credit() {
        for data in [json!({"satisfied":true}),
            json!({"value":-1, "unit":"reps", "satisfied":true}),
            json!({"value":10, "unit":"unknown", "satisfied":true}),
            json!({"value":10, "unit":"reps", "satisfied":"yes"})] {
            assert!(event_from_frame(&json!({"stream":"progress", "data":data})).is_none());
        }
    }
}
