use agentfs_sdk::{
    AdapterControlActionV1, AdapterControlOutcomeV1, AdapterErrorV1, AdapterExecutionModeV1,
    AdapterInputModeV1, AdapterStreamingPlanV1, AdapterSubmitOutcomeV1, AppAdapterV1,
    DemoAppAdapterV1, RequestContextV1,
};
use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};
use uuid::Uuid;

mod http_bridge_adapter;

use http_bridge_adapter::HttpBridgeAdapterV1;

const DEFAULT_RETENTION_HINT_SEC: i64 = 86400;
const MIN_POLL_MS: u64 = 50;
#[cfg(unix)]
const ACTION_COOLDOWN_MS: u64 = 1500;
const SUBMIT_STABLE_MS: u64 = 1100;

const ERR_PAGER_HANDLE_NOT_FOUND: &str = "PAGER_HANDLE_NOT_FOUND";
const ERR_PAGER_HANDLE_EXPIRED: &str = "PAGER_HANDLE_EXPIRED";
const ERR_PAGER_HANDLE_CLOSED: &str = "PAGER_HANDLE_CLOSED";
const ERR_PERMISSION_DENIED: &str = "PERMISSION_DENIED";
const ERR_INVALID_ARGUMENT: &str = "INVALID_ARGUMENT";
const ERR_INVALID_PAYLOAD: &str = "INVALID_PAYLOAD";
const ERR_INTERNAL: &str = "INTERNAL";
const MAX_SEGMENT_BYTES: usize = 255;

const ALLOWED_SEGMENT_CHARS: &str =
    "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-~";

pub async fn handle_appfs_adapter_command(
    root: PathBuf,
    app_id: String,
    session_id: Option<String>,
    poll_ms: u64,
    adapter_http_endpoint: Option<String>,
    adapter_http_timeout_ms: u64,
) -> Result<()> {
    let session_id = session_id.unwrap_or_else(|| {
        let uuid = Uuid::new_v4().simple().to_string();
        format!("sess-{}", &uuid[..8])
    });

    let mut adapter = AppfsAdapter::new(
        root,
        app_id,
        session_id,
        adapter_http_endpoint,
        adapter_http_timeout_ms,
    )?;
    adapter.prepare_action_sinks()?;

    eprintln!(
        "AppFS adapter started for {} (session={})",
        adapter.app_dir.display(),
        adapter.session_id
    );
    eprintln!("Press Ctrl+C to stop.");

    let mut interval = tokio::time::interval(Duration::from_millis(poll_ms.max(MIN_POLL_MS)));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("AppFS adapter stopping...");
                return Ok(());
            }
            _ = interval.tick() => {
                if let Err(err) = adapter.poll_once() {
                    eprintln!("AppFS adapter poll error: {err:#}");
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessOutcome {
    Submitted,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutionMode {
    Inline,
    Streaming,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Text,
    Json,
    TextOrJson,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActionFingerprint {
    len: u64,
    modified_ns: u128,
}

#[derive(Debug, Clone, Copy)]
struct PendingSubmit {
    fingerprint: ActionFingerprint,
    first_seen: Instant,
}

#[derive(Debug, Clone)]
struct ActionSpec {
    template: String,
    input_mode: InputMode,
    execution_mode: ExecutionMode,
    max_payload_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ManifestDoc {
    #[serde(default)]
    nodes: HashMap<String, ManifestNodeDoc>,
}

#[derive(Debug, Deserialize)]
struct ManifestNodeDoc {
    kind: String,
    #[serde(default)]
    input_mode: Option<String>,
    #[serde(default)]
    execution_mode: Option<String>,
    #[serde(default)]
    max_payload_bytes: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CursorState {
    min_seq: i64,
    max_seq: i64,
    retention_hint_sec: i64,
}

#[derive(Debug, Clone)]
struct PagingHandle {
    page_no: u64,
    closed: bool,
    owner_session: String,
    expires_at_ts: Option<i64>,
}

#[derive(Debug, Clone)]
struct PagingRequest {
    handle_id: String,
    session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StreamingJob {
    request_id: String,
    path: String,
    #[serde(default)]
    client_token: Option<String>,
    #[serde(default)]
    accepted: Option<JsonValue>,
    #[serde(default)]
    progress: Option<JsonValue>,
    terminal: JsonValue,
    stage: u8,
}

struct AppfsAdapter {
    app_id: String,
    session_id: String,
    app_dir: PathBuf,
    action_specs: Vec<ActionSpec>,
    events_path: PathBuf,
    cursor_path: PathBuf,
    replay_dir: PathBuf,
    jobs_path: PathBuf,
    cursor: CursorState,
    next_seq: i64,
    last_fingerprint_by_action: HashMap<PathBuf, ActionFingerprint>,
    pending_submit_by_action: HashMap<PathBuf, PendingSubmit>,
    blocked_actions: HashMap<PathBuf, Instant>,
    handles: HashMap<String, PagingHandle>,
    handle_aliases: HashMap<String, String>,
    streaming_jobs: Vec<StreamingJob>,
    business_adapter: Box<dyn AppAdapterV1>,
}

impl AppfsAdapter {
    fn new(
        root: PathBuf,
        app_id: String,
        session_id: String,
        adapter_http_endpoint: Option<String>,
        adapter_http_timeout_ms: u64,
    ) -> Result<Self> {
        let app_dir = root.join(&app_id);
        let manifest_path = app_dir.join("_meta").join("manifest.res.json");
        let events_path = app_dir.join("_stream").join("events.evt.jsonl");
        let cursor_path = app_dir.join("_stream").join("cursor.res.json");
        let replay_dir = app_dir.join("_stream").join("from-seq");
        let jobs_path = app_dir.join("_stream").join("inflight.jobs.res.json");

        if !app_dir.exists() {
            anyhow::bail!("App directory not found: {}", app_dir.display());
        }
        if !manifest_path.exists() {
            anyhow::bail!("Missing manifest file: {}", manifest_path.display());
        }
        if !events_path.exists() {
            anyhow::bail!("Missing events stream file: {}", events_path.display());
        }
        if !cursor_path.exists() {
            anyhow::bail!("Missing cursor file: {}", cursor_path.display());
        }
        if !replay_dir.exists() {
            anyhow::bail!("Missing replay directory: {}", replay_dir.display());
        }

        let cursor = Self::load_cursor(&cursor_path)?;
        let next_seq = cursor.max_seq + 1;
        let action_specs = Self::load_action_specs(&manifest_path)?;
        let business_adapter: Box<dyn AppAdapterV1> = if let Some(endpoint) = adapter_http_endpoint
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            eprintln!("AppFS adapter using HTTP bridge endpoint: {endpoint}");
            Box::new(HttpBridgeAdapterV1::new(
                app_id.clone(),
                endpoint.to_string(),
                Duration::from_millis(adapter_http_timeout_ms.max(1)),
            ))
        } else {
            Box::new(DemoAppAdapterV1::new(app_id.clone()))
        };
        if business_adapter.app_id() != app_id {
            anyhow::bail!(
                "Adapter app_id mismatch: adapter={} runtime={}",
                business_adapter.app_id(),
                app_id
            );
        }

        let mut adapter = Self {
            app_id,
            session_id,
            app_dir,
            action_specs,
            events_path,
            cursor_path,
            replay_dir,
            jobs_path: jobs_path.clone(),
            cursor,
            next_seq,
            last_fingerprint_by_action: HashMap::new(),
            pending_submit_by_action: HashMap::new(),
            blocked_actions: HashMap::new(),
            handles: HashMap::new(),
            handle_aliases: HashMap::new(),
            streaming_jobs: Self::load_streaming_jobs(&jobs_path)?,
            business_adapter,
        };
        adapter.load_known_handles()?;
        Ok(adapter)
    }

    fn prepare_action_sinks(&mut self) -> Result<()> {
        let actions = self.collect_action_files()?;
        for action in actions {
            #[cfg(unix)]
            {
                // Keep sink readable for adapter polling; submit cooldown enforces
                // temporary deny semantics for compatibility checks.
                let perms = fs::Permissions::from_mode(0o666);
                fs::set_permissions(&action, perms).with_context(|| {
                    format!("Failed to set write permissions on {}", action.display())
                })?;
            }

            if let Some(fp) = action_fingerprint(&action) {
                self.last_fingerprint_by_action.insert(action, fp);
            }
        }
        Ok(())
    }

    fn poll_once(&mut self) -> Result<()> {
        self.restore_blocked_actions();
        self.drain_streaming_jobs()?;

        let mut actions = self.collect_action_files()?;
        actions.sort();
        let mut seen = HashSet::new();

        for action_path in actions {
            seen.insert(action_path.clone());

            let Some(fingerprint) = action_fingerprint(&action_path) else {
                continue;
            };
            if self
                .last_fingerprint_by_action
                .get(&action_path)
                .is_some_and(|last| *last == fingerprint)
            {
                self.pending_submit_by_action.remove(&action_path);
                continue;
            }

            let now = Instant::now();
            match self.pending_submit_by_action.get(&action_path) {
                Some(pending)
                    if pending.fingerprint == fingerprint
                        && now.duration_since(pending.first_seen)
                            >= Duration::from_millis(SUBMIT_STABLE_MS) => {}
                Some(pending) if pending.fingerprint == fingerprint => {
                    continue;
                }
                _ => {
                    self.pending_submit_by_action.insert(
                        action_path.clone(),
                        PendingSubmit {
                            fingerprint,
                            first_seen: now,
                        },
                    );
                    continue;
                }
            }
            self.pending_submit_by_action.remove(&action_path);

            let payload = match fs::read_to_string(&action_path) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if payload.trim().is_empty() {
                self.last_fingerprint_by_action
                    .insert(action_path.clone(), fingerprint);
                continue;
            }

            let outcome = self.process_action(&action_path, &payload)?;
            self.last_fingerprint_by_action
                .insert(action_path.clone(), fingerprint);

            if outcome == ProcessOutcome::Submitted {
                self.enforce_submit_cooldown(&action_path);
            }
        }

        self.last_fingerprint_by_action
            .retain(|path, _| seen.contains(path));
        self.pending_submit_by_action
            .retain(|path, _| seen.contains(path));

        Ok(())
    }

    fn process_action(&mut self, action_path: &Path, payload: &str) -> Result<ProcessOutcome> {
        let rel = action_path
            .strip_prefix(&self.app_dir)
            .unwrap_or(action_path)
            .to_string_lossy()
            .replace('\\', "/");
        if !is_safe_action_rel_path(&rel) {
            eprintln!("AppFS adapter rejected unsafe action path: {rel}");
            return Ok(ProcessOutcome::Rejected);
        }

        let Some(spec) = self.find_action_spec(&rel).cloned() else {
            eprintln!("AppFS adapter ignored undeclared action path: {rel}");
            return Ok(ProcessOutcome::Rejected);
        };

        if let Err(code) = validate_payload(&spec, payload) {
            eprintln!(
                "AppFS adapter rejected action payload for {rel}: validation={code} len={}",
                payload.len()
            );
            return Ok(ProcessOutcome::Rejected);
        }

        let normalized_path = format!("/{}", rel);
        let request_id = Self::new_request_id();
        let client_token = extract_client_token(payload);

        if normalized_path == "/_paging/fetch_next.act" {
            match parse_paging_request(payload) {
                Ok(request) => {
                    if !is_handle_format_valid(&request.handle_id) {
                        eprintln!(
                            "AppFS adapter rejected invalid handle format at close-time: {}",
                            normalized_path
                        );
                        return Ok(ProcessOutcome::Rejected);
                    }
                    self.handle_fetch_next(
                        &normalized_path,
                        &request_id,
                        &request.handle_id,
                        request.session_id.as_deref(),
                        client_token,
                    )?;
                    return Ok(ProcessOutcome::Submitted);
                }
                Err(_) => {
                    // v0.1 Core expects malformed handle to fail at close-time (EINVAL)
                    // and not be accepted into the async stream lifecycle.
                    eprintln!(
                        "AppFS adapter rejected malformed paging handle at close-time: {}",
                        normalized_path
                    );
                    return Ok(ProcessOutcome::Rejected);
                }
            };
        }

        if normalized_path == "/_paging/close.act" {
            match parse_paging_request(payload) {
                Ok(request) => {
                    if !is_handle_format_valid(&request.handle_id) {
                        eprintln!(
                            "AppFS adapter rejected invalid close handle format at close-time: {}",
                            normalized_path
                        );
                        return Ok(ProcessOutcome::Rejected);
                    }
                    self.handle_close_handle(
                        &normalized_path,
                        &request_id,
                        &request.handle_id,
                        request.session_id.as_deref(),
                        client_token,
                    )?;
                    return Ok(ProcessOutcome::Submitted);
                }
                Err(_) => {
                    eprintln!(
                        "AppFS adapter rejected malformed paging close handle at close-time: {}",
                        normalized_path
                    );
                    return Ok(ProcessOutcome::Rejected);
                }
            };
        }

        let request_ctx = RequestContextV1 {
            app_id: self.app_id.clone(),
            session_id: self.session_id.clone(),
            request_id: request_id.clone(),
            client_token: client_token.clone(),
        };
        let adapter_input_mode = match spec.input_mode {
            InputMode::Text => AdapterInputModeV1::Text,
            InputMode::Json => AdapterInputModeV1::Json,
            InputMode::TextOrJson => AdapterInputModeV1::TextOrJson,
        };
        let adapter_execution_mode = match spec.execution_mode {
            ExecutionMode::Inline => AdapterExecutionModeV1::Inline,
            ExecutionMode::Streaming => AdapterExecutionModeV1::Streaming,
        };

        match self.business_adapter.submit_action(
            &normalized_path,
            payload,
            adapter_input_mode,
            adapter_execution_mode,
            &request_ctx,
        ) {
            Ok(AdapterSubmitOutcomeV1::Completed { content }) => {
                self.emit_event(
                    &normalized_path,
                    &request_id,
                    "action.completed",
                    Some(content),
                    None,
                    client_token,
                )?;
            }
            Ok(AdapterSubmitOutcomeV1::Streaming { plan }) => {
                self.enqueue_streaming_job(&normalized_path, &request_id, client_token, plan)?;
            }
            Err(AdapterErrorV1::Rejected {
                code,
                message,
                retryable,
            }) => {
                self.emit_failed_with_retryable(
                    &normalized_path,
                    &request_id,
                    &code,
                    &message,
                    retryable,
                    client_token,
                )?;
            }
            Err(AdapterErrorV1::Internal { message }) => {
                self.emit_failed_with_retryable(
                    &normalized_path,
                    &request_id,
                    ERR_INTERNAL,
                    &message,
                    true,
                    client_token,
                )?;
            }
        }
        Ok(ProcessOutcome::Submitted)
    }

    fn handle_fetch_next(
        &mut self,
        action_path: &str,
        request_id: &str,
        handle_id: &str,
        requester_session_id: Option<&str>,
        client_token: Option<String>,
    ) -> Result<()> {
        if !is_handle_format_valid(handle_id) {
            return self.emit_failed(
                action_path,
                request_id,
                ERR_INVALID_ARGUMENT,
                "invalid handle_id format",
                client_token,
            );
        }

        let handle_key = self.resolve_handle_key(handle_id);
        let (owner_session, expires_at_ts, closed) = match self.handles.get(&handle_key) {
            Some(h) => (h.owner_session.clone(), h.expires_at_ts, h.closed),
            None => {
                return self.emit_failed(
                    action_path,
                    request_id,
                    ERR_PAGER_HANDLE_NOT_FOUND,
                    "handle not found",
                    client_token,
                );
            }
        };

        let effective_session = requester_session_id.unwrap_or(self.session_id.as_str());
        if effective_session != owner_session {
            return self.emit_failed(
                action_path,
                request_id,
                ERR_PERMISSION_DENIED,
                "cross-session handle access denied",
                client_token,
            );
        }

        if expires_at_ts.is_some_and(|expiry| Utc::now().timestamp() >= expiry) {
            return self.emit_failed(
                action_path,
                request_id,
                ERR_PAGER_HANDLE_EXPIRED,
                "handle expired",
                client_token,
            );
        }

        if closed {
            return self.emit_failed(
                action_path,
                request_id,
                ERR_PAGER_HANDLE_CLOSED,
                "handle already closed",
                client_token,
            );
        }

        let handle = self
            .handles
            .get_mut(&handle_key)
            .expect("paging handle should exist after precheck");
        handle.page_no += 1;
        let page_no = handle.page_no;
        let has_more = page_no < 3;
        let request_ctx = RequestContextV1 {
            app_id: self.app_id.clone(),
            session_id: self.session_id.clone(),
            request_id: request_id.to_string(),
            client_token: client_token.clone(),
        };
        match self.business_adapter.submit_control_action(
            action_path,
            AdapterControlActionV1::PagingFetchNext {
                handle_id: handle_key.clone(),
                page_no,
                has_more,
            },
            &request_ctx,
        ) {
            Ok(AdapterControlOutcomeV1::Completed { content }) => self.emit_event(
                action_path,
                request_id,
                "action.completed",
                Some(content),
                None,
                client_token,
            ),
            Err(err) => self.emit_adapter_error(action_path, request_id, err, client_token),
        }
    }

    fn handle_close_handle(
        &mut self,
        action_path: &str,
        request_id: &str,
        handle_id: &str,
        requester_session_id: Option<&str>,
        client_token: Option<String>,
    ) -> Result<()> {
        if !is_handle_format_valid(handle_id) {
            return self.emit_failed(
                action_path,
                request_id,
                ERR_INVALID_ARGUMENT,
                "invalid handle_id format",
                client_token,
            );
        }

        let handle_key = self.resolve_handle_key(handle_id);
        let (owner_session, expires_at_ts, closed) = match self.handles.get(&handle_key) {
            Some(h) => (h.owner_session.clone(), h.expires_at_ts, h.closed),
            None => {
                return self.emit_failed(
                    action_path,
                    request_id,
                    ERR_PAGER_HANDLE_NOT_FOUND,
                    "handle not found",
                    client_token,
                );
            }
        };

        let effective_session = requester_session_id.unwrap_or(self.session_id.as_str());
        if effective_session != owner_session {
            return self.emit_failed(
                action_path,
                request_id,
                ERR_PERMISSION_DENIED,
                "cross-session handle access denied",
                client_token,
            );
        }

        if expires_at_ts.is_some_and(|expiry| Utc::now().timestamp() >= expiry) {
            return self.emit_failed(
                action_path,
                request_id,
                ERR_PAGER_HANDLE_EXPIRED,
                "handle expired",
                client_token,
            );
        }

        if closed {
            return self.emit_failed(
                action_path,
                request_id,
                ERR_PAGER_HANDLE_CLOSED,
                "handle already closed",
                client_token,
            );
        }

        let handle = self
            .handles
            .get_mut(&handle_key)
            .expect("paging handle should exist after precheck");
        handle.closed = true;
        let request_ctx = RequestContextV1 {
            app_id: self.app_id.clone(),
            session_id: self.session_id.clone(),
            request_id: request_id.to_string(),
            client_token: client_token.clone(),
        };
        match self.business_adapter.submit_control_action(
            action_path,
            AdapterControlActionV1::PagingClose {
                handle_id: handle_key,
            },
            &request_ctx,
        ) {
            Ok(AdapterControlOutcomeV1::Completed { content }) => self.emit_event(
                action_path,
                request_id,
                "action.completed",
                Some(content),
                None,
                client_token,
            ),
            Err(err) => self.emit_adapter_error(action_path, request_id, err, client_token),
        }
    }

    fn enqueue_streaming_job(
        &mut self,
        action_path: &str,
        request_id: &str,
        client_token: Option<String>,
        plan: AdapterStreamingPlanV1,
    ) -> Result<()> {
        self.streaming_jobs.push(StreamingJob {
            request_id: request_id.to_string(),
            path: action_path.to_string(),
            client_token,
            accepted: plan.accepted_content,
            progress: plan.progress_content,
            terminal: plan.terminal_content,
            stage: 0,
        });
        self.save_streaming_jobs()
    }

    fn drain_streaming_jobs(&mut self) -> Result<()> {
        if self.streaming_jobs.is_empty() {
            return Ok(());
        }

        let jobs = std::mem::take(&mut self.streaming_jobs);
        let mut next_jobs = Vec::with_capacity(jobs.len());
        for mut job in jobs {
            match job.stage {
                0 => {
                    self.emit_event(
                        &job.path,
                        &job.request_id,
                        "action.accepted",
                        Some(job.accepted.clone().unwrap_or_else(|| json!("accepted"))),
                        None,
                        job.client_token.clone(),
                    )?;
                    job.stage = 1;
                    next_jobs.push(job);
                }
                1 => {
                    self.emit_event(
                        &job.path,
                        &job.request_id,
                        "action.progress",
                        Some(
                            job.progress
                                .clone()
                                .unwrap_or_else(|| json!({ "percent": 50 })),
                        ),
                        None,
                        job.client_token.clone(),
                    )?;
                    job.stage = 2;
                    next_jobs.push(job);
                }
                _ => {
                    self.emit_event(
                        &job.path,
                        &job.request_id,
                        "action.completed",
                        Some(job.terminal),
                        None,
                        job.client_token,
                    )?;
                }
            }
        }
        self.streaming_jobs = next_jobs;
        self.save_streaming_jobs()
    }

    fn restore_blocked_actions(&mut self) {
        if self.blocked_actions.is_empty() {
            return;
        }

        let now = Instant::now();
        let mut to_unblock = Vec::new();
        for (path, until) in &self.blocked_actions {
            if now >= *until {
                to_unblock.push(path.clone());
            }
        }

        for path in to_unblock {
            #[cfg(unix)]
            {
                let perms = fs::Permissions::from_mode(0o666);
                let _ = fs::set_permissions(&path, perms);
            }
            self.blocked_actions.remove(&path);
        }
    }

    fn enforce_submit_cooldown(&mut self, action_path: &Path) {
        #[cfg(not(unix))]
        let _ = action_path;
        #[cfg(unix)]
        {
            // Short cooldown window keeps CT-002 deterministic while still allowing
            // later submissions to the same action sink.
            let perms = fs::Permissions::from_mode(0o000);
            let _ = fs::set_permissions(action_path, perms);
            self.blocked_actions.insert(
                action_path.to_path_buf(),
                Instant::now() + Duration::from_millis(ACTION_COOLDOWN_MS),
            );
        }
    }

    fn find_action_spec(&self, rel_path: &str) -> Option<&ActionSpec> {
        self.action_specs
            .iter()
            .find(|spec| action_template_matches(&spec.template, rel_path))
    }

    fn load_action_specs(manifest_path: &Path) -> Result<Vec<ActionSpec>> {
        let manifest_json = fs::read_to_string(manifest_path)
            .with_context(|| format!("Failed to read {}", manifest_path.display()))?;
        let manifest: ManifestDoc = serde_json::from_str(&manifest_json)
            .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;

        let mut specs = Vec::new();
        for (template, node) in manifest.nodes {
            if node.kind != "action" || !template.ends_with(".act") {
                continue;
            }

            let input_mode = match node.input_mode.as_deref() {
                Some("text") => InputMode::Text,
                Some("json") => InputMode::Json,
                Some("text_or_json") | None => InputMode::TextOrJson,
                Some(other) => {
                    eprintln!(
                        "AppFS adapter unknown input_mode='{other}' for action template={template}, defaulting to text_or_json"
                    );
                    InputMode::TextOrJson
                }
            };

            let execution_mode = match node.execution_mode.as_deref() {
                Some("streaming") => ExecutionMode::Streaming,
                Some("inline") | None => ExecutionMode::Inline,
                Some(other) => {
                    eprintln!(
                        "AppFS adapter unknown execution_mode='{other}' for action template={template}, defaulting to inline"
                    );
                    ExecutionMode::Inline
                }
            };

            specs.push(ActionSpec {
                template: template.trim_start_matches('/').to_string(),
                input_mode,
                execution_mode,
                max_payload_bytes: node.max_payload_bytes,
            });
        }

        if specs.is_empty() {
            eprintln!(
                "AppFS adapter warning: no action definitions found in {}",
                manifest_path.display()
            );
        }

        Ok(specs)
    }

    fn emit_failed(
        &mut self,
        action_path: &str,
        request_id: &str,
        error_code: &str,
        message: &str,
        client_token: Option<String>,
    ) -> Result<()> {
        let retryable = error_code == ERR_PAGER_HANDLE_EXPIRED;
        self.emit_failed_with_retryable(
            action_path,
            request_id,
            error_code,
            message,
            retryable,
            client_token,
        )
    }

    fn emit_failed_with_retryable(
        &mut self,
        action_path: &str,
        request_id: &str,
        error_code: &str,
        message: &str,
        retryable: bool,
        client_token: Option<String>,
    ) -> Result<()> {
        self.emit_event(
            action_path,
            request_id,
            "action.failed",
            None,
            Some(json!({
                "code": error_code,
                "message": message,
                "retryable": retryable,
            })),
            client_token,
        )
    }

    fn emit_adapter_error(
        &mut self,
        action_path: &str,
        request_id: &str,
        err: AdapterErrorV1,
        client_token: Option<String>,
    ) -> Result<()> {
        match err {
            AdapterErrorV1::Rejected {
                code,
                message,
                retryable,
            } => self.emit_failed_with_retryable(
                action_path,
                request_id,
                &code,
                &message,
                retryable,
                client_token,
            ),
            AdapterErrorV1::Internal { message } => self.emit_failed_with_retryable(
                action_path,
                request_id,
                ERR_INTERNAL,
                &message,
                true,
                client_token,
            ),
        }
    }

    fn emit_event(
        &mut self,
        action_path: &str,
        request_id: &str,
        event_type: &str,
        content: Option<JsonValue>,
        error: Option<JsonValue>,
        client_token: Option<String>,
    ) -> Result<()> {
        let seq = self.next_seq;
        self.next_seq += 1;

        let mut event = json!({
            "seq": seq,
            "event_id": format!("evt-{}", seq),
            "ts": Utc::now().to_rfc3339(),
            "app": self.app_id,
            "session_id": self.session_id,
            "request_id": request_id,
            "path": action_path,
            "type": event_type,
        });

        if let Some(content) = content {
            event["content"] = content;
        }
        if let Some(error) = error {
            event["error"] = error;
        }
        if let Some(token) = client_token {
            event["client_token"] = json!(token);
        }

        let line = serde_json::to_string(&event)?;
        self.publish_event(seq, &line)
    }

    fn publish_event(&mut self, seq: i64, line: &str) -> Result<()> {
        let mut events = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.events_path)
            .with_context(|| {
                format!("Failed to open stream file {}", self.events_path.display())
            })?;
        writeln!(events, "{line}")?;
        events.flush()?;

        let replay_file = self.replay_dir.join(format!("{seq}.evt.jsonl"));
        fs::write(&replay_file, format!("{line}\n"))
            .with_context(|| format!("Failed to write replay file {}", replay_file.display()))?;

        self.cursor.max_seq = seq;
        if self.cursor.min_seq <= 0 {
            self.cursor.min_seq = seq;
        }
        if self.cursor.retention_hint_sec <= 0 {
            self.cursor.retention_hint_sec = DEFAULT_RETENTION_HINT_SEC;
        }
        self.save_cursor()?;
        Ok(())
    }

    fn save_cursor(&self) -> Result<()> {
        let tmp_path = self.cursor_path.with_extension("res.json.tmp");
        let bytes = serde_json::to_vec_pretty(&self.cursor)?;
        fs::write(&tmp_path, bytes)
            .with_context(|| format!("Failed to write cursor temp file {}", tmp_path.display()))?;
        if self.cursor_path.exists() {
            fs::remove_file(&self.cursor_path).with_context(|| {
                format!(
                    "Failed to remove old cursor file {}",
                    self.cursor_path.display()
                )
            })?;
        }
        fs::rename(&tmp_path, &self.cursor_path).with_context(|| {
            format!(
                "Failed to move cursor temp file {} to {}",
                tmp_path.display(),
                self.cursor_path.display()
            )
        })?;
        Ok(())
    }

    fn load_cursor(path: &Path) -> Result<CursorState> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let mut cursor: CursorState = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        if cursor.retention_hint_sec <= 0 {
            cursor.retention_hint_sec = DEFAULT_RETENTION_HINT_SEC;
        }
        Ok(cursor)
    }

    fn load_streaming_jobs(path: &Path) -> Result<Vec<StreamingJob>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let jobs: Vec<StreamingJob> = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        Ok(jobs)
    }

    fn save_streaming_jobs(&self) -> Result<()> {
        let tmp_path = self.jobs_path.with_extension("res.json.tmp");
        let bytes = serde_json::to_vec_pretty(&self.streaming_jobs)?;
        fs::write(&tmp_path, bytes)
            .with_context(|| format!("Failed to write jobs temp file {}", tmp_path.display()))?;
        if self.jobs_path.exists() {
            fs::remove_file(&self.jobs_path).with_context(|| {
                format!(
                    "Failed to remove old jobs file {}",
                    self.jobs_path.display()
                )
            })?;
        }
        fs::rename(&tmp_path, &self.jobs_path).with_context(|| {
            format!(
                "Failed to move jobs temp file {} to {}",
                tmp_path.display(),
                self.jobs_path.display()
            )
        })?;
        Ok(())
    }

    fn collect_action_files(&self) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        collect_files_with_suffix(&self.app_dir, ".act", &mut out)?;
        Ok(out)
    }

    fn load_known_handles(&mut self) -> Result<()> {
        let mut resources = Vec::new();
        collect_files_with_suffix(&self.app_dir, ".res.json", &mut resources)?;
        for path in resources {
            if path.starts_with(self.app_dir.join("_stream")) {
                continue;
            }

            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let json: JsonValue = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(handle_id) = json
                .get("page")
                .and_then(|p| p.get("handle_id"))
                .and_then(|h| h.as_str())
            {
                let normalized_handle = normalize_runtime_handle_id(handle_id);
                if normalized_handle != handle_id {
                    self.handle_aliases
                        .insert(handle_id.to_string(), normalized_handle.clone());
                }
                self.handles.insert(
                    normalized_handle,
                    PagingHandle {
                        page_no: 0,
                        closed: false,
                        owner_session: json
                            .get("page")
                            .and_then(|p| p.get("session_id"))
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .unwrap_or(self.session_id.as_str())
                            .to_string(),
                        expires_at_ts: json
                            .get("page")
                            .and_then(|p| p.get("expires_at"))
                            .and_then(|v| v.as_str())
                            .and_then(parse_rfc3339_timestamp),
                    },
                );
            }
        }
        Ok(())
    }

    fn resolve_handle_key(&self, requested: &str) -> String {
        if let Some(alias) = self.handle_aliases.get(requested) {
            return alias.clone();
        }
        let normalized = normalize_runtime_handle_id(requested);
        if self.handles.contains_key(&normalized) {
            return normalized;
        }
        requested.to_string()
    }

    fn new_request_id() -> String {
        let uuid = Uuid::new_v4().simple().to_string();
        format!("req-{}", &uuid[..8])
    }
}

fn collect_files_with_suffix(dir: &Path, suffix: &str, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("Failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files_with_suffix(&path, suffix, out)?;
            continue;
        }

        if file_type.is_file()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.ends_with(suffix))
        {
            out.push(path);
        }
    }
    Ok(())
}

fn action_fingerprint(path: &Path) -> Option<ActionFingerprint> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta
        .modified()
        .unwrap_or(UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some(ActionFingerprint {
        len: meta.len(),
        modified_ns: modified,
    })
}

fn validate_payload(spec: &ActionSpec, payload: &str) -> std::result::Result<(), &'static str> {
    if let Some(max) = spec.max_payload_bytes {
        if payload.len() > max {
            return Err("EMSGSIZE");
        }
    }

    if payload.trim().is_empty() {
        return Err(ERR_INVALID_ARGUMENT);
    }

    match spec.input_mode {
        InputMode::Text => validate_text_payload_complete(payload),
        InputMode::Json => {
            serde_json::from_str::<JsonValue>(payload).map_err(|_| ERR_INVALID_PAYLOAD)?;
            Ok(())
        }
        InputMode::TextOrJson => {
            let trimmed = payload.trim_start();
            if trimmed.starts_with('{') || trimmed.starts_with('[') {
                serde_json::from_str::<JsonValue>(payload).map_err(|_| ERR_INVALID_PAYLOAD)?;
            } else {
                validate_text_payload_complete(payload)?;
            }
            Ok(())
        }
    }
}

fn validate_text_payload_complete(payload: &str) -> std::result::Result<(), &'static str> {
    if !payload.ends_with('\n') {
        return Err(ERR_INVALID_PAYLOAD);
    }
    Ok(())
}

fn action_template_matches(template: &str, rel_path: &str) -> bool {
    let template = template.trim_matches('/');
    let rel_path = rel_path.trim_matches('/');
    if template.is_empty() || rel_path.is_empty() {
        return false;
    }

    let template_segments: Vec<&str> = template.split('/').collect();
    let rel_segments: Vec<&str> = rel_path.split('/').collect();
    if template_segments.len() != rel_segments.len() {
        return false;
    }

    template_segments
        .iter()
        .zip(rel_segments.iter())
        .all(|(t, r)| {
            if is_template_placeholder(t) {
                !r.is_empty()
            } else {
                *t == *r
            }
        })
}

fn is_template_placeholder(segment: &str) -> bool {
    segment.len() >= 3 && segment.starts_with('{') && segment.ends_with('}')
}

fn is_safe_action_rel_path(rel_path: &str) -> bool {
    let path = rel_path.trim_matches('/');
    if path.is_empty() {
        return false;
    }

    path.split('/').all(is_safe_segment)
}

fn is_safe_segment(segment: &str) -> bool {
    if segment.is_empty() || segment == "." || segment == ".." {
        return false;
    }
    if segment.contains('\\') || segment.contains('\0') {
        return false;
    }
    if is_drive_letter_segment(segment) {
        return false;
    }
    if is_windows_reserved_name(segment) {
        return false;
    }
    if segment.as_bytes().len() > MAX_SEGMENT_BYTES {
        return false;
    }

    segment.chars().all(|c| ALLOWED_SEGMENT_CHARS.contains(c))
}

fn is_drive_letter_segment(segment: &str) -> bool {
    segment.len() >= 2
        && segment.as_bytes()[0].is_ascii_alphabetic()
        && segment.as_bytes()[1] == b':'
}

fn is_windows_reserved_name(segment: &str) -> bool {
    let upper = segment.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn extract_client_token(payload: &str) -> Option<String> {
    if let Ok(json) = serde_json::from_str::<JsonValue>(payload) {
        return json
            .get("client_token")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned);
    }

    let first_line = payload.lines().next()?.trim();
    first_line
        .strip_prefix("token:")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_paging_request(payload: &str) -> std::result::Result<PagingRequest, &'static str> {
    let text = payload.trim();
    if text.is_empty() {
        return Err(ERR_INVALID_ARGUMENT);
    }

    if text.starts_with('{') {
        let json = serde_json::from_str::<JsonValue>(text).map_err(|_| ERR_INVALID_ARGUMENT)?;
        let handle_id = json
            .get("handle_id")
            .and_then(|v| v.as_str())
            .ok_or(ERR_INVALID_ARGUMENT)?;
        let session_id = json
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);
        return Ok(PagingRequest {
            handle_id: handle_id.trim().to_string(),
            session_id,
        });
    }

    Ok(PagingRequest {
        handle_id: text.lines().next().unwrap_or("").trim().to_string(),
        session_id: None,
    })
}

fn parse_rfc3339_timestamp(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.timestamp())
}

fn normalize_runtime_handle_id(handle_id: &str) -> String {
    deterministic_shorten_segment(handle_id, MAX_SEGMENT_BYTES)
}

fn deterministic_shorten_segment(segment: &str, max_bytes: usize) -> String {
    if segment.as_bytes().len() <= max_bytes {
        return segment.to_string();
    }

    let hash = format!("{:016x}", fnv1a_64(segment.as_bytes()));
    let suffix = format!("_{}", hash);
    let prefix_budget = max_bytes.saturating_sub(suffix.len());

    let mut prefix = String::new();
    let mut used = 0usize;
    for ch in segment.chars() {
        let ch_len = ch.len_utf8();
        if used + ch_len > prefix_budget {
            break;
        }
        prefix.push(ch);
        used += ch_len;
    }

    if prefix.is_empty() {
        return hash;
    }

    prefix.push_str(&suffix);
    prefix
}

fn fnv1a_64(input: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;

    let mut hash = OFFSET_BASIS;
    for byte in input {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

fn is_handle_format_valid(handle_id: &str) -> bool {
    if !handle_id.starts_with("ph_") {
        return false;
    }
    handle_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

#[cfg(test)]
mod tests {
    use super::{
        deterministic_shorten_segment, extract_client_token, is_handle_format_valid,
        normalize_runtime_handle_id, parse_paging_request, MAX_SEGMENT_BYTES,
    };

    #[test]
    fn parse_handle_text_mode() {
        let req = parse_paging_request("ph_7f2c\n").expect("expected handle");
        assert_eq!(req.handle_id, "ph_7f2c");
        assert_eq!(req.session_id, None);
    }

    #[test]
    fn parse_handle_json_mode() {
        let req = parse_paging_request(r#"{"handle_id":"ph_abc"}"#).expect("expected handle");
        assert_eq!(req.handle_id, "ph_abc");
        assert_eq!(req.session_id, None);
    }

    #[test]
    fn parse_handle_json_with_session_mode() {
        let req = parse_paging_request(r#"{"handle_id":"ph_abc","session_id":"sess-other"}"#)
            .expect("expected handle");
        assert_eq!(req.handle_id, "ph_abc");
        assert_eq!(req.session_id.as_deref(), Some("sess-other"));
    }

    #[test]
    fn extract_token_from_text() {
        let token = extract_client_token("token:msg-001\nhello").expect("token missing");
        assert_eq!(token, "msg-001");
    }

    #[test]
    fn extract_token_from_json() {
        let token = extract_client_token(r#"{"client_token":"x-1"}"#).expect("token missing");
        assert_eq!(token, "x-1");
    }

    #[test]
    fn handle_format_validation() {
        assert!(is_handle_format_valid("ph_7f2c"));
        assert!(!is_handle_format_valid("bad/handle"));
    }

    #[test]
    fn normalize_runtime_handle_id_keeps_short_value() {
        let handle = "ph_short_handle";
        assert_eq!(normalize_runtime_handle_id(handle), handle);
    }

    #[test]
    fn deterministic_shorten_is_bounded_and_stable() {
        let long_handle = format!("ph_{}", "a".repeat(500));
        let shortened_a = deterministic_shorten_segment(&long_handle, MAX_SEGMENT_BYTES);
        let shortened_b = deterministic_shorten_segment(&long_handle, MAX_SEGMENT_BYTES);

        assert_eq!(shortened_a, shortened_b);
        assert!(shortened_a.starts_with("ph_"));
        assert!(shortened_a.as_bytes().len() <= MAX_SEGMENT_BYTES);
    }
}
