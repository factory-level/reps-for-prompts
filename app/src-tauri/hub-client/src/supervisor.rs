//! Owns the bundled hubd process: spawn, READY handshake, restart-once,
//! drop-safe kill. The lock screen must never depend on an unsupervised
//! external service.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{
    EnableMetric, HubClient, HubError, HubHealth, VisionEvent, VisionHub,
};

const READY_TIMEOUT: Duration = Duration::from_secs(60);
const RESTART_BACKOFF: Duration = Duration::from_millis(500);
/// Bounds for retrying a hub that never came up at all (see start_retrying).
const START_RETRY_BACKOFF: Duration = Duration::from_millis(500);
const START_RETRY_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// True if something is currently listening on `127.0.0.1:port`.
fn port_in_use(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
}

/// Kill only a child/process group that this supervisor created. Also close
/// descendants if the direct child has already exited.
fn terminate_owned_child(child: &mut Child) {
    #[cfg(unix)]
    {
        let group = -(child.id() as i32);
        unsafe { libc::kill(group, libc::SIGTERM); }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() { break; }
            std::thread::sleep(Duration::from_millis(50));
        }
        unsafe { libc::kill(group, libc::SIGKILL); }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Covers every error between spawn and handing ownership to Active.
struct StartingChild(Option<Child>);
impl Drop for StartingChild {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() { terminate_owned_child(child); }
    }
}

#[derive(Clone)]
pub struct HubSupervisorConfig {
    /// Directory of the usb-mcp-hub checkout or bundle.
    pub hub_dir: std::path::PathBuf,
    /// Command + args to launch hubd (e.g. ["pnpm", "--filter", "@hub/hubd", "start"]).
    pub command: Vec<String>,
    /// Extra environment (e.g. HUB_PLUGIN_ARGS for the reps plugin).
    pub env: Vec<(String, String)>,
}

impl HubSupervisorConfig {
    /// Production: the staged bundle under the app's resources dir
    /// (hub-bundle/hubd.mjs + public/ + vision/), reps plugin loaded from
    /// `plugin_src` (the shipped reps_vision sources). `app_home` is the caller's
    /// data directory; the provisioned Python env lives at `<app_home>/vision-env`.
    pub fn bundled(resources_dir: &std::path::Path, plugin_src: &std::path::Path, app_home: &std::path::Path) -> Self {
        let bundle = resources_dir.join("hub-bundle");
        let python_env = std::env::var("UV_PROJECT_ENVIRONMENT").unwrap_or_else(|_| app_home.join("vision-env").display().to_string());
        HubSupervisorConfig {
            hub_dir: bundle.clone(),
            command: vec![
                "node".into(),
                resources_dir.join("boot-hub.mjs").display().to_string(),
            ],
            env: vec![
                ("UV_PROJECT_ENVIRONMENT".into(), python_env),
                ("UV_FROZEN".into(), "1".into()),
                ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
                ("REPS_POSE_MODEL".into(), resources_dir.join("models").join("pose_landmarker_full.task").display().to_string()),
                (
                    "HUB_VISION_DIR".into(),
                    bundle.join("vision").display().to_string(),
                ),
                (
                    "HUB_PUBLIC_DIR".into(),
                    bundle.join("public").display().to_string(),
                ),
                (
                    "HUB_PLUGIN_ARGS_JSON".into(),
                    serde_json::json!(["--plugin-path", plugin_src.display().to_string(), "--plugin",
                        "reps_vision.hub_plugin.plugin:RepsVisionPlugin"]).to_string(),
                ),
                // Companion screens staged beside the bundle (hubd ignores the
                // env when the directory is absent).
                (
                    "HUB_APP_UI_DIR".into(),
                    resources_dir.join("companion").display().to_string(),
                ),
            ],
        }
    }

    /// Dev default: sibling checkout via $HUB_DIR, reps plugin loaded from
    /// this repo's vision/src.
    pub fn dev(repo_root: &std::path::Path) -> Self {
        let hub_dir = std::env::var("HUB_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| repo_root.join("..").join("usb-mcp-hub"));
        let plugin_path = repo_root.join("vision").join("src");
        HubSupervisorConfig {
            hub_dir,
            command: vec![
                "pnpm".into(),
                "--filter".into(),
                "@hub/hubd".into(),
                "start".into(),
            ],
            env: vec![
                (
                    "HUB_PLUGIN_ARGS_JSON".into(),
                    serde_json::json!(["--plugin-path", plugin_path.display().to_string(), "--plugin",
                        "reps_vision.hub_plugin.plugin:RepsVisionPlugin"]).to_string(),
                ),
                // Companion screens (Workout / Calibrate / History) ship with
                // reps; hubd hosts them under /app/.
                (
                    "HUB_APP_UI_DIR".into(),
                    repo_root.join("companion").display().to_string(),
                ),
            ],
        }
    }
}

struct Active {
    child: Child,
    client: HubClient,
}

pub struct HubSupervisor {
    config: HubSupervisorConfig,
    active: Arc<Mutex<Option<Active>>>,
    stopping: Arc<AtomicBool>,
    dead: Arc<AtomicBool>,
    event_tx: mpsc::Sender<VisionEvent>,
    events_rx: Option<mpsc::Receiver<VisionEvent>>,
    restarts: Arc<Mutex<u32>>,
}

impl HubSupervisor {
    pub fn start(config: HubSupervisorConfig) -> Result<Self, HubError> {
        let (event_tx, events_rx) = mpsc::channel();
        let mut supervisor = HubSupervisor {
            config,
            active: Arc::new(Mutex::new(None)),
            stopping: Arc::new(AtomicBool::new(false)),
            dead: Arc::new(AtomicBool::new(false)),
            event_tx,
            events_rx: Some(events_rx),
            restarts: Arc::new(Mutex::new(0)),
        };
        supervisor.spawn_and_connect()?;
        Ok(supervisor)
    }

    /// Like `start`, but keeps trying after a failure that is usually
    /// transient: a leaked hubd still holding the port, or a previous instance
    /// mid-shutdown. A single failed attempt used to leave the desktop in honor
    /// mode for the life of the process even once the port was free again.
    ///
    /// Backs off from `START_RETRY_BACKOFF` to `START_RETRY_BACKOFF_MAX`,
    /// reporting every failure through `on_error`. Returns `None` as soon as
    /// `stopping` is true, including partway through a backoff, so quitting
    /// never waits out the delay.
    pub fn start_retrying(
        config: HubSupervisorConfig,
        stopping: impl Fn() -> bool,
        mut on_error: impl FnMut(&HubError, Duration),
    ) -> Option<Self> {
        let mut delay = START_RETRY_BACKOFF;
        loop {
            if stopping() {
                return None;
            }
            match Self::start(config.clone()) {
                Ok(supervisor) => return Some(supervisor),
                Err(error) => {
                    on_error(&error, delay);
                    let deadline = Instant::now() + delay;
                    while Instant::now() < deadline {
                        if stopping() {
                            return None;
                        }
                        std::thread::sleep(Duration::from_millis(100).min(delay));
                    }
                    delay = (delay * 2).min(START_RETRY_BACKOFF_MAX);
                }
            }
        }
    }

    fn spawn_and_connect(&mut self) -> Result<(), HubError> {
        let (program, args) = self
            .config
            .command
            .split_first()
            .ok_or_else(|| HubError::Api("empty hub command".into()))?;
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(&self.config.hub_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (key, value) in &self.config.env {
            command.env(key, value);
        }
        let configured_port = self.config.env.iter().rev().find(|(key, _)| key == "PORT")
            .map(|(_, value)| value.clone()).or_else(|| std::env::var("PORT").ok())
            .unwrap_or_else(|| "8443".into());
        let port: u16 = configured_port.parse().map_err(|_| HubError::Api("invalid hub PORT".into()))?;
        if port != 0 && port_in_use(port) {
            return Err(HubError::Io(format!("hub port {port} is in use; refusing to terminate another process")));
        }
        // A pipe is an ownership signal: even an abrupt desktop exit closes it.
        command.stdin(Stdio::piped()).env("HUB_EXIT_ON_STDIN_CLOSE", "1");
        // New process group so drop can signal hubd (and, via its own
        // shutdown handler, vision-host) without touching our group.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                command.pre_exec(|| {
                    libc::setpgid(0, 0);
                    Ok(())
                });
            }
        }
        let mut child = StartingChild(Some(command.spawn().map_err(|e| HubError::Io(e.to_string()))?));

        let stdout = child.0.as_mut().unwrap().stdout.take().ok_or_else(|| HubError::Io("no stdout".into()))?;
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("hubd-stdout".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if let Some(json) = line.strip_prefix("HUBD READY ") {
                        let _ = ready_tx.send(json.to_string());
                    }
                }
            })
            .map_err(|e| HubError::Io(e.to_string()))?;

        let ready = ready_rx
            .recv_timeout(READY_TIMEOUT)
            .map_err(|_| HubError::Timeout("HUBD READY handshake".into()))?;
        let ready: serde_json::Value =
            serde_json::from_str(&ready).map_err(|e| HubError::Api(e.to_string()))?;
        let port = ready
            .get("debugPort")
            .and_then(|p| p.as_u64())
            .ok_or_else(|| HubError::Api("no debugPort in HUBD READY".into()))?;

        let mut client = HubClient::connect(&format!("ws://127.0.0.1:{port}/v1/ws"))?;

        // Forward client events into the supervisor's single stream.
        let client_rx = client
            .take_receiver()
            .ok_or_else(|| HubError::Api("client receiver already taken".into()))?;
        let forward_tx = self.event_tx.clone();
        std::thread::Builder::new()
            .name("hub-event-forward".into())
            .spawn(move || {
                for event in client_rx {
                    if forward_tx.send(event).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| HubError::Io(e.to_string()))?;

        *self.active.lock().unwrap() = Some(Active { child: child.0.take().unwrap(), client });
        self.watch();
        Ok(())
    }

    /// Monitor thread: on unexpected exit restart once, then give up.
    fn watch(&self) {
        let active = Arc::clone(&self.active);
        let stopping = Arc::clone(&self.stopping);
        let dead = Arc::clone(&self.dead);
        let restarts = Arc::clone(&self.restarts);
        let event_tx = self.event_tx.clone();
        let config_snapshot = (
            self.config.hub_dir.clone(),
            self.config.command.clone(),
            self.config.env.clone(),
        );
        let self_active = Arc::clone(&self.active);
        std::thread::Builder::new()
            .name("hubd-watch".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_millis(250));
                if stopping.load(Ordering::SeqCst) || dead.load(Ordering::SeqCst) {
                    return;
                }
                let exited = {
                    let mut guard = active.lock().unwrap();
                    match guard.as_mut() {
                        None => return,
                        Some(active_ref) => {
                            let exited = active_ref.child.try_wait().ok().flatten().is_some();
                            if exited { terminate_owned_child(&mut active_ref.child); }
                            exited
                        }
                    }
                };
                if !exited {
                    continue;
                }
                let _ = event_tx.send(VisionEvent::ConnectionLost);
                let mut count = restarts.lock().unwrap();
                if *count >= 1 {
                    dead.store(true, Ordering::SeqCst);
                    let _ = event_tx.send(VisionEvent::Health(HubHealth::failed()));
                    return;
                }
                *count += 1;
                drop(count);
                std::thread::sleep(RESTART_BACKOFF);
                let mut respawner = HubSupervisor {
                    config: HubSupervisorConfig {
                        hub_dir: config_snapshot.0.clone(),
                        command: config_snapshot.1.clone(),
                        env: config_snapshot.2.clone(),
                    },
                    active: Arc::clone(&self_active),
                    stopping: Arc::clone(&stopping),
                    dead: Arc::clone(&dead),
                    event_tx: event_tx.clone(),
                    events_rx: None,
                    restarts: Arc::clone(&restarts),
                };
                if respawner.spawn_and_connect().is_err() {
                    dead.store(true, Ordering::SeqCst);
                    let _ = event_tx.send(VisionEvent::Health(HubHealth::failed()));
                }
                // respawner shares Arcs; prevent its Drop from killing the child
                std::mem::forget(respawner);
                return;
            })
            .ok();
    }

    fn with_client<T>(
        &mut self,
        f: impl FnOnce(&mut HubClient) -> Result<T, HubError>,
    ) -> Result<T, HubError> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(HubError::Down);
        }
        let mut guard = self.active.lock().unwrap();
        match guard.as_mut() {
            Some(active) => f(&mut active.client),
            None => Err(HubError::Down),
        }
    }

    fn kill_child(&self) {
        if let Some(active) = self.active.lock().unwrap().as_mut() {
            terminate_owned_child(&mut active.child);
        }
    }

    pub fn stop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.kill_child();
    }
}

impl Drop for HubSupervisor {
    /// The camera can never stay hot past the app: kill hubd on drop.
    fn drop(&mut self) {
        self.stop();
    }
}

impl VisionHub for HubSupervisor {
    fn enable_metric(&mut self, req: &EnableMetric) -> Result<(), HubError> {
        let req = req.clone();
        self.with_client(move |client| client.enable_metric(&req))
    }

    fn disable_metric(&mut self, metric_id: &str) -> Result<(), HubError> {
        let id = metric_id.to_string();
        self.with_client(move |client| client.disable_metric(&id))
    }

    fn add_camera(&mut self, camera: &serde_json::Value) -> Result<(), HubError> {
        let camera = camera.clone();
        self.with_client(move |client| client.add_camera(&camera))
    }

    fn update_metric_config_for_camera(
        &mut self,
        metric_id: &str,
        camera_id: &str,
        config: &serde_json::Value,
    ) -> Result<(), HubError> {
        let (id, camera, config) = (metric_id.to_string(), camera_id.to_string(), config.clone());
        self.with_client(move |client| {
            client.update_metric_config_for_camera(&id, &camera, &config)
        })
    }

    fn update_metric_config(
        &mut self,
        metric_id: &str,
        config: &serde_json::Value,
    ) -> Result<(), HubError> {
        let id = metric_id.to_string();
        let config = config.clone();
        self.with_client(move |client| client.update_metric_config(&id, &config))
    }

    fn simulate(&mut self, metric_id: &str, event: &serde_json::Value) -> Result<(), HubError> {
        let id = metric_id.to_string();
        let event = event.clone();
        self.with_client(move |client| client.simulate(&id, &event))
    }

    fn health(&mut self) -> Result<HubHealth, HubError> {
        self.with_client(|client| client.health())
    }

    fn take_receiver(&mut self) -> Option<mpsc::Receiver<VisionEvent>> {
        self.events_rx.take()
    }

    fn register_application(
        &mut self,
        app_id: &str,
        version: &str,
        manifest: &serde_json::Value,
    ) -> Result<(), HubError> {
        let (app_id, version, manifest) =
            (app_id.to_string(), version.to_string(), manifest.clone());
        self.with_client(move |client| client.register_application(&app_id, &version, &manifest))
    }

    fn publish_event(&mut self, params: &serde_json::Value) -> Result<(), HubError> {
        let params = params.clone();
        self.with_client(move |client| client.publish_event(&params))
    }

    fn publish_command(&mut self, params: &serde_json::Value) -> Result<(), HubError> {
        let params = params.clone();
        self.with_client(move |client| client.publish_command(&params))
    }

    fn report_action_result(&mut self, params: &serde_json::Value) -> Result<(), HubError> {
        let params = params.clone();
        self.with_client(move |client| client.report_action_result(&params))
    }
}

#[cfg(test)]
mod port_tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn occupied_port_is_reported_without_terminating_its_owner() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let result = HubSupervisor::start(HubSupervisorConfig {
            hub_dir: dir.path().into(), command: vec!["must-not-execute".into()],
            env: vec![("PORT".into(), port.to_string())],
        });
        assert!(matches!(result, Err(HubError::Io(message)) if message.contains("refusing to terminate")));
        assert!(port_in_use(port));
    }

    #[test]
    fn a_failed_start_is_retried_instead_of_dropping_to_honor_mode_forever() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let stopping = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(Mutex::new(0u32));
        let quit = Arc::clone(&stopping);
        let seen = Arc::clone(&attempts);
        let result = HubSupervisor::start_retrying(
            HubSupervisorConfig {
                hub_dir: dir.path().into(), command: vec!["must-not-execute".into()],
                env: vec![("PORT".into(), port.to_string())],
            },
            { let stopping = Arc::clone(&stopping); move || stopping.load(Ordering::SeqCst) },
            move |_error, _delay| {
                let mut count = seen.lock().unwrap();
                *count += 1;
                // Let it prove it comes back, then release it.
                if *count >= 3 { quit.store(true, Ordering::SeqCst); }
            },
        );
        assert!(result.is_none(), "must give up only when asked to stop");
        assert_eq!(*attempts.lock().unwrap(), 3, "a failed start must be retried");
        assert!(port_in_use(port), "retrying must not kill the port's owner");
    }

    #[cfg(unix)]
    #[test]
    fn failed_handshake_reaps_owned_child_and_its_camera_process() {
        let dir = tempfile::tempdir().unwrap();
        let port_file = dir.path().join("port");
        let script = r#"import socket, subprocess, sys, time
s = socket.socket(); s.bind(('127.0.0.1', 0)); s.listen()
subprocess.Popen(['python3', '-c', 'import time; time.sleep(300)'], pass_fds=(s.fileno(),))
with open(sys.argv[1], 'w') as f: f.write(str(s.getsockname()[1]))
print('HUBD READY invalid-json', flush=True)
time.sleep(300)
"#;
        let result = HubSupervisor::start(HubSupervisorConfig {
            hub_dir: dir.path().into(),
            command: vec!["python3".into(), "-u".into(), "-c".into(), script.into(), port_file.display().to_string()],
            env: vec![("PORT".into(), "0".into())],
        });
        assert!(result.is_err());
        let port = std::fs::read_to_string(port_file).unwrap().parse().unwrap();
        let closed = (0..50).any(|_| {
            if !port_in_use(port) { return true; }
            std::thread::sleep(Duration::from_millis(20)); false
        });
        assert!(closed, "owned descendant retained its camera/listener after startup failed");
    }
}
