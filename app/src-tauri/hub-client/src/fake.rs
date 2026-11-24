//! Scripted in-memory hub for app-level tests: records calls, replays events.

use std::sync::{mpsc, Arc, Mutex};

use crate::{EnableMetric, HubError, HubHealth, VisionEvent, VisionHub};

pub struct FakeHub {
    /// Shared, so a test can keep reading it after the hub is boxed away.
    pub calls: Arc<Mutex<Vec<String>>>,
    /// Wire params of every publish_event/publish_command/report_action_result.
    pub published: Vec<serde_json::Value>,
    scripted: Vec<VisionEvent>,
    tx: mpsc::Sender<VisionEvent>,
    rx: Option<mpsc::Receiver<VisionEvent>>,
    pub fail_next: bool,
    /// Fails the next `enable_metric` only — `fail_next` would be consumed by
    /// the stale-stream disable that precedes every enable.
    pub fail_next_enable: bool,
    enabled: Vec<String>,
}

impl FakeHub {
    /// Events in `scripted` are emitted when a metric is enabled.
    pub fn new(scripted: Vec<VisionEvent>) -> Self {
        let (tx, rx) = mpsc::channel();
        FakeHub {
            calls: Arc::default(),
            published: vec![],
            scripted,
            tx,
            rx: Some(rx),
            fail_next: false,
            fail_next_enable: false,
            enabled: vec![],
        }
    }

    /// Push an extra event mid-test.
    pub fn push(&self, event: VisionEvent) {
        let _ = self.tx.send(event);
    }

    fn log(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }

    fn check_fail(&mut self) -> Result<(), HubError> {
        if self.fail_next {
            self.fail_next = false;
            return Err(HubError::Down);
        }
        Ok(())
    }
}

impl VisionHub for FakeHub {
    fn enable_metric(&mut self, req: &EnableMetric) -> Result<(), HubError> {
        if std::mem::take(&mut self.fail_next_enable) {
            return Err(HubError::Down);
        }
        self.check_fail()?;
        self.log(format!("enable:{}:{}", req.metric_id, req.plugin_id));
        self.enabled.push(req.metric_id.clone());
        for event in self.scripted.drain(..) {
            let _ = self.tx.send(event);
        }
        Ok(())
    }

    fn disable_metric(&mut self, metric_id: &str) -> Result<(), HubError> {
        self.check_fail()?;
        self.log(format!("disable:{metric_id}"));
        // Like the hub: only an enabled metric can be disabled.
        let before = self.enabled.len();
        self.enabled.retain(|enabled| enabled != metric_id);
        if self.enabled.len() == before {
            return Err(HubError::Api(format!("unknown metric: {metric_id}")));
        }
        Ok(())
    }

    fn update_metric_config(
        &mut self,
        metric_id: &str,
        _config: &serde_json::Value,
    ) -> Result<(), HubError> {
        self.log(format!("update:{metric_id}"));
        Ok(())
    }

    fn add_camera(&mut self, camera: &serde_json::Value) -> Result<(), HubError> {
        self.check_fail()?;
        let id = camera["cameraId"].as_str().unwrap_or("?");
        self.log(format!("add_camera:{id}"));
        Ok(())
    }

    fn update_metric_config_for_camera(
        &mut self,
        metric_id: &str,
        camera_id: &str,
        _config: &serde_json::Value,
    ) -> Result<(), HubError> {
        self.check_fail()?;
        self.log(format!("update:{metric_id}:{camera_id}"));
        Ok(())
    }

    fn simulate(&mut self, metric_id: &str, event: &serde_json::Value) -> Result<(), HubError> {
        self.log(format!("simulate:{metric_id}"));
        if let Some(converted) = crate::event_from_frame(&serde_json::json!({
            "stream": "event",
            "data": event,
        })) {
            let _ = self.tx.send(converted);
        }
        Ok(())
    }

    fn health(&mut self) -> Result<HubHealth, HubError> {
        Ok(HubHealth {
            vision_host: "up".into(),
            camera: "closed".into(),
            enabled_metrics: vec![],
        })
    }

    fn take_receiver(&mut self) -> Option<mpsc::Receiver<VisionEvent>> {
        self.rx.take()
    }

    fn register_application(
        &mut self,
        app_id: &str,
        _version: &str,
        _manifest: &serde_json::Value,
    ) -> Result<(), HubError> {
        self.check_fail()?;
        self.log(format!("register_application:{app_id}"));
        Ok(())
    }

    fn publish_event(&mut self, params: &serde_json::Value) -> Result<(), HubError> {
        self.check_fail()?;
        let event_type = params["type"].as_str().unwrap_or("?");
        self.log(format!("publish_event:{event_type}"));
        self.published.push(params.clone());
        Ok(())
    }

    fn publish_command(&mut self, params: &serde_json::Value) -> Result<(), HubError> {
        self.check_fail()?;
        let command_type = params["type"].as_str().unwrap_or("?");
        self.log(format!("publish_command:{command_type}"));
        self.published.push(params.clone());
        Ok(())
    }

    fn report_action_result(&mut self, params: &serde_json::Value) -> Result<(), HubError> {
        self.check_fail()?;
        let result_type = params["type"].as_str().unwrap_or("?");
        self.log(format!("report_action_result:{result_type}"));
        self.published.push(params.clone());
        Ok(())
    }
}
