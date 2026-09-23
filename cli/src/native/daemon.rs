use serde_json::Value;
use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::signal;
use tokio::sync::{Notify, RwLock};

use super::actions::{
    auto_save_restore_state, close_all_browser_backends, close_current_browser, execute_command,
    maybe_autosave_restore_state, DaemonState,
};
use super::cdp::client::CdpClient;
use super::state;
use super::stream::{IdleActivity, StreamServer};
use crate::connection::INTERNAL_DAEMON_SHUTDOWN_ACTION;

pub async fn run_daemon(session: &str) {
    let socket_dir = get_daemon_socket_dir();
    if !socket_dir.exists() {
        let _ = fs::create_dir_all(&socket_dir);
    }

    // When debug mode is on, redirect stderr to a log file so daemon
    // output can be inspected (the daemon normally has stderr piped to its
    // parent which drops the read end after startup).
    #[cfg(unix)]
    if env::var("AGENT_BROWSER_DEBUG").is_ok() {
        let log_path = socket_dir.join(format!("{}.log", session));
        if let Ok(file) = fs::File::create(&log_path) {
            use std::os::unix::io::IntoRawFd;
            let fd = file.into_raw_fd();
            unsafe {
                libc::dup2(fd, 2);
                libc::close(fd);
            }
            let _ = writeln!(
                std::io::stderr(),
                "[daemon] Debug logging started for session: {}",
                session
            );
        }
    } else {
        // Redirect stderr to /dev/null to prevent daemon crash when the
        // parent CLI drops the piped stderr handle after startup.  Cloud
        // providers (AgentCore, Browserbase, etc.) may write to stderr
        // during connection setup; a broken pipe would kill the daemon.
        #[cfg(unix)]
        {
            use std::os::unix::io::IntoRawFd;
            if let Ok(devnull) = fs::File::create("/dev/null") {
                let fd = devnull.into_raw_fd();
                unsafe {
                    libc::dup2(fd, 2);
                    libc::close(fd);
                }
            }
        }
    }

    let pid_path = socket_dir.join(format!("{}.pid", session));
    let _ = fs::write(&pid_path, process::id().to_string());

    let version_path = socket_dir.join(format!("{}.version", session));
    let _ = fs::write(&version_path, env!("CARGO_PKG_VERSION"));

    // On Unix the daemon listens on a Unix domain socket; on Windows it uses
    // TCP, so there is no .sock file — only a .port file written by the server.
    let socket_path = socket_dir.join(format!("{}.sock", session));

    #[cfg(unix)]
    if socket_path.exists() {
        let _ = fs::remove_file(&socket_path);
    }

    #[cfg(windows)]
    {
        let _ = fs::remove_file(socket_dir.join(format!("{}.port", session)));
    }

    let stream_path = socket_dir.join(format!("{}.stream", session));
    let _ = fs::remove_file(&stream_path);
    let _ = fs::remove_file(socket_dir.join(format!("{}.engine", session)));
    let _ = fs::remove_file(socket_dir.join(format!("{}.provider", session)));
    let _ = fs::remove_file(socket_dir.join(format!("{}.extensions", session)));

    if let Ok(days_str) = env::var("AGENT_BROWSER_STATE_EXPIRE_DAYS") {
        if let Ok(days) = days_str.parse::<u64>() {
            if days > 0 {
                let _ = state::state_clean(days);
            }
        }
    }

    let mut stream_client: Option<Arc<RwLock<Option<Arc<CdpClient>>>>> = None;
    let mut stream_server_instance: Option<Arc<StreamServer>> = None;
    let idle_activity = Arc::new(IdleActivity::new());
    let preferred_port = env::var("AGENT_BROWSER_STREAM_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    match StreamServer::start_without_client(
        preferred_port,
        session.to_string(),
        true,
        idle_activity.clone(),
    )
    .await
    {
        Ok((stream_server, client_slot)) => {
            stream_client = Some(client_slot.clone());
            if let Err(e) = fs::write(&stream_path, stream_server.port().to_string()) {
                let _ = writeln!(std::io::stderr(), "Failed to write .stream file: {}", e);
            }
            stream_server_instance = Some(Arc::new(stream_server));
        }
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "Stream server failed to start: {}", e);
        }
    }

    // Auto-shutdown the daemon after this many ms of inactivity (no commands
    // or dashboard input received). Applies a default when
    // AGENT_BROWSER_IDLE_TIMEOUT_MS is unset; an explicit 0 disables idle
    // shutdown entirely.
    let idle_timeout = resolve_idle_timeout(env::var("AGENT_BROWSER_IDLE_TIMEOUT_MS").ok());

    let autosave_interval_ms = autosave_interval_ms_from_env();

    let result = run_socket_server(
        &socket_path,
        session,
        stream_client,
        stream_server_instance,
        idle_activity,
        idle_timeout,
        autosave_interval_ms,
    )
    .await;

    #[cfg(unix)]
    {
        let _ = fs::remove_file(&socket_path);
    }
    #[cfg(windows)]
    {
        let _ = fs::remove_file(socket_dir.join(format!("{}.port", session)));
    }
    let _ = fs::remove_file(&pid_path);
    let _ = fs::remove_file(&version_path);
    let _ = fs::remove_file(&stream_path);
    let _ = fs::remove_file(socket_dir.join(format!("{}.engine", session)));
    let _ = fs::remove_file(socket_dir.join(format!("{}.provider", session)));
    let _ = fs::remove_file(socket_dir.join(format!("{}.extensions", session)));

    if let Err(e) = result {
        let _ = writeln!(std::io::stderr(), "Daemon error: {}", e);
        process::exit(1);
    }
}

/// Idle timeout applied when AGENT_BROWSER_IDLE_TIMEOUT_MS is unset, so an
/// integration that dies without calling `close` cannot leak the daemon and
/// its Chrome tree indefinitely (issue: leaked daemons observed running for
/// days). Socket commands and dashboard input reset the timer. Unlike an
/// explicit timeout, the default never closes a headed browser (including
/// Safari and iOS WebDriver sessions) or a user-attached browser because those
/// may be in direct human use that the daemon cannot observe. Provider-owned
/// CDP browsers remain eligible for cleanup.
pub const DEFAULT_IDLE_TIMEOUT_MS: u64 = 60 * 60 * 1000;

#[derive(Clone, Copy)]
struct IdleTimeout {
    ms: u64,
    /// True when the value came from DEFAULT_IDLE_TIMEOUT_MS rather than an
    /// explicit AGENT_BROWSER_IDLE_TIMEOUT_MS. Only the default exempts
    /// headed and user-attached browsers from shutdown.
    is_default: bool,
}

/// Resolve AGENT_BROWSER_IDLE_TIMEOUT_MS into an effective idle timeout:
/// unset or unparseable → the default; explicit 0 → disabled (None);
/// any other value → that many milliseconds.
fn resolve_idle_timeout(raw: Option<String>) -> Option<IdleTimeout> {
    match raw.as_deref().map(str::trim).map(str::parse::<u64>) {
        Some(Ok(0)) => None,
        Some(Ok(ms)) => Some(IdleTimeout {
            ms,
            is_default: false,
        }),
        // Unparseable values are validated (with a warning) at the flags
        // layer; falling back to the default here keeps the leak backstop
        // in place rather than silently disabling it.
        Some(Err(_)) | None => Some(IdleTimeout {
            ms: DEFAULT_IDLE_TIMEOUT_MS,
            is_default: true,
        }),
    }
}

fn remaining_idle_timeout(activity: &IdleActivity, timeout_ms: u64) -> Option<Duration> {
    Duration::from_millis(timeout_ms).checked_sub(activity.elapsed())
}

/// Minimum ms between periodic session autosaves while the browser is open.
/// Defaults to 30s; 0 disables periodic autosave (save-on-close still runs).
fn autosave_interval_ms_from_env() -> u64 {
    env::var("AGENT_BROWSER_AUTOSAVE_INTERVAL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(30_000)
}

#[cfg(unix)]
async fn run_socket_server(
    socket_path: &PathBuf,
    session: &str,
    stream_client: Option<Arc<RwLock<Option<Arc<CdpClient>>>>>,
    stream_server: Option<Arc<StreamServer>>,
    idle_activity: Arc<IdleActivity>,
    idle_timeout: Option<IdleTimeout>,
    autosave_interval_ms: u64,
) -> Result<(), String> {
    use tokio::net::UnixListener;

    let idle_timeout_ms = idle_timeout.map(|t| t.ms);

    let listener =
        UnixListener::bind(socket_path).map_err(|e| format!("Failed to bind socket: {}", e))?;

    let stream_file: Option<PathBuf> = if stream_server.is_some() {
        let dir = socket_path.parent().unwrap_or(std::path::Path::new("."));
        Some(dir.join(format!("{}.stream", session)))
    } else {
        None
    };
    let state: std::sync::Arc<tokio::sync::Mutex<DaemonState>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(DaemonState::new_with_stream(
            stream_client,
            stream_server,
            idle_activity.clone(),
        )));

    // Notifier used by handle_connection to signal the daemon loop to exit
    // after a "close" command, instead of calling process::exit() which skips
    // destructors and can leave Chrome processes orphaned (issue #1113).
    let close_notify = Arc::new(Notify::new());

    let mut drain_interval = tokio::time::interval(Duration::from_millis(100));
    drain_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let idle_sleep = idle_timeout_ms.map(|ms| tokio::time::sleep(Duration::from_millis(ms)));
    let mut idle_sleep_pin = idle_sleep.map(Box::pin);

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _)) => {
                        let state = state.clone();
                        let idle_activity = idle_activity.clone();
                        let sf = stream_file.clone();
                        let cn = close_notify.clone();
                        tokio::spawn(async move {
                            handle_connection(stream, state, idle_activity, sf, cn).await;
                        });
                    }
                    Err(e) => {
                        let _ = writeln!(std::io::stderr(), "Accept error: {}", e);
                    }
                }
            }
            _ = drain_interval.tick() => {
                let mut s = state.lock().await;
                let process_exited = s
                    .browser
                    .as_mut()
                    .map(|mgr| mgr.has_process_exited())
                    .unwrap_or(false);
                if process_exited {
                    let _ = close_current_browser(&mut s).await;
                } else if s.browser.is_some() {
                    if let Err(error) = s.drain_cdp_events_background().await {
                        let _ = writeln!(
                            std::io::stderr(),
                            "Failed to apply browser network controls: {}",
                            error
                        );
                    } else {
                        maybe_autosave_restore_state(&mut s, autosave_interval_ms).await;
                    }
                }
            }
            _ = async {
                match idle_sleep_pin {
                    Some(ref mut s) => s.as_mut().await,
                    None => std::future::pending::<()>().await,
                }
            }, if idle_timeout_ms.is_some() => {
                let mut s = state.lock().await;
                // The timer may have expired while a command held the state
                // lock. Command completion refreshes the shared activity
                // clock before releasing that lock, so re-check it here.
                if let Some(remaining) =
                    remaining_idle_timeout(&idle_activity, idle_timeout_ms.unwrap_or_default())
                {
                    idle_sleep_pin = Some(Box::pin(tokio::time::sleep(remaining)));
                    continue;
                }
                // The default timeout is a leak backstop, not a lifecycle
                // policy: never pull a headed, WebDriver, or attached browser
                // out from under a human. Re-arm and keep waiting instead.
                if idle_timeout.is_some_and(|t| t.is_default)
                    && s.blocks_default_idle_shutdown()
                {
                    idle_sleep_pin = idle_timeout_ms
                        .map(|ms| Box::pin(tokio::time::sleep(Duration::from_millis(ms))));
                    continue;
                }
                if idle_timeout.is_some_and(|t| t.is_default) {
                    let _ = writeln!(
                        std::io::stderr(),
                        "Idle for {}m with no commands or dashboard input; saving configured restore state and shutting down (AGENT_BROWSER_IDLE_TIMEOUT_MS=0 disables)",
                        DEFAULT_IDLE_TIMEOUT_MS / 60_000
                    );
                }
                let _ = auto_save_restore_state(&mut s).await;
                let _ = close_all_browser_backends(&mut s).await;
                break;
            }
            _ = idle_activity.notified(), if idle_timeout_ms.is_some() => {
                idle_sleep_pin = idle_timeout_ms
                    .map(|ms| Box::pin(tokio::time::sleep(Duration::from_millis(ms))));
                continue;
            }
            _ = close_notify.notified() => {
                // "close" command was handled; browser already closed by
                // handle_close(). Break to run cleanup and exit gracefully
                // so destructors fire.
                break;
            }
            _ = shutdown_signal() => {
                let mut s = state.lock().await;
                let _ = auto_save_restore_state(&mut s).await;
                let _ = close_all_browser_backends(&mut s).await;
                break;
            }
        }
    }

    Ok(())
}

#[cfg(windows)]
async fn run_socket_server(
    socket_path: &PathBuf,
    session: &str,
    stream_client: Option<Arc<RwLock<Option<Arc<CdpClient>>>>>,
    stream_server: Option<Arc<StreamServer>>,
    idle_activity: Arc<IdleActivity>,
    idle_timeout: Option<IdleTimeout>,
    autosave_interval_ms: u64,
) -> Result<(), String> {
    use tokio::net::TcpListener;

    let idle_timeout_ms = idle_timeout.map(|t| t.ms);

    let preferred_port = get_port_for_session(session);
    // Try the hash-derived port first; if it is blocked (e.g. Windows Hyper-V
    // excluded port range), fall back to an OS-assigned ephemeral port.
    let listener = match TcpListener::bind(format!("127.0.0.1:{}", preferred_port)).await {
        Ok(l) => l,
        Err(_) => TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("Failed to bind TCP: {}", e))?,
    };
    let actual_port = listener
        .local_addr()
        .map_err(|e| format!("Failed to get local address: {}", e))?
        .port();

    let socket_dir = socket_path.parent().unwrap_or(std::path::Path::new("."));
    let port_path = socket_dir.join(format!("{}.port", session));
    let _ = fs::write(&port_path, actual_port.to_string());

    let stream_file: Option<PathBuf> = if stream_server.is_some() {
        Some(socket_dir.join(format!("{}.stream", session)))
    } else {
        None
    };
    let state: std::sync::Arc<tokio::sync::Mutex<DaemonState>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(DaemonState::new_with_stream(
            stream_client,
            stream_server,
            idle_activity.clone(),
        )));

    let close_notify = Arc::new(Notify::new());

    let idle_sleep = idle_timeout_ms.map(|ms| tokio::time::sleep(Duration::from_millis(ms)));
    let mut idle_sleep_pin = idle_sleep.map(Box::pin);

    // Mirror the unix loop's background tick: reap a browser the user closed
    // by hand, and drain CDP events (dialog state in particular) before
    // autosave so a save never runs against a dialog-blocked renderer.
    let mut drain_interval = tokio::time::interval(Duration::from_millis(100));
    drain_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _)) => {
                        let state = state.clone();
                        let idle_activity = idle_activity.clone();
                        let sf = stream_file.clone();
                        let cn = close_notify.clone();
                        tokio::spawn(async move {
                            handle_connection(stream, state, idle_activity, sf, cn).await;
                        });
                    }
                    Err(e) => {
                        let _ = writeln!(std::io::stderr(), "Accept error: {}", e);
                    }
                }
            }
            _ = drain_interval.tick() => {
                let mut s = state.lock().await;
                let process_exited = s
                    .browser
                    .as_mut()
                    .map(|mgr| mgr.has_process_exited())
                    .unwrap_or(false);
                if process_exited {
                    let _ = close_current_browser(&mut s).await;
                } else if s.browser.is_some() {
                    s.drain_cdp_events_background().await;
                    maybe_autosave_restore_state(&mut s, autosave_interval_ms).await;
                }
            }
            _ = async {
                match idle_sleep_pin {
                    Some(ref mut s) => s.as_mut().await,
                    None => std::future::pending::<()>().await,
                }
            }, if idle_timeout_ms.is_some() => {
                let mut s = state.lock().await;
                if let Some(remaining) =
                    remaining_idle_timeout(&idle_activity, idle_timeout_ms.unwrap_or_default())
                {
                    idle_sleep_pin = Some(Box::pin(tokio::time::sleep(remaining)));
                    continue;
                }
                // The default timeout is a leak backstop, not a lifecycle
                // policy: never pull a headed, WebDriver, or attached browser
                // out from under a human. Re-arm and keep waiting instead.
                if idle_timeout.is_some_and(|t| t.is_default)
                    && s.blocks_default_idle_shutdown()
                {
                    idle_sleep_pin = idle_timeout_ms
                        .map(|ms| Box::pin(tokio::time::sleep(Duration::from_millis(ms))));
                    continue;
                }
                if idle_timeout.is_some_and(|t| t.is_default) {
                    let _ = writeln!(
                        std::io::stderr(),
                        "Idle for {}m with no commands or dashboard input; saving configured restore state and shutting down (AGENT_BROWSER_IDLE_TIMEOUT_MS=0 disables)",
                        DEFAULT_IDLE_TIMEOUT_MS / 60_000
                    );
                }
                let _ = auto_save_restore_state(&mut s).await;
                let _ = close_all_browser_backends(&mut s).await;
                let _ = fs::remove_file(&port_path);
                break;
            }
            _ = idle_activity.notified(), if idle_timeout_ms.is_some() => {
                idle_sleep_pin = idle_timeout_ms
                    .map(|ms| Box::pin(tokio::time::sleep(Duration::from_millis(ms))));
                continue;
            }
            _ = close_notify.notified() => {
                let _ = fs::remove_file(&port_path);
                break;
            }
            _ = shutdown_signal() => {
                let mut s = state.lock().await;
                let _ = auto_save_restore_state(&mut s).await;
                let _ = close_all_browser_backends(&mut s).await;
                let _ = fs::remove_file(&port_path);
                break;
            }
        }
    }

    Ok(())
}

async fn handle_connection<S>(
    stream: S,
    state: std::sync::Arc<tokio::sync::Mutex<DaemonState>>,
    idle_activity: Arc<IdleActivity>,
    stream_file_cleanup: Option<PathBuf>,
    close_notify: Arc<Notify>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        match buf_reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                if looks_like_http(trimmed) {
                    break;
                }

                let cmd: Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(e) => {
                        let err = serde_json::json!({
                            "success": false,
                            "error": format!("Invalid JSON: {}", e),
                        });
                        let mut resp = serde_json::to_string(&err).unwrap_or_default();
                        resp.push('\n');
                        let _ = writer.write_all(resp.as_bytes()).await;
                        continue;
                    }
                };

                idle_activity.mark();

                let action = cmd
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();

                let response = {
                    let mut s = state.lock().await;
                    let response = execute_command(&cmd, &mut s).await;
                    // Refresh while the state lock is still held. An idle
                    // timer waiting on this command will observe the updated
                    // clock as soon as it acquires the lock.
                    idle_activity.mark();
                    response
                };

                let mut resp = serde_json::to_string(&response).unwrap_or_default();
                resp.push('\n');
                if writer.write_all(resp.as_bytes()).await.is_err() {
                    break;
                }

                if close_completed_response(&action, &response) {
                    if let Some(ref path) = stream_file_cleanup {
                        let _ = fs::remove_file(path);
                    }
                    // Signal the daemon loop to exit gracefully instead of
                    // calling process::exit(), which skips destructors and
                    // can leave Chrome processes orphaned (issue #1113).
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    close_notify.notify_one();
                    return;
                }
            }
            Err(_) => break,
        }
    }
}

fn looks_like_http(line: &str) -> bool {
    let prefixes = [
        "GET ", "POST ", "PUT ", "DELETE ", "PATCH ", "HEAD ", "OPTIONS ", "CONNECT ", "TRACE ",
    ];
    prefixes.iter().any(|p| line.starts_with(p))
}

fn close_completed_response(action: &str, response: &Value) -> bool {
    if !matches!(
        action,
        "close" | "confirm" | INTERNAL_DAEMON_SHUTDOWN_ACTION
    ) {
        return false;
    }

    fn data_closed(data: &Value) -> bool {
        data.get("closed").and_then(|v| v.as_bool()) == Some(true)
    }

    if response.get("success").and_then(|v| v.as_bool()) != Some(true) {
        return false;
    }

    let Some(data) = response.get("data") else {
        return false;
    };
    if data_closed(data) {
        return true;
    }

    data.get("result").is_some_and(|result| {
        result.get("success").and_then(|v| v.as_bool()) == Some(true)
            && result.get("data").is_some_and(data_closed)
    })
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut sigint = match signal::unix::signal(signal::unix::SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "Failed to install SIGINT handler: {}", e);
                process::exit(1);
            }
        };
        let mut sigterm = match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                let _ = writeln!(
                    std::io::stderr(),
                    "Failed to install SIGTERM handler: {}",
                    e
                );
                process::exit(1);
            }
        };
        let mut sighup = match signal::unix::signal(signal::unix::SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "Failed to install SIGHUP handler: {}", e);
                process::exit(1);
            }
        };

        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
            _ = sighup.recv() => {}
        }
    }

    #[cfg(windows)]
    {
        if let Err(e) = signal::ctrl_c().await {
            let _ = writeln!(std::io::stderr(), "Failed to install Ctrl+C handler: {}", e);
            process::exit(1);
        }
    }
}

fn get_daemon_socket_dir() -> PathBuf {
    crate::connection::get_socket_dir()
}

#[cfg(windows)]
fn get_port_for_session(session: &str) -> u16 {
    crate::connection::get_port_for_session(session)
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    /// Exercise the actual per-connection executor and its shared state lock.
    /// Keeping the first executor as a pinned future lets the fixture make a
    /// complete response readable before resuming it, including after its
    /// deadline. A timeout around an otherwise unbounded read can still poll
    /// that ready inner future to completion before checking its timer.
    async fn completion_webdriver_failure_releases_daemon(near_deadline: bool) {
        use crate::native::actions::BackendType;
        use crate::native::webdriver::{backend::WebDriverBackend, client::WebDriverClient};
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (request_seen_tx, request_seen_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (response_written_tx, response_written_rx) = tokio::sync::oneshot::channel();
        let (no_retry_tx, no_retry_rx) = tokio::sync::oneshot::channel();
        let backend_server = tokio::spawn(async move {
            async fn read_request(stream: &mut tokio::net::TcpStream) {
                let mut request = Vec::new();
                let mut bytes = [0; 1024];
                while !request.ends_with(b"\r\n\r\n") {
                    let count = stream.read(&mut bytes).await.unwrap();
                    assert!(count > 0, "expected a complete WebDriver request");
                    request.extend_from_slice(&bytes[..count]);
                    assert!(request.len() < 4096, "unexpected extra WebDriver request");
                }
                assert!(String::from_utf8(request)
                    .unwrap()
                    .starts_with("GET /session/completion-daemon/url HTTP/1.1\r\n"));
            }

            async fn write_response(stream: &mut tokio::net::TcpStream, body: &str) {
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body
                );
                tokio::time::timeout(
                    Duration::from_secs(2),
                    stream.write_all(response.as_bytes()),
                )
                .await
                .expect("the complete fixture response must fit in the socket")
                .unwrap();
                // The complete response must be written, but its reader may
                // already have closed before our local write-half shutdown.
                let _ = stream.shutdown().await;
            }

            async fn assert_backend_closed(stream: &mut tokio::net::TcpStream) {
                let closed = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut [0]))
                    .await
                    .expect("completion must promptly release its backend socket");
                assert!(
                    matches!(closed, Ok(0))
                        || matches!(closed, Err(ref error) if matches!(error.kind(),
                            std::io::ErrorKind::ConnectionReset
                                | std::io::ErrorKind::ConnectionAborted
                                | std::io::ErrorKind::BrokenPipe)),
                    "completion must close the socket without another request: {closed:?}"
                );
            }

            let (mut first, _) = listener.accept().await.unwrap();
            read_request(&mut first).await;
            request_seen_tx.send(()).unwrap();
            release_rx.await.unwrap();
            let body = if near_deadline {
                serde_json::json!({ "value": "https://example.com/too-late" }).to_string()
            } else {
                // This valid, complete JSON exceeds the wire cap without
                // relying on an endless stream or on a malformed body. Keep
                // it just over the cap to fit portable TCP fixture buffers.
                serde_json::json!({ "value": "https://example.com/".to_string() + &"x".repeat(64 * 1024) }).to_string()
            };
            write_response(&mut first, &body).await;
            response_written_tx.send(()).unwrap();
            assert_backend_closed(&mut first).await;
            assert!(
                tokio::time::timeout(Duration::from_millis(30), listener.accept())
                    .await
                    .is_err(),
                "failed completion must not retry or create another backend session"
            );
            no_retry_tx.send(()).unwrap();

            let (mut next, _) = listener.accept().await.unwrap();
            read_request(&mut next).await;
            write_response(&mut next, r#"{"value":"https://example.com/next"}"#).await;
            assert_backend_closed(&mut next).await;
            assert!(
                tokio::time::timeout(Duration::from_millis(30), listener.accept())
                    .await
                    .is_err(),
                "only the two explicitly requested URL reads are allowed"
            );
        });

        let mut initial = DaemonState::new();
        initial.backend_type = BackendType::WebDriver;
        initial.webdriver_backend = Some(WebDriverBackend::new(WebDriverClient::new_with_session(
            port,
            "completion-daemon".to_string(),
        )));
        initial.session_id = "completion-daemon".to_string();
        initial.session_name = None;
        initial.policy = None;
        initial.confirm_actions = None;
        let state = Arc::new(tokio::sync::Mutex::new(initial));
        let activity = Arc::new(IdleActivity::new());
        let close_notify = Arc::new(Notify::new());
        let (mut first_client, first_daemon) = tokio::io::duplex(8192);
        first_client
            .write_all(b"{\"action\":\"url\",\"id\":\"completion\",\"existingBrowserOnly\":true}\n")
            .await
            .unwrap();
        let completion = handle_connection(
            first_daemon,
            state.clone(),
            activity.clone(),
            None,
            close_notify.clone(),
        );
        tokio::pin!(completion);
        let start = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                _ = &mut completion => panic!("completion connection exited before the backend request"),
                seen = request_seen_rx => seen.unwrap(),
            }
        })
        .await
        .unwrap();
        assert!(
            state.try_lock().is_err(),
            "completion must hold daemon state during the read"
        );

        let (mut queued_client, queued_daemon) = tokio::io::duplex(8192);
        let queued = tokio::spawn(handle_connection(
            queued_daemon,
            state.clone(),
            activity,
            None,
            close_notify,
        ));
        queued_client
            .write_all(b"{\"action\":\"session_info\",\"id\":\"queued\"}\n")
            .await
            .unwrap();
        let mut queued_client = BufReader::new(queued_client);
        let mut queued_response = String::new();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                queued_client.read_line(&mut queued_response),
            )
            .await
            .is_err(),
            "the second connection must queue behind the first command's state lock"
        );

        if near_deadline {
            tokio::time::sleep_until(tokio::time::Instant::from_std(
                start + Duration::from_millis(crate::goal::FINAL_URL_TIMEOUT_MS - 5),
            ))
            .await;
        }
        release_tx.send(()).unwrap();
        response_written_rx.await.unwrap();
        // Give the reactor a turn while the completion future remains
        // unpolled. In the deadline case this deliberately resumes after the
        // budget with a complete response already ready to read and parse.
        tokio::time::sleep(Duration::from_millis(if near_deadline { 20 } else { 1 })).await;
        let resumed = std::time::Instant::now();
        let mut first_client = BufReader::new(first_client);
        let mut response = String::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                _ = &mut completion => panic!("completion connection exited before sending its response"),
                result = first_client.read_line(&mut response) => { result.unwrap(); },
            }
            queued_client.read_line(&mut queued_response).await.unwrap();
        })
        .await
        .expect("fallback must release daemon state for the queued command");
        assert!(resumed.elapsed() < Duration::from_millis(500));
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["success"], false, "{response}");
        let error = response["error"].as_str().unwrap();
        if near_deadline {
            assert!(error.contains("Completion URL read timed out"), "{error}");
        } else {
            assert!(error.contains("64 KiB"), "{error}");
            assert!(start.elapsed() < Duration::from_millis(500));
        }
        let queued_response: Value = serde_json::from_str(&queued_response).unwrap();
        assert_eq!(queued_response["success"], true);
        assert_eq!(queued_response["data"]["session"], "completion-daemon");
        assert_eq!(queued_response["data"]["browserLaunched"], false);
        no_retry_rx.await.unwrap();

        // An explicit later URL command must still reach the original
        // WebDriver session, proving both lock and backend remain usable.
        queued_client
            .get_mut()
            .write_all(b"{\"action\":\"url\",\"id\":\"next\",\"existingBrowserOnly\":true}\n")
            .await
            .unwrap();
        let mut next_response = String::new();
        tokio::time::timeout(
            Duration::from_millis(500),
            queued_client.read_line(&mut next_response),
        )
        .await
        .expect("subsequent URL commands must remain available")
        .unwrap();
        let next_response: Value = serde_json::from_str(&next_response).unwrap();
        assert_eq!(next_response["success"], true);
        assert_eq!(next_response["data"]["url"], "https://example.com/next");
        assert_eq!(next_response["data"]["lifecycle"]["launched"], false);
        assert_eq!(
            next_response["data"]["lifecycle"]["relaunchedBrowser"],
            false
        );
        {
            let state = state.lock().await;
            assert!(state.browser.is_none());
            assert!(state.webdriver_backend.is_some());
            assert!(matches!(state.backend_type, BackendType::WebDriver));
            assert!(state.pending_confirmation.is_none());
            assert!(state.last_command_finished.is_none());
        }
        drop(first_client);
        tokio::time::timeout(Duration::from_secs(1), &mut completion)
            .await
            .unwrap();
        drop(queued_client);
        queued.await.unwrap();
        backend_server.await.unwrap();
    }

    #[tokio::test]
    async fn test_completion_webdriver_oversized_response_releases_daemon() {
        completion_webdriver_failure_releases_daemon(false).await;
    }

    #[tokio::test]
    async fn test_completion_webdriver_ready_response_after_deadline_releases_daemon() {
        completion_webdriver_failure_releases_daemon(true).await;
    }

    #[test]
    fn test_resolve_idle_timeout_unset_applies_default() {
        let t = resolve_idle_timeout(None).expect("default should apply when unset");
        assert_eq!(t.ms, DEFAULT_IDLE_TIMEOUT_MS);
        assert!(t.is_default);
    }

    #[test]
    fn test_resolve_idle_timeout_explicit_zero_disables() {
        assert!(resolve_idle_timeout(Some("0".to_string())).is_none());
        assert!(resolve_idle_timeout(Some(" 0 ".to_string())).is_none());
    }

    #[test]
    fn test_resolve_idle_timeout_explicit_value_is_not_default() {
        let t = resolve_idle_timeout(Some("5000".to_string())).expect("explicit value");
        assert_eq!(t.ms, 5000);
        assert!(!t.is_default);
    }

    #[test]
    fn test_resolve_idle_timeout_unparseable_falls_back_to_default() {
        for raw in ["banana", "", "-1", "30s"] {
            let t = resolve_idle_timeout(Some(raw.to_string()))
                .unwrap_or_else(|| panic!("{:?} should fall back to default", raw));
            assert_eq!(t.ms, DEFAULT_IDLE_TIMEOUT_MS);
            assert!(t.is_default);
        }
    }

    #[test]
    fn test_default_idle_timeout_does_not_close_webdriver_sessions() {
        let mut state = DaemonState::new();
        assert!(!state.blocks_default_idle_shutdown());

        state.backend_type = crate::native::actions::BackendType::WebDriver;
        assert!(state.blocks_default_idle_shutdown());
    }

    #[tokio::test]
    async fn test_idle_activity_receives_dashboard_activity() {
        let activity = Arc::new(IdleActivity::new());
        activity.mark();

        tokio::time::timeout(Duration::from_millis(100), activity.notified())
            .await
            .expect("dashboard input notification should wake the idle loop");
    }

    #[tokio::test]
    async fn test_command_completion_rearms_expired_idle_timeout() {
        let activity = IdleActivity::new();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            remaining_idle_timeout(&activity, 1).is_none(),
            "the original idle deadline should have expired"
        );

        // A command that held the daemon state lock past the deadline marks
        // completion before releasing the lock. The timeout path must then
        // wait for a new full idle period instead of closing immediately.
        activity.mark();
        assert!(remaining_idle_timeout(&activity, 100).is_some());
    }

    #[test]
    fn test_daemon_socket_dir_matches_client_namespace() {
        let guard = crate::test_utils::EnvGuard::new(&[
            "AGENT_BROWSER_SOCKET_DIR",
            "XDG_RUNTIME_DIR",
            "AGENT_BROWSER_NAMESPACE",
        ]);
        let dir = tempfile::tempdir().unwrap();
        guard.set("AGENT_BROWSER_SOCKET_DIR", dir.path().to_str().unwrap());
        guard.remove("XDG_RUNTIME_DIR");
        guard.set("AGENT_BROWSER_NAMESPACE", "Worktree: One");

        let socket_dir = get_daemon_socket_dir();

        assert_eq!(socket_dir, crate::connection::get_socket_dir());
        assert!(socket_dir.ends_with(
            std::path::PathBuf::from("namespaces")
                .join("worktree-one")
                .join("run")
        ));
    }

    #[cfg(windows)]
    #[test]
    fn test_port_matches_client_algorithm() {
        let guard = crate::test_utils::EnvGuard::new(&["AGENT_BROWSER_NAMESPACE"]);
        guard.remove("AGENT_BROWSER_NAMESPACE");

        assert_eq!(get_port_for_session("default"), 50838);
        assert_eq!(get_port_for_session("my-session"), 63105);
        assert_eq!(get_port_for_session("work"), 51184);
        assert_eq!(get_port_for_session(""), 49152);
    }

    #[test]
    fn test_close_completed_response_requires_actual_close_result() {
        let confirmation_response = serde_json::json!({
            "success": true,
            "data": {
                "confirmation_required": true,
                "confirmation_id": "close-1",
                "action": "close"
            }
        });

        assert!(!close_completed_response("close", &confirmation_response));
    }

    #[test]
    fn test_close_completed_response_accepts_direct_and_confirmed_close() {
        let direct = serde_json::json!({
            "success": true,
            "data": { "closed": true }
        });
        let confirmed = serde_json::json!({
            "success": true,
            "data": {
                "confirmed": true,
                "action": "close",
                "result": {
                    "success": true,
                    "data": { "closed": true }
                }
            }
        });

        assert!(close_completed_response("close", &direct));
        assert!(close_completed_response(
            crate::connection::INTERNAL_DAEMON_SHUTDOWN_ACTION,
            &direct
        ));
        assert!(close_completed_response("confirm", &confirmed));
    }

    /// Guard against re-introducing `waitpid(-1)` in daemon code.
    ///
    /// Issue #1035: a SIGCHLD handler that called `waitpid(-1, WNOHANG)` was
    /// added in v0.22.3 to reap zombie Chrome processes. This races with
    /// Rust's `Child::try_wait()` / `Child::wait()` because `waitpid(-1)`
    /// reaps *any* child, stealing the exit status before Rust can collect
    /// it. The result is ECHILD errors in `BrowserManager::has_process_exited()`
    /// and `ChromeProcess::kill()`, which can leave the daemon in a broken
    /// state or cause hangs on certain Linux configurations.
    ///
    /// The fix uses the existing 500ms drain interval to call
    /// `has_process_exited()` (which delegates to `Child::try_wait()`)
    /// for targeted, race-free zombie detection.
    #[test]
    fn test_no_waitpid_minus_one_in_daemon() {
        let source = include_str!("daemon.rs");
        // Only check production code (everything before `#[cfg(test)]`)
        let production_code = source.split("#[cfg(test)]").next().unwrap_or(source);
        assert!(
            !production_code.contains("waitpid(-1"),
            "daemon.rs production code must not call waitpid(-1, ...). \
             Use Child::try_wait() via has_process_exited() instead. \
             See issue #1035."
        );
    }

    /// Verify that `Child::try_wait()` correctly detects a crashed child
    /// without needing a global SIGCHLD handler or `waitpid(-1)`.
    /// This is what `has_process_exited()` uses in the fixed code.
    #[cfg(unix)]
    #[test]
    fn test_child_try_wait_detects_exit_without_sigchld_handler() {
        use std::process::{Command, Stdio};

        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 42"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn child");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("child did not exit before the deadline");
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                Err(e) => panic!("try_wait() should succeed without waitpid(-1): {}", e),
            }
        };

        assert_eq!(status.code(), Some(42));
    }

    /// Regression test for #1101: idle timeout must fire even while the
    /// drain interval ticks every 500 ms.  The bug was that `sleep_future`
    /// was created **inside** the loop, so each drain tick dropped the
    /// in-progress sleep and replaced it with a fresh one – the timer
    /// could never reach its deadline.
    #[tokio::test]
    async fn test_idle_timeout_fires_despite_drain_interval() {
        let idle_timeout_ms: u64 = 1000;
        let mut drain_interval = tokio::time::interval(Duration::from_millis(500));
        drain_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let activity = IdleActivity::new();

        let start = tokio::time::Instant::now();

        let exited = tokio::time::timeout(Duration::from_secs(5), async {
            let mut idle_sleep_pin = Some(Box::pin(tokio::time::sleep(Duration::from_millis(
                idle_timeout_ms,
            ))));

            loop {
                tokio::select! {
                    _ = drain_interval.tick() => {}
                    _ = async {
                        match idle_sleep_pin {
                            Some(ref mut s) => s.as_mut().await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        break;
                    }
                    _ = activity.notified() => {
                        idle_sleep_pin = Some(Box::pin(
                            tokio::time::sleep(Duration::from_millis(idle_timeout_ms)),
                        ));
                        continue;
                    }
                }
            }
        })
        .await;

        let elapsed = start.elapsed();

        assert!(
            exited.is_ok(),
            "idle timeout never fired – loop ran for >5 s (bug #1101)"
        );
        assert!(
            elapsed < Duration::from_millis(idle_timeout_ms + 500),
            "idle timeout took too long: {:?} (expected ~{} ms)",
            elapsed,
            idle_timeout_ms,
        );
    }

    /// Verify that `ChromeProcess::has_exited()` (which uses `Child::try_wait()`)
    /// correctly detects a killed child, the same way the drain interval does
    /// in the fixed daemon code. This ensures crash detection works without
    /// a SIGCHLD handler.
    #[cfg(unix)]
    #[test]
    fn test_has_exited_detects_killed_process() {
        use std::process::{Command, Stdio};

        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 60"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn child");

        // Process should be running
        match child.try_wait() {
            Ok(None) => {} // expected
            other => panic!("expected Ok(None) for running process, got {:?}", other),
        }

        // Kill it (simulates Chrome crash)
        child.kill().expect("failed to kill child");
        std::thread::sleep(std::time::Duration::from_millis(100));

        // try_wait should detect the exit
        match child.try_wait() {
            Ok(Some(_)) => {} // expected: detected the crash
            other => panic!(
                "expected Ok(Some(_)) after kill, got {:?}. \
                 Crash detection via try_wait() must work for the drain \
                 interval fix (issue #1035) to function correctly.",
                other
            ),
        }
    }
}
