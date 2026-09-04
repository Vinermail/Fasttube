use serde::Serialize;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[derive(Serialize, Clone, Default)]
pub struct ApiStatus {
    pub state: String, // starting | ok | down
    pub version: String,
    pub auth_ok: bool,
}

pub type SharedApiStatus = Arc<RwLock<ApiStatus>>;

/// Handle to the spawned sidecar process, shared with the supervisor thread.
/// Allows synchronous kill-on-exit from the main thread.
#[derive(Clone, Default)]
pub struct Sidecar {
    child: Arc<Mutex<Option<Child>>>,
}

impl Sidecar {
    pub fn kill(&self) {
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn python_executable() -> PathBuf {
    // Prefer the project venv directly: we own the process and can kill it
    // (spawning via `uv run` makes python a grandchild that survives kills).
    let venv_python = project_root()
        .join("api")
        .join(".venv")
        .join("Scripts")
        .join("python.exe");
    if venv_python.exists() {
        venv_python
    } else {
        PathBuf::from("python")
    }
}

fn spawn_child() -> std::io::Result<Child> {
    let mut cmd = Command::new(python_executable());
    cmd.args(["-m", "ytapi.server"])
        .current_dir(project_root())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn()
}

/// Spawns the Python sidecar and supervises it: health-checks over gRPC,
/// restarts on crash, publishes status into `SharedApiStatus`.
/// If a healthy sidecar is already running (e.g. started manually or left
/// over from a previous run), it is reused instead of spawning a new one.
/// When `shutdown` is set, the spawned child is killed.
pub fn spawn_and_supervise(status: SharedApiStatus, shutdown: Arc<AtomicBool>) -> Sidecar {
    let sidecar = Sidecar::default();
    let slot = sidecar.child.clone();
    std::thread::Builder::new()
        .name("ytapi-supervisor".to_string())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime for sidecar supervisor");
            rt.block_on(supervise_loop(status, shutdown, slot));
        })
        .expect("failed to spawn ytapi supervisor thread");
    sidecar
}

async fn supervise_loop(
    status: SharedApiStatus,
    shutdown: Arc<AtomicBool>,
    slot: Arc<Mutex<Option<Child>>>,
) {
    if crate::grpc::health().await.is_ok() {
        eprintln!("[ytapi] reusing healthy sidecar");
        monitor_external(status, shutdown).await;
        return;
    }

    let mut child = match spawn_child() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("[ytapi] failed to spawn sidecar: {e}");
            set_status(&status, "down", "", false);
            return;
        }
    };
    *slot.lock().unwrap() = child.take();
    let mut restarts = 0u32;
    const MAX_RESTARTS: u32 = 3;

    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if shutdown.load(Ordering::SeqCst) {
            if let Some(mut c) = slot.lock().unwrap().take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            eprintln!("[ytapi] shutdown");
            return;
        }

        let mut alive = true;
        if let Some(c) = slot.lock().unwrap().as_mut()
            && c.try_wait().ok().flatten().is_some()
        {
            alive = false;
        }

        if !alive {
            if restarts >= MAX_RESTARTS {
                eprintln!("[ytapi] sidecar died, giving up after {restarts} restarts");
                set_status(&status, "down", "", false);
                return;
            }
            restarts += 1;
            eprintln!("[ytapi] sidecar died, restarting ({restarts}/{MAX_RESTARTS})");
            let respawned = match spawn_child() {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("[ytapi] respawn failed: {e}");
                    set_status(&status, "down", "", false);
                    return;
                }
            };
            *slot.lock().unwrap() = respawned;
            continue;
        }

        match tokio::time::timeout(Duration::from_secs(3), crate::grpc::health()).await {
            Ok(Ok(h)) => {
                let prev_state = status.read().map(|s| s.state.clone()).unwrap_or_default();
                set_status(&status, "ok", &h.version, h.auth_ok);
                if prev_state != "ok" {
                    eprintln!("[ytapi] health ok, version {}", h.version);
                }
            }
            _ => set_status(&status, "starting", "", false),
        }
    }
}

/// Monitors an externally managed sidecar without owning its process.
async fn monitor_external(status: SharedApiStatus, shutdown: Arc<AtomicBool>) {
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        match tokio::time::timeout(Duration::from_secs(3), crate::grpc::health()).await {
            Ok(Ok(h)) => set_status(&status, "ok", &h.version, h.auth_ok),
            _ => set_status(&status, "starting", "", false),
        }
    }
}

fn set_status(shared: &SharedApiStatus, state: &str, version: &str, auth_ok: bool) {
    if let Ok(mut s) = shared.write()
        && (s.state != state || s.version != version || s.auth_ok != auth_ok)
    {
        s.state = state.to_string();
        s.version = version.to_string();
        s.auth_ok = auth_ok;
    }
}
