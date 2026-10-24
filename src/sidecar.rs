use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Rotate sidecar.log once it grows past this size, keeping one previous file.
const LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;
/// How long the sidecar may take to print its `PORT=` line.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
const HEALTH_EVERY: Duration = Duration::from_secs(2);
/// A sidecar that stays up this long earns a fresh restart budget.
const STABLE_AFTER: Duration = Duration::from_secs(300);
const MAX_RESTARTS: u32 = 3;

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
        stop_child(&self.child);
    }
}

// ---------------------------------------------------------------- logging

fn log_file_path() -> PathBuf {
    crate::downloader::data_dir()
        .join("logs")
        .join("sidecar.log")
}

struct LogSink {
    file: Option<File>,
    written: u64,
}

impl LogSink {
    fn open() -> Self {
        let path = log_file_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        if file.is_none() {
            eprintln!("[ytapi] cannot open {}", path.display());
        }
        let written = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Self { file, written }
    }

    fn write_line(&mut self, stream: &str, line: &str) {
        if self.written > LOG_MAX_BYTES {
            self.rotate();
        }
        let text = format!("{stream} {line}\n");
        match self.file.as_mut() {
            Some(file) => {
                if file.write_all(text.as_bytes()).is_ok() {
                    self.written += text.len() as u64;
                }
            }
            None => eprint!("{text}"),
        }
    }

    fn rotate(&mut self) {
        // Close first: Windows refuses to rename a file that is still open.
        self.file = None;
        let path = log_file_path();
        let rotated = path.with_file_name("sidecar.log.1");
        let _ = std::fs::remove_file(&rotated);
        let _ = std::fs::rename(&path, &rotated);
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        self.written = 0;
    }
}

fn log_line(stream: &str, line: &str) {
    static SINK: OnceLock<Mutex<LogSink>> = OnceLock::new();
    let sink = SINK.get_or_init(|| Mutex::new(LogSink::open()));
    if let Ok(mut sink) = sink.lock() {
        sink.write_line(stream, line);
    }
}

// ------------------------------------------------------------ child process

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn python_executable() -> PathBuf {
    // Prefer the project venv directly: we own the process and can kill it
    // (spawning via `uv run` makes python a grandchild that survives kills).
    #[cfg(target_os = "windows")]
    let venv_python = project_root()
        .join("api")
        .join(".venv")
        .join("Scripts")
        .join("python.exe");
    #[cfg(not(target_os = "windows"))]
    let venv_python = project_root()
        .join("api")
        .join(".venv")
        .join("bin")
        .join("python");
    if venv_python.exists() {
        venv_python
    } else {
        PathBuf::from("python")
    }
}

/// Spawns the sidecar and waits for its `PORT=<n>` handshake on stdout.
/// `port` is only set when restarting: re-using the first port keeps the app's
/// single gRPC channel valid across restarts.
fn spawn_child(token: &str, port: Option<u16>) -> Result<(Child, u16), String> {
    let mut cmd = Command::new(python_executable());
    cmd.args(["-m", "ytapi.server"])
        .env("FASTTUBE_API_TOKEN", token)
        // Ask the sidecar to exit when this process dies (stdin is its
        // liveness pipe). Off by default so manual runs work normally.
        .env("FASTTUBE_PARENT_PIPE", "1")
        // Piped stdio otherwise uses the console code page: keep it UTF-8 so
        // the handshake parses and the log stays readable.
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUNBUFFERED", "1")
        .current_dir(project_root())
        // stdin is a liveness pipe: the sidecar exits when we do (see
        // _watch_parent_pipe in server.py), so a crash cannot orphan it.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(port) = port {
        cmd.env("YTAPI_PORT", port.to_string());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn sidecar: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "sidecar stdout is not piped".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "sidecar stderr is not piped".to_string())?;

    let (tx, rx) = std::sync::mpsc::channel::<Option<u16>>();

    std::thread::Builder::new()
        .name("ytapi-stdout".to_string())
        .spawn(move || {
            let mut handshake = Some(tx);
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(tx) = handshake.take() {
                    let port = line
                        .strip_prefix("PORT=")
                        .and_then(|p| p.trim().parse::<u16>().ok());
                    let _ = tx.send(port);
                }
                log_line("[out]", &line);
            }
            if let Some(tx) = handshake.take() {
                let _ = tx.send(None); // EOF before the handshake
            }
        })
        .map_err(|e| format!("failed to start sidecar stdout reader: {e}"))?;

    std::thread::Builder::new()
        .name("ytapi-stderr".to_string())
        .spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                log_line("[err]", &line);
            }
        })
        .map_err(|e| format!("failed to start sidecar stderr reader: {e}"))?;

    match rx.recv_timeout(HANDSHAKE_TIMEOUT) {
        Ok(Some(port)) => Ok((child, port)),
        Ok(None) => {
            kill_child(&mut child);
            Err("sidecar exited before reporting its port; see logs/sidecar.log".to_string())
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            kill_child(&mut child);
            Err(format!(
                "sidecar did not report a port within {}s; see logs/sidecar.log",
                HANDSHAKE_TIMEOUT.as_secs()
            ))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            kill_child(&mut child);
            Err("sidecar handshake channel closed".to_string())
        }
    }
}

fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn stop_child(slot: &Arc<Mutex<Option<Child>>>) {
    if let Some(mut child) = slot.lock().unwrap().take() {
        kill_child(&mut child);
    }
}

fn child_exited(slot: &Arc<Mutex<Option<Child>>>) -> bool {
    let mut guard = slot.lock().unwrap();
    match guard.as_mut() {
        Some(child) => child.try_wait().map(|s| s.is_some()).unwrap_or(true),
        None => true,
    }
}

// ---------------------------------------------------------------- supervisor

/// Spawns the Python sidecar and supervises it: waits for the port handshake,
/// health-checks over gRPC, restarts on crash with exponential backoff.
/// A leftover sidecar from a previous run is never reused: every launch makes
/// a fresh token, so an old process could not authenticate this app anyway.
pub fn spawn_and_supervise(
    status: SharedApiStatus,
    shutdown: Arc<AtomicBool>,
    token: String,
) -> Sidecar {
    let sidecar = Sidecar::default();
    let slot = sidecar.child.clone();
    std::thread::Builder::new()
        .name("ytapi-supervisor".to_string())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime for sidecar supervisor");
            rt.block_on(supervise_loop(status, shutdown, slot, token));
        })
        .expect("failed to spawn ytapi supervisor thread");
    sidecar
}

/// 2s, 4s, 8s, 16s, 32s capped at 30s.
fn backoff(attempt: u32) -> Duration {
    let secs = 2u64.saturating_pow(attempt.min(5)).min(30);
    Duration::from_secs(secs)
}

async fn supervise_loop(
    status: SharedApiStatus,
    shutdown: Arc<AtomicBool>,
    slot: Arc<Mutex<Option<Child>>>,
    token: String,
) {
    let mut restarts: u32 = 0;
    let mut pinned_port: Option<u16> = None;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            stop_child(&slot);
            eprintln!("[ytapi] shutdown");
            return;
        }

        let (child, port) = match spawn_child(&token, pinned_port) {
            Ok(started) => started,
            Err(e) => {
                eprintln!("[ytapi] {e}");
                restarts += 1;
                if restarts > MAX_RESTARTS {
                    eprintln!("[ytapi] giving up after {MAX_RESTARTS} restarts");
                    set_status(&status, "down", "", false);
                    return;
                }
                let delay = backoff(restarts);
                set_status(&status, "starting", "", false);
                eprintln!(
                    "[ytapi] retry {restarts}/{MAX_RESTARTS} in {}s",
                    delay.as_secs()
                );
                tokio::time::sleep(delay).await;
                continue;
            }
        };

        pinned_port = Some(port);
        if let Err(e) = crate::grpc::set_endpoint(port, token.clone()) {
            eprintln!("[ytapi] invalid sidecar endpoint: {e}");
        }
        let started = Instant::now();
        *slot.lock().unwrap() = Some(child);
        eprintln!("[ytapi] sidecar listening on 127.0.0.1:{port}");

        loop {
            tokio::time::sleep(HEALTH_EVERY).await;
            if shutdown.load(Ordering::SeqCst) {
                stop_child(&slot);
                eprintln!("[ytapi] shutdown");
                return;
            }
            if child_exited(&slot) {
                break;
            }

            match tokio::time::timeout(Duration::from_secs(3), crate::grpc::health()).await {
                Ok(Ok(h)) => {
                    let prev = status.read().map(|s| s.state.clone()).unwrap_or_default();
                    set_status(&status, "ok", &h.version, h.auth_ok);
                    if prev != "ok" {
                        eprintln!("[ytapi] health ok, version {}", h.version);
                    }
                }
                _ => set_status(&status, "starting", "", false),
            }

            if restarts > 0 && started.elapsed() >= STABLE_AFTER {
                restarts = 0;
                eprintln!(
                    "[ytapi] sidecar stable for {}s, restart counter reset",
                    STABLE_AFTER.as_secs()
                );
            }
        }

        restarts += 1;
        if restarts > MAX_RESTARTS {
            eprintln!("[ytapi] sidecar died, giving up after {MAX_RESTARTS} restarts");
            set_status(&status, "down", "", false);
            return;
        }
        let delay = backoff(restarts);
        set_status(&status, "starting", "", false);
        eprintln!(
            "[ytapi] sidecar died, restart {restarts}/{MAX_RESTARTS} in {}s",
            delay.as_secs()
        );
        tokio::time::sleep(delay).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(2), Duration::from_secs(4));
        assert_eq!(backoff(3), Duration::from_secs(8));
        assert_eq!(backoff(4), Duration::from_secs(16));
        assert_eq!(backoff(9), Duration::from_secs(30));
    }
}
