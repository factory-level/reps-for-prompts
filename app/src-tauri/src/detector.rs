//! The detector request: what the hub is asked to run for a prescription —
//! shipped exercise definition, targets and camera selection — built from
//! plain values. `load_inputs` is the only IO (files and environment).

use std::path::Path;

use engine::types::{ExerciseKind, Prescription};
use hub_client::EnableMetric;
use serde_json::{json, Value};

use crate::hub::{APP_ID, WORKOUT_METRIC};

pub const EXERCISE_SPECS: &str = include_str!("../resources/exercise_specs.json");
/// Rep target of a debug set; the debug video stream counts to the same number.
pub const DEBUG_TARGET_REPS: u32 = 10; // matches WorkoutEngine's default_reps

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CameraSettings {
    pub consensus: bool,
    pub usb_device: String,
    #[serde(default = "default_usb_rotation")]
    pub usb_rotation: u16,
    pub phone_url: String,
    pub phone_rotation: u16,
}
/// The rig's webcam is mounted upside-down, and every exercise was tuned on
/// frames rotated 180° — so the app must rotate identically or the joint
/// angles are inverted and no rep crosses its thresholds.
fn default_usb_rotation() -> u16 { 180 }
pub fn phone_stream_url(value: &str) -> bool {
    ["rtsp://", "rtsps://", "http://", "https://"].iter().any(|prefix| value.starts_with(prefix))
}
impl Default for CameraSettings {
    fn default() -> Self { Self { consensus: false, usb_device: "/dev/video0".into(), usb_rotation: default_usb_rotation(), phone_url: String::new(), phone_rotation: 0 } }
}
impl CameraSettings {
    pub fn validate(&self) -> Result<(), String> {
        if !matches!(self.usb_rotation, 0 | 90 | 180 | 270) || !matches!(self.phone_rotation, 0 | 90 | 180 | 270) || !self.usb_device.starts_with("/dev/video") {
            return Err("Choose a /dev/video device and a valid rotation".into());
        }
        if self.consensus && (!phone_stream_url(&self.phone_url) || self.phone_url.contains('@')) {
            return Err("Enter an RTSP or HTTP MJPEG URL without embedded credentials".into());
        }
        Ok(())
    }
}

/// The saved camera choice; `None` when there is no `cameras.json`, which
/// leaves the env camera index in charge.
pub fn load_camera_settings(normal_home: &Path) -> Result<Option<CameraSettings>, String> {
    match std::fs::read(normal_home.join("cameras.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Parse and sanity-check the shipped specs at startup: the model must be
/// declared and every exercise must name a known activity.
pub fn load_exercise_specs() -> Result<Value, String> {
    let specs: Value =
        serde_json::from_str(EXERCISE_SPECS).map_err(|e| format!("exercise_specs.json: {e}"))?;
    let model = specs
        .get("model")
        .and_then(|m| m.get("plugin"))
        .and_then(|p| p.as_str())
        .ok_or("exercise_specs.json: missing model.plugin declaration")?;
    if model.is_empty() {
        return Err("exercise_specs.json: empty model.plugin".into());
    }
    let exercises = specs
        .get("exercises")
        .and_then(|e| e.as_object())
        .ok_or("exercise_specs.json: missing exercises")?;
    for (name, entry) in exercises {
        let activity = entry.get("activity").and_then(|a| a.as_str()).unwrap_or("");
        if !matches!(activity, "lift" | "jumprope" | "stretch") {
            return Err(format!("exercise {name}: unknown activity {activity:?}"));
        }
    }
    Ok(specs)
}

/// One selectable debug set per shipped exercise.
pub fn debug_exercise_options() -> Result<Vec<Prescription>, String> {
    let specs = load_exercise_specs()?;
    let exercises = specs["exercises"].as_object().ok_or("Missing exercise specifications")?;
    Ok(exercises.iter().map(|(name, spec)| {
        let seconds = match spec["activity"].as_str() {
            Some("jumprope") => spec["targetSeconds"].as_f64().unwrap_or(60.0),
            Some("stretch") => spec["holdSeconds"].as_f64().unwrap_or(30.0),
            _ => 0.0,
        };
        Prescription { exercise: name.clone(), kind: if seconds > 0.0 { ExerciseKind::Continuous } else { ExerciseKind::Rep },
            target_reps: if seconds > 0.0 { 0 } else { DEBUG_TARGET_REPS }, target_seconds: seconds, default_weight: 0.0 }
    }).collect())
}

/// Everything outside the prescription that shapes a detector request.
pub struct DetectorInputs {
    /// The shipped `exercise_specs.json`.
    pub specs: Value,
    /// `cameras.json`. `Ok(None)` = no file, so the env camera applies; a
    /// present file overrides the env camera even with consensus off. `Err` =
    /// unreadable or invalid, reported only once the exercise is known.
    pub settings: Result<Option<CameraSettings>, String>,
    /// Trimmed contents of the `REPS_PHONE_URL_FILE` file, when that variable
    /// is set (empty if the file is unreadable). Replaces `phoneUrl`.
    pub phone_url_override: Option<String>,
    /// `camera-calibration.json`, or `{}`.
    pub calibration: Value,
    /// `REPS_CAMERA_INDEX`, default 0.
    pub camera_index: i64,
    /// `REPS_CAMERA_ROTATE`; `None` = the default USB rotation.
    pub camera_rotate: Option<i64>,
}

/// Why detection cannot start, leaving the set to honor mode.
#[derive(Debug, PartialEq)]
pub enum HonorReason {
    /// The shipped specs failed their startup check.
    BadSpecs(String),
    /// No shipped or custom definition for this exercise.
    NoSpec(String),
    /// `REPS_PHONE_URL_FILE` is set but does not yield a stream URL.
    PhoneUrlInvalid,
    /// `cameras.json` could not be read.
    Settings(String),
}

/// The hub request for one prescription, plus the camera set to register when
/// two cameras vote. No IO, no environment, no clock. The caller stamps
/// `config.sessionId`, which is only known once the request is about to be sent.
pub fn detector_request(
    inputs: &DetectorInputs,
    exercise: &str,
    target_reps: u32,
    target_seconds: f64,
) -> Result<(EnableMetric, Option<CameraSet>), HonorReason> {
    let (plugin_id, mut config) = metric_config_for(
        &inputs.specs, exercise, target_reps, target_seconds, inputs.camera_index, inputs.camera_rotate,
    ).ok_or_else(|| HonorReason::NoSpec(exercise.into()))?;
    let mut consensus_cameras = None;
    match inputs.settings.as_ref().map_err(|error| HonorReason::Settings(error.clone()))? {
        Some(settings) if settings.consensus && config.get("movementId").is_some() => {
            // Require an active version: legacy exercise fallback has no cycle evidence.
            if let Some(object) = config.as_object_mut() { object.remove("exercise"); }
            let phone_source = match &inputs.phone_url_override {
                Some(url) if phone_stream_url(url) => url.clone(),
                Some(_) => return Err(HonorReason::PhoneUrlInvalid),
                None => settings.phone_url.clone(),
            };
            config["cameraCalibration"] = inputs.calibration.clone();
            consensus_cameras = Some(json!({ "set": ["webcam", "phone"],
                "fusion": {"policy": "consensus", "quorum": 2},
                "registry": [
                    {"cameraId": "webcam", "label": "USB webcam", "kind": "usb", "transport": "direct", "source": settings.usb_device, "rotate": settings.usb_rotation},
                    {"cameraId": "phone", "label": "Android phone", "kind": "rtsp", "transport": "direct", "source": phone_source, "rotate": settings.phone_rotation}
                ] }));
        }
        Some(settings) => {
            config["camera"]["source"] = json!("uri");
            config["camera"]["value"] = json!(settings.usb_device);
            config["camera"]["rotate"] = json!(settings.usb_rotation);
        }
        None => {}
    }
    config["appId"] = json!(APP_ID);
    let camera_set = apply_camera_set(consensus_cameras.as_ref().or_else(|| inputs.specs.get("cameras")), &mut config);
    let request = EnableMetric {
        metric_id: WORKOUT_METRIC.into(),
        plugin_id,
        cameras: camera_set.as_ref().map(|set| set.cameras.clone()),
        config,
    };
    Ok((request, camera_set))
}

/// `REPS_CAMERA_INDEX` and `REPS_CAMERA_ROTATE` (0/90/180/270).
pub fn env_camera() -> (i64, Option<i64>) {
    let number = |name| std::env::var(name).ok().and_then(|v: String| v.parse().ok());
    (number("REPS_CAMERA_INDEX").unwrap_or(0), number("REPS_CAMERA_ROTATE"))
}

/// Read everything `detector_request` needs from the data directory and the
/// environment. The only IO in this module.
pub fn load_inputs(normal_home: &Path) -> Result<DetectorInputs, HonorReason> {
    let (camera_index, camera_rotate) = env_camera();
    Ok(DetectorInputs {
        specs: load_exercise_specs().map_err(HonorReason::BadSpecs)?,
        settings: load_camera_settings(normal_home),
        phone_url_override: std::env::var("REPS_PHONE_URL_FILE").ok()
            .map(|path| std::fs::read_to_string(path).map(|value| value.trim().to_string()).unwrap_or_default()),
        calibration: std::fs::read(normal_home.join("camera-calibration.json")).ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or(json!({})),
        camera_index,
        camera_rotate,
    })
}

/// The enable_metric config for a prescription: shipped exercise definition
/// + targets + the single env-selected camera.
pub fn metric_config_for(
    specs: &Value,
    exercise: &str,
    target_reps: u32,
    target_seconds: f64,
    camera_index: i64,
    camera_rotate: Option<i64>,
) -> Option<(String, Value)> {
    let plugin_id = specs["model"]["plugin"].as_str()?.to_string();
    let mut config = match specs["exercises"].get(exercise) {
        Some(config) => config.clone(),
        // A Studio movement needs no compiled exercise registry entry. Without
        // a legacy exercise fallback the hub requires an activated version.
        None if target_reps > 0 && !exercise.trim().is_empty() => {
            json!({"activity": "lift", "movementId": exercise})
        }
        None => return None,
    };
    let object = config.as_object_mut()?;
    if object.get("activity").and_then(|a| a.as_str()) == Some("lift") {
        object.insert("movementId".into(), json!(exercise));
        object.insert("targetReps".into(), json!(target_reps));
    } else if target_seconds > 0.0 {
        object.insert("targetSeconds".into(), json!(target_seconds));
    }
    let camera_rotate = camera_rotate.unwrap_or(default_usb_rotation().into());
    object.insert(
        "camera".into(),
        json!({"source": "index", "value": camera_index, "id": "webcam", "rotate": camera_rotate}),
    );
    Some((plugin_id, config))
}

/// Registry entries to register, the set to fuse across, and per-camera
/// tuning overlays.
#[derive(Debug, PartialEq)]
pub struct CameraSet {
    pub registry: Vec<Value>,
    pub cameras: Vec<String>,
    /// Per-camera config deltas applied after enable (tuning overlays).
    pub overlays: Vec<(String, Value)>,
}

/// Given a camera-set block, strip the injected single-camera object, put the
/// fusion policy into the config, and return the set.
pub fn apply_camera_set(block: Option<&Value>, config: &mut Value) -> Option<CameraSet> {
    let block = block?;
    let cameras: Vec<String> = block
        .get("set")
        .and_then(|s| s.as_array())
        .map(|set| set.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if cameras.len() < 2 {
        eprintln!("hub: cameras.set needs >= 2 camera ids; falling back to single camera");
        return None;
    }
    let registry = block
        .get("registry")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    let object = config.as_object_mut()?;
    object.remove("camera");
    object.insert(
        "fusion".into(),
        block
            .get("fusion")
            .cloned()
            .unwrap_or_else(|| json!({"policy": "best", "scoreField": "visibility"})),
    );
    let overlays = block
        .get("overlays")
        .and_then(|o| o.as_object())
        .map(|map| {
            map.iter()
                .filter(|(camera_id, _)| cameras.contains(camera_id))
                .map(|(camera_id, delta)| (camera_id.clone(), delta.clone()))
                .collect()
        })
        .unwrap_or_default();
    Some(CameraSet {
        registry,
        cameras,
        overlays,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> DetectorInputs {
        DetectorInputs {
            specs: load_exercise_specs().unwrap(),
            settings: Ok(None),
            phone_url_override: None,
            calibration: json!({}),
            camera_index: 0,
            camera_rotate: None,
        }
    }

    fn saved(consensus: bool) -> Result<Option<CameraSettings>, String> {
        Ok(Some(CameraSettings {
            consensus,
            usb_device: "/dev/video2".into(),
            usb_rotation: 90,
            phone_url: "rtsp://phone.local/live".into(),
            phone_rotation: 270,
        }))
    }

    /// The whole request, as comparable parts.
    fn parts(inputs: &DetectorInputs, exercise: &str, reps: u32, seconds: f64) -> (String, String, Option<Vec<String>>, Value, Option<CameraSet>) {
        let (request, set) = detector_request(inputs, exercise, reps, seconds).unwrap();
        (request.metric_id, request.plugin_id, request.cameras, request.config, set)
    }

    fn squat_definition() -> Value {
        // The (camera-tuned) spec value, not a hardcoded constant.
        let definition = load_exercise_specs().unwrap()["exercises"]["squat"]["exercise"].clone();
        assert!(definition["downBelow"].is_number());
        definition
    }

    #[test]
    fn shipped_specs_parse_and_declare_the_model() {
        let specs = load_exercise_specs().unwrap();
        assert_eq!(specs["model"]["plugin"], "reps_vision");
        assert_eq!(specs["model"]["name"], "mediapipe-pose-landmarker");
        assert!(specs["exercises"].get("squat").is_some());
    }

    #[test]
    fn single_camera_comes_from_env_until_cameras_json_exists() {
        // (settings, env index, env rotate) -> camera object
        let cases = [
            // No cameras.json: env index, and the rig's 180° unless overridden.
            (Ok(None), 0, None, json!({"source": "index", "value": 0, "id": "webcam", "rotate": 180})),
            (Ok(None), 3, Some(90), json!({"source": "index", "value": 3, "id": "webcam", "rotate": 90})),
            // cameras.json present, consensus off: its device and rotation win over env.
            (saved(false), 3, Some(0), json!({"source": "uri", "value": "/dev/video2", "id": "webcam", "rotate": 90})),
        ];
        for (settings, camera_index, camera_rotate, camera) in cases {
            let inputs = DetectorInputs { settings, camera_index, camera_rotate, ..inputs() };
            assert_eq!(parts(&inputs, "squat", 12, 0.0), (
                "workout".into(), "reps_vision".into(), None,
                json!({"activity": "lift", "exercise": squat_definition(), "movementId": "squat",
                    "targetReps": 12, "camera": camera, "appId": "reps"}),
                None,
            ));
        }
    }

    #[test]
    fn consensus_with_a_movement_asks_for_webcam_and_phone_to_agree() {
        // (REPS_PHONE_URL_FILE contents, phone source sent)
        let cases = [
            (None, "rtsp://phone.local/live"),
            (Some("rtsps://user:secret@phone.local/live"), "rtsps://user:secret@phone.local/live"),
        ];
        for (phone_url_override, phone_source) in cases {
            let calibration = json!({"webcam": {"scale": 1.5}});
            let inputs = DetectorInputs {
                settings: saved(true),
                phone_url_override: phone_url_override.map(String::from),
                calibration: calibration.clone(),
                camera_index: 3,
                camera_rotate: Some(0),
                ..inputs()
            };
            let registry = vec![
                json!({"cameraId": "webcam", "label": "USB webcam", "kind": "usb", "transport": "direct", "source": "/dev/video2", "rotate": 90}),
                json!({"cameraId": "phone", "label": "Android phone", "kind": "rtsp", "transport": "direct", "source": phone_source, "rotate": 270}),
            ];
            let cameras = vec!["webcam".to_string(), "phone".to_string()];
            // No `camera`, and no legacy `exercise` fallback: the hub must hold an active version.
            assert_eq!(parts(&inputs, "squat", 12, 0.0), (
                "workout".into(), "reps_vision".into(), Some(cameras.clone()),
                json!({"activity": "lift", "movementId": "squat", "targetReps": 12,
                    "cameraCalibration": calibration, "appId": "reps",
                    "fusion": {"policy": "consensus", "quorum": 2}}),
                Some(CameraSet { registry, cameras, overlays: vec![] }),
            ));
        }
    }

    #[test]
    fn a_phone_url_file_that_yields_no_stream_url_is_honor_mode() {
        for contents in ["", "ftp://phone.local/live"] {
            let consensus = DetectorInputs { settings: saved(true), phone_url_override: Some(contents.into()), ..inputs() };
            assert_eq!(detector_request(&consensus, "squat", 12, 0.0).unwrap_err(), HonorReason::PhoneUrlInvalid);
            // Only consensus reads the phone URL.
            let single = DetectorInputs { settings: saved(false), phone_url_override: Some(contents.into()), ..inputs() };
            assert!(detector_request(&single, "squat", 12, 0.0).is_ok());
        }
    }

    #[test]
    fn timed_exercises_carry_seconds_and_never_use_consensus() {
        let camera = json!({"source": "index", "value": 0, "id": "webcam", "rotate": 180});
        let saved_camera = json!({"source": "uri", "value": "/dev/video2", "id": "webcam", "rotate": 90});
        // (settings, target seconds) -> (targetSeconds sent, camera)
        let cases = [
            (Ok(None), 90.0, 90.0, &camera),
            // No target: the shipped default stands.
            (Ok(None), 0.0, 60.0, &camera),
            // Not a movement, so consensus settings only choose the USB camera.
            (saved(true), 90.0, 90.0, &saved_camera),
        ];
        for (settings, seconds, sent, camera) in cases {
            let inputs = DetectorInputs { settings, ..inputs() };
            let mut config = inputs.specs["exercises"]["jumprope"].clone();
            config["targetSeconds"] = json!(sent);
            config["camera"] = camera.clone();
            config["appId"] = json!("reps");
            assert_eq!(parts(&inputs, "jumprope", 0, seconds), ("workout".into(), "reps_vision".into(), None, config, None));
        }
    }

    #[test]
    fn custom_repetition_movement_requires_an_active_hub_version() {
        // Not in the shipped specs: a Studio movement id, sent without an `exercise` fallback.
        assert_eq!(parts(&inputs(), "my-curl", 8, 0.0), (
            "workout".into(), "reps_vision".into(), None,
            json!({"activity": "lift", "movementId": "my-curl", "targetReps": 8,
                "camera": {"source": "index", "value": 0, "id": "webcam", "rotate": 180}, "appId": "reps"}),
            None,
        ));
    }

    #[test]
    fn honor_reasons() {
        let broken = || Err::<Option<CameraSettings>, _>("expected value at line 1 column 1".to_string());
        // (settings, exercise, reps, seconds) -> reason
        let cases = [
            // Unknown and timed, or blank: nothing to run.
            (Ok(None), "wallsit", 0, 30.0, HonorReason::NoSpec("wallsit".into())),
            (Ok(None), "", 8, 0.0, HonorReason::NoSpec("".into())),
            // The missing spec is reported ahead of a broken cameras.json.
            (broken(), "wallsit", 0, 30.0, HonorReason::NoSpec("wallsit".into())),
            (broken(), "squat", 12, 0.0, HonorReason::Settings("expected value at line 1 column 1".into())),
        ];
        for (settings, exercise, reps, seconds, reason) in cases {
            let inputs = DetectorInputs { settings, ..inputs() };
            assert_eq!(detector_request(&inputs, exercise, reps, seconds).unwrap_err(), reason);
        }
    }

    #[test]
    fn camera_set_declared_in_specs_overrides_single_camera_config() {
        let block = |set: Value| json!({
            "model": {"plugin": "reps_vision"},
            "cameras": {
                "registry": [
                    {"cameraId": "front", "kind": "usb", "source": "v4l2:///dev/video0"},
                    {"cameraId": "side", "kind": "usb", "source": "v4l2:///dev/video1"}
                ],
                "set": set,
                "overlays": {"side": {"exercise": {"downBelow": 95.0}}, "back": {}}
            },
            "exercises": {"squat": {"activity": "lift", "exercise": {"name": "squat"}}}
        });
        let declared = DetectorInputs { specs: block(json!(["front", "side"])), ..inputs() };
        let (request, set) = detector_request(&declared, "squat", 5, 0.0).unwrap();
        let set = set.unwrap();
        assert_eq!(request.cameras, Some(vec!["front".to_string(), "side".to_string()]));
        assert_eq!(set.registry, declared.specs["cameras"]["registry"].as_array().unwrap().clone());
        assert_eq!(set.overlays, vec![("side".to_string(), json!({"exercise": {"downBelow": 95.0}}))]);
        // exclusive with the injected single-camera object
        assert_eq!(request.config, json!({"activity": "lift", "exercise": {"name": "squat"}, "movementId": "squat",
            "targetReps": 5, "appId": "reps", "fusion": {"policy": "best", "scoreField": "visibility"}}));

        // A set needs two cameras; otherwise the single camera stays.
        let lone = DetectorInputs { specs: block(json!(["front"])), ..inputs() };
        let (request, set) = detector_request(&lone, "squat", 5, 0.0).unwrap();
        assert_eq!((request.cameras, set), (None, None));
        assert_eq!(request.config["camera"]["id"], "webcam");
    }

    #[test]
    fn load_inputs_reads_the_data_directory() {
        let dir = std::env::temp_dir().join(format!("reps-detector-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Empty directory: no settings, empty calibration.
        let inputs = load_inputs(&dir).unwrap();
        assert_eq!((inputs.settings, inputs.calibration), (Ok(None), json!({})));
        assert_eq!(inputs.specs, load_exercise_specs().unwrap());

        // usbRotation is optional in the file and defaults to the rig's mounting.
        std::fs::write(dir.join("cameras.json"), r#"{"consensus":true,"usbDevice":"/dev/video2","phoneUrl":"rtsp://phone.local/live","phoneRotation":270}"#).unwrap();
        std::fs::write(dir.join("camera-calibration.json"), r#"{"webcam":{"scale":1.5}}"#).unwrap();
        let inputs = load_inputs(&dir).unwrap();
        assert_eq!(inputs.settings, Ok(Some(CameraSettings { usb_rotation: 180, ..saved(true).unwrap().unwrap() })));
        assert_eq!(inputs.calibration, json!({"webcam": {"scale": 1.5}}));

        std::fs::write(dir.join("cameras.json"), "invalid").unwrap();
        assert!(load_inputs(&dir).unwrap().settings.is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
