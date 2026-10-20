//! Pod log streaming via kube-rs.
//!
//! Each active log stream runs as a tokio task that reads from the K8s log API
//! and sends lines through a bounded channel. The UI thread receives lines and
//! appends them to the log viewer buffer.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures::AsyncBufReadExt;
use kube::{
    api::LogParams,
    config::{KubeConfigOptions, Kubeconfig},
    Api, Client,
};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::config::EnvironmentConfig;

/// A single parsed log line.
#[derive(Debug, Clone)]
pub struct LogLine {
    /// Raw text of the line
    pub text: String,
    /// Parsed timestamp (if available)
    pub timestamp: Option<DateTime<Utc>>,
    /// Parsed log level (if available)
    pub level: Option<LogLevel>,
    /// Pod name this line came from (for multi-pod "All" view)
    pub pod_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

/// Parse log level from a JSON log line or plain text.
fn parse_log_level(text: &str) -> Option<LogLevel> {
    // Try JSON first ({"level":"INFO",...} or {"level":"WARN",...})
    if text.starts_with('{') {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(text) {
            if let Some(level_str) = val.get("level").and_then(|v| v.as_str()) {
                return match level_str.to_uppercase().as_str() {
                    "TRACE" => Some(LogLevel::Trace),
                    "DEBUG" => Some(LogLevel::Debug),
                    "INFO" => Some(LogLevel::Info),
                    "WARN" | "WARNING" => Some(LogLevel::Warn),
                    "ERROR" => Some(LogLevel::Error),
                    "FATAL" => Some(LogLevel::Fatal),
                    _ => None,
                };
            }
        }
    }

    // Plain text scan — check for level keywords in common formats:
    // "2026-03-12 INFO Starting server", "[ERROR] Connection refused", "level=WARN ..."
    let upper = text.to_uppercase();
    if upper.contains(" FATAL ") || upper.contains("[FATAL]") || upper.contains("]FATAL") {
        Some(LogLevel::Fatal)
    } else if upper.contains(" ERROR ") || upper.contains("[ERROR]") || upper.contains("]ERROR") {
        Some(LogLevel::Error)
    } else if upper.contains(" WARN ")
        || upper.contains("[WARN]")
        || upper.contains("]WARN")
        || upper.contains(" WARNING ")
        || upper.contains("[WARNING]")
    {
        Some(LogLevel::Warn)
    } else if upper.contains(" INFO ") || upper.contains("[INFO]") || upper.contains("]INFO") {
        Some(LogLevel::Info)
    } else if upper.contains(" DEBUG ") || upper.contains("[DEBUG]") || upper.contains("]DEBUG") {
        Some(LogLevel::Debug)
    } else if upper.contains(" TRACE ") || upper.contains("[TRACE]") || upper.contains("]TRACE") {
        Some(LogLevel::Trace)
    } else {
        None
    }
}

/// Parse timestamp from a JSON log line.
fn parse_timestamp(text: &str) -> Option<DateTime<Utc>> {
    if !text.starts_with('{') {
        return None;
    }
    let val: serde_json::Value = serde_json::from_str(text).ok()?;
    let ts_str = val
        .get("timestamp")
        .or_else(|| val.get("time"))
        .or_else(|| val.get("ts"))
        .and_then(|v| v.as_str())?;
    DateTime::parse_from_rfc3339(ts_str)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Handle to an active log stream. Drop to cancel.
pub struct LogStreamHandle {
    /// Receive log lines from the background task.
    pub rx: mpsc::Receiver<LogLine>,
    /// The pod name this stream is for.
    pub pod_name: String,
    /// Drop this to cancel the background task.
    _cancel: tokio::sync::watch::Sender<bool>,
}

/// Request parameters for starting a log stream.
#[derive(Debug, Clone)]
pub struct LogStreamRequest {
    pub env_name: String,
    pub env_config: EnvironmentConfig,
    pub namespace: String,
    pub pod_name: String,
    /// Container name (None = default container)
    pub container: Option<String>,
    /// Number of historical lines to fetch (tail)
    pub tail_lines: i64,
    /// Whether to follow (stream) new lines
    pub follow: bool,
}

/// Start a log stream for a specific pod. Returns a handle with a receiver channel.
/// The stream runs until the handle is dropped or the pod stops.
pub fn start_log_stream(
    runtime: &tokio::runtime::Runtime,
    request: LogStreamRequest,
) -> LogStreamHandle {
    let (tx, rx) = mpsc::channel::<LogLine>(2048);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

    let pod_name = request.pod_name.clone();
    runtime.spawn(run_log_stream(request, tx, cancel_rx));

    LogStreamHandle {
        rx,
        pod_name,
        _cancel: cancel_tx,
    }
}

async fn run_log_stream(
    request: LogStreamRequest,
    tx: mpsc::Sender<LogLine>,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
) {
    if let Err(e) = run_log_stream_inner(&request, &tx, &mut cancel_rx).await {
        warn!(
            env = %request.env_name,
            pod = %request.pod_name,
            err = %e,
            "Log stream ended with error"
        );
        // Send error as a log line so the UI can show it
        let _ = tx
            .send(LogLine {
                text: format!("--- Log stream error: {} ---", e),
                timestamp: Some(Utc::now()),
                level: Some(LogLevel::Error),
                pod_name: Some(request.pod_name.clone()),
            })
            .await;
    }
}

async fn run_log_stream_inner(
    request: &LogStreamRequest,
    tx: &mpsc::Sender<LogLine>,
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let client = create_client(&request.env_config).await?;
    let pods_api: Api<k8s_openapi::api::core::v1::Pod> =
        Api::namespaced(client, &request.namespace);

    let mut params = LogParams {
        follow: request.follow,
        tail_lines: Some(request.tail_lines),
        timestamps: true,
        ..Default::default()
    };
    if let Some(ref container) = request.container {
        params.container = Some(container.clone());
    }

    info!(
        env = %request.env_name,
        pod = %request.pod_name,
        ns = %request.namespace,
        follow = request.follow,
        tail = request.tail_lines,
        "Starting log stream"
    );

    if request.follow {
        // Streaming mode: read lines via futures::AsyncBufReadExt
        // kube-rs log_stream returns futures_io::AsyncBufRead
        let stream = pods_api
            .log_stream(&request.pod_name, &params)
            .await
            .context("Failed to start log stream")?;

        let reader = futures::io::BufReader::new(stream);
        let mut lines_stream = reader.lines();

        use futures::StreamExt;
        loop {
            tokio::select! {
                _ = cancel_rx.changed() => {
                    info!(pod = %request.pod_name, "Log stream cancelled");
                    break;
                }
                result = lines_stream.next() => {
                    match result {
                        Some(Ok(line)) => {
                            if line.is_empty() {
                                continue;
                            }
                            let log_line = parse_line(&line);
                            if tx.send(log_line).await.is_err() {
                                // Receiver dropped
                                return Ok(());
                            }
                        }
                        Some(Err(e)) => {
                            return Err(e.into());
                        }
                        None => {
                            info!(pod = %request.pod_name, "Log stream ended (pod completed)");
                            let _ = tx.send(LogLine {
                                text: "--- Log stream ended ---".to_string(),
                                timestamp: Some(Utc::now()),
                                level: None,
                                pod_name: Some(request.pod_name.clone()),
                            }).await;
                            break;
                        }
                    }
                }
            }
        }
    } else {
        // One-shot mode: fetch tail and return
        let logs = pods_api
            .logs(&request.pod_name, &params)
            .await
            .context("Failed to fetch logs")?;

        for line in logs.lines() {
            if line.is_empty() {
                continue;
            }
            let log_line = parse_line(line);
            if tx.send(log_line).await.is_err() {
                return Ok(());
            }
        }
    }

    Ok(())
}

/// Parse a raw log line (potentially with K8s timestamp prefix) into a LogLine.
fn parse_line(raw: &str) -> LogLine {
    // K8s timestamps mode prepends "2026-03-12T17:16:16.377526803Z <log>"
    // Try to split on the first space after an RFC3339-like prefix
    let (k8s_ts, body) = if raw.len() > 30 && raw.as_bytes().get(4) == Some(&b'-') {
        if let Some(space_idx) = raw[..35.min(raw.len())].find(' ') {
            let ts_part = &raw[..space_idx];
            if let Ok(dt) = DateTime::parse_from_rfc3339(ts_part) {
                (Some(dt.with_timezone(&Utc)), &raw[space_idx + 1..])
            } else {
                (None, raw)
            }
        } else {
            (None, raw)
        }
    } else {
        (None, raw)
    };

    let level = parse_log_level(body);
    let body_ts = parse_timestamp(body);
    let timestamp = body_ts.or(k8s_ts);

    LogLine {
        text: body.to_string(),
        timestamp,
        level,
        pod_name: None,
    }
}

/// Create a kube::Client from an EnvironmentConfig (same as k8s poller).
async fn create_client(env_config: &EnvironmentConfig) -> Result<Client> {
    let kubeconfig_path = crate::config::Config::expand_path(&env_config.kubeconfig);
    let kubeconfig_path_str = kubeconfig_path
        .to_str()
        .context("Invalid kubeconfig path")?;

    let kubeconfig = Kubeconfig::read_from(kubeconfig_path_str)
        .with_context(|| format!("Failed to read kubeconfig: {}", kubeconfig_path_str))?;

    let kube_options = KubeConfigOptions {
        context: env_config.context.clone(),
        ..Default::default()
    };

    let config = kube::Config::from_custom_kubeconfig(kubeconfig, &kube_options)
        .await
        .with_context(|| format!("Failed to build K8s config from: {}", kubeconfig_path_str))?;

    Client::try_from(config).context("Failed to create K8s client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_level_from_json() {
        assert_eq!(
            parse_log_level(r#"{"level":"INFO","message":"hello"}"#),
            Some(LogLevel::Info)
        );
        assert_eq!(
            parse_log_level(r#"{"level":"WARN","message":"oops"}"#),
            Some(LogLevel::Warn)
        );
        assert_eq!(
            parse_log_level(r#"{"level":"ERROR","message":"bad"}"#),
            Some(LogLevel::Error)
        );
        assert_eq!(
            parse_log_level(r#"{"level":"WARNING","message":"hmm"}"#),
            Some(LogLevel::Warn)
        );
    }

    #[test]
    fn parse_level_from_plain_text() {
        assert_eq!(
            parse_log_level("2026-03-12 INFO Starting server"),
            Some(LogLevel::Info)
        );
        assert_eq!(
            parse_log_level("[ERROR] Connection refused"),
            Some(LogLevel::Error)
        );
        assert_eq!(parse_log_level("just some text"), None);
    }

    #[test]
    fn parse_timestamp_from_json() {
        let ts =
            parse_timestamp(r#"{"timestamp":"2026-03-12T17:16:16.377526803Z","level":"INFO"}"#);
        assert!(ts.is_some());
    }

    #[test]
    fn parse_k8s_timestamped_line() {
        let line = "2026-03-12T17:16:16.377526803Z {\"level\":\"INFO\",\"message\":\"hello\"}";
        let parsed = parse_line(line);
        assert!(parsed.timestamp.is_some());
        assert_eq!(parsed.level, Some(LogLevel::Info));
        assert!(parsed.text.starts_with('{'));
    }

    #[test]
    fn parse_plain_line() {
        let line = "just a plain log line";
        let parsed = parse_line(line);
        assert_eq!(parsed.text, "just a plain log line");
        assert!(parsed.timestamp.is_none());
        assert!(parsed.level.is_none());
    }
}
