//! Single-connection async bridge client (port of the POC `BridgeClient`).
//!
//! Rules kept from the POC:
//! * discovery is re-read on every (re)connect — the token rotates per bridge load;
//! * `control.hello` is the first frame; protocol major / expected build are checked there;
//! * every request carries an explicit `deadlineMs` inside `params` (never 0);
//! * timeout classification: `-32008` -> `QueueTimeout` (never executed), no reply within
//!   deadline + grace -> `OutcomeUnknown`, socket closed -> `ConnectionLost` (both close the socket);
//! * the modal gate probes `control.ping` before any non-`control.*` method and fails closed;
//! * `control.ping.revision` changes re-fetch `control.capabilities`.
//!
//! One instance == one client identity; callers serialise access (see [`SharedBridge`]).
use super::discovery::{pid_alive, Discovery};
use super::errors::{BridgeError, Kind};
use super::protocol::*;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The servers hold the client behind an async mutex: one in-flight request per process, which
/// is also what the bridge enforces (a second socket gets `Busy`).
pub type SharedBridge = Arc<tokio::sync::Mutex<BridgeClient>>;

#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub discovery_path: PathBuf,
    pub deadline_ms: u64,
    pub connect_timeout_ms: u64,
    pub reply_grace_ms: u64,
    pub blocked_age_ms: u64,
    pub expected_protocol: u32,
    pub expected_build: Option<String>,
    pub client_name: &'static str,
    pub client_version: &'static str,
    pub block_when_stale: bool,
    pub busy_retries: u32,
    pub busy_retry_ms: u64,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            discovery_path: PathBuf::from("session.json"),
            deadline_ms: DEFAULT_DEADLINE_MS,
            connect_timeout_ms: DEFAULT_CONNECT_TIMEOUT_MS,
            reply_grace_ms: DEFAULT_REPLY_GRACE_MS,
            blocked_age_ms: DEFAULT_BLOCKED_AGE_MS,
            expected_protocol: PROTOCOL,
            expected_build: None,
            client_name: "cs2-mcp",
            client_version: env!("CARGO_PKG_VERSION"),
            block_when_stale: true,
            busy_retries: 8,
            busy_retry_ms: 250,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RequestOpts {
    /// per-request deadline; default `ClientOptions::deadline_ms`
    pub deadline_ms: Option<u64>,
    /// `expectDoc` for mutating methods
    pub expect_doc: Option<Value>,
    /// override the liveness gate (default: `block_when_stale` for non-`control.*` methods)
    pub gate: Option<bool>,
}

impl RequestOpts {
    pub fn deadline(ms: u64) -> Self {
        RequestOpts {
            deadline_ms: Some(ms),
            ..Default::default()
        }
    }
    pub fn ungated(ms: u64) -> Self {
        RequestOpts {
            deadline_ms: Some(ms),
            gate: Some(false),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub id: String,
    pub method: String,
    pub result: Value,
    pub bridge: Value,
    pub raw: String,
}

impl Response {
    pub fn build(&self) -> Option<&str> {
        self.bridge.get("build").and_then(Value::as_str)
    }
    pub fn protocol(&self) -> Option<i64> {
        self.bridge.get("protocol").and_then(Value::as_i64)
    }
}

/// Result of a `control.ping` probe (never an error: `unknown` + `note` instead).
#[derive(Debug, Clone, Default)]
pub struct Liveness {
    pub ok: bool,
    pub main_thread_age_ms: Option<u64>,
    pub drain_blocked: Option<bool>,
    pub queue_depth: Option<i64>,
    pub shutting_down: Option<bool>,
    /// modal/in-progress stall: queued work would never run
    pub hammer_blocked: bool,
    /// a `map.run`/compile job is loading (frame loop frozen; not a dialog)
    pub engine_busy: bool,
    /// The `engine_busy` came from an **external** job's grace (`note_external_job`): the bridge
    /// lists no job for it, `control.status` cannot confirm it, and the caller may want to tell
    /// this window apart from a bridge job (review-2 N4).
    pub external_job: bool,
    /// How much of the external grace is left (`ms`): a caller that keeps hitting the gate can
    /// tell how much longer this window excuses a frozen frame loop.
    pub external_grace_remaining_ms: Option<u64>,
    /// liveness could not be verified: callers fail closed before dispatch
    pub unknown: bool,
    pub inflight: Option<Value>,
    pub revision: Option<Value>,
    pub modules: Option<Value>,
    pub threshold_ms: u64,
    pub note: Option<String>,
}

impl Liveness {
    pub fn to_json(&self) -> Value {
        json!({
            "ok": self.ok, "mainThreadAgeMs": self.main_thread_age_ms,
            "drainBlocked": self.drain_blocked, "queueDepth": self.queue_depth,
            "shuttingDown": self.shutting_down, "hammerBlocked": self.hammer_blocked,
            "engineBusy": self.engine_busy,
            "externalJob": self.external_job,
            "externalGraceRemainingMs": self.external_grace_remaining_ms,
            "unknown": self.unknown,
            "inflight": self.inflight, "revision": self.revision, "modules": self.modules,
            "hammerBlockedAgeMs": self.threshold_ms, "note": self.note,
        })
    }
}

enum ReadOutcome {
    Line(String),
    Timeout,
}

pub struct BridgeClient {
    pub opts: ClientOptions,
    pub discovery: Option<Discovery>,
    pub hello_result: Option<Value>,
    pub capabilities: Map<String, Value>,
    /// last 64 server notifications (id-less frames with a method)
    pub notifications: Vec<Value>,
    pub max_line_bytes: usize,
    stream: Option<TcpStream>,
    buf: Vec<u8>,
    /// heap read scratch: keeps the request futures small (a stack array here blew test stacks)
    scratch: Vec<u8>,
    caps_revision: Option<Value>,
    connects: u32,
    job_grace_until: Option<Instant>,
    /// The grace was claimed by [`BridgeClient::note_external_job`]: a job the *bridge* does not
    /// know about (`run_map`'s console load), so `control.status` will never list it and the
    /// "no running job" reset in [`BridgeClient::liveness`] must not clear it (review-1 L1).
    ///
    /// It is kept SEPARATE from the bridge-job grace (review-2 N4): `clear_external_job` must
    /// release exactly the external window it claimed, never a concurrent bridge job's grace —
    /// the old single-field shape let the release wipe `note_job`'s grace as a side effect.
    external_grace_until: Option<Instant>,
    shutting_down: bool,
    id_seq: u64,
}

impl BridgeClient {
    pub fn new(opts: ClientOptions) -> Self {
        BridgeClient {
            opts,
            discovery: None,
            hello_result: None,
            capabilities: Map::new(),
            notifications: Vec::new(),
            max_line_bytes: MAX_LINE_BYTES,
            stream: None,
            buf: Vec::new(),
            scratch: vec![0u8; 65536],
            caps_revision: None,
            connects: 0,
            job_grace_until: None,
            external_grace_until: None,
            shutting_down: false,
            id_seq: 0,
        }
    }

    pub fn shared(opts: ClientOptions) -> SharedBridge {
        Arc::new(tokio::sync::Mutex::new(Self::new(opts)))
    }

    // -- small helpers ---------------------------------------------------------------------

    pub fn connected(&self) -> bool {
        self.stream.is_some()
    }
    pub fn connects(&self) -> u32 {
        self.connects
    }
    pub fn discovery_path(&self) -> &PathBuf {
        &self.opts.discovery_path
    }
    pub fn has(&self, method: &str) -> bool {
        self.capabilities
            .get("methods")
            .and_then(Value::as_array)
            .map(|a| a.iter().any(|m| m.as_str() == Some(method)))
            .unwrap_or(false)
    }
    /// `control.capabilities.modules[name].state` (`verified`, `not_loaded`, `verifying`, ...),
    /// when the bridge reports it.
    pub fn module_state(&self, name: &str) -> Option<String> {
        self.capabilities
            .get("modules")?
            .get(name)?
            .get("state")?
            .as_str()
            .map(str::to_string)
    }
    pub fn missing(&self, methods: &[&str]) -> Vec<String> {
        methods
            .iter()
            .filter(|m| !self.has(m))
            .map(|m| m.to_string())
            .collect()
    }
    pub fn features(&self) -> Map<String, Value> {
        self.capabilities
            .get("features")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    }
    pub fn limits(&self) -> Map<String, Value> {
        self.capabilities
            .get("limits")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    }
    pub fn caps_revision(&self) -> Option<&Value> {
        self.caps_revision.as_ref()
    }
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down
    }

    /// Tell the blocked gate that a long job (`map.run`/compile) is running: the engine map load
    /// freezes the frame loop and must not be reported as "a dialog is up" (WORKFLOW §11).
    ///
    /// The grace is only kept while `control.status` still reports a `running` job
    /// ([`BridgeClient::liveness`]) — for a job the bridge runs. A job the bridge knows nothing
    /// about takes [`BridgeClient::note_external_job`].
    pub fn note_job(&mut self, grace: Duration) {
        let until = Instant::now() + grace;
        self.job_grace_until = Some(self.job_grace_until.map_or(until, |u| u.max(until)));
    }

    /// Like [`BridgeClient::note_job`], for a job that is **not** a bridge job: `run_map`'s engine
    /// load goes over the VToolMessage console channel, so `control.status` lists no `running` job
    /// and [`BridgeClient::liveness`] would clear the grace as "the job finished" — the frozen
    /// frame loop would then be reported as `HammerBlocked` ("a modal dialog owns the tools main
    /// thread") while the engine is actually loading (review-1 L1). The grace is trusted until it
    /// expires instead; the caller releases it with
    /// [`BridgeClient::clear_external_job`] as soon as its load settles.
    ///
    /// The external window is tracked apart from a bridge job's grace (review-2 N4): one field
    /// per claimant, so neither release touches the other's grace.
    pub fn note_external_job(&mut self, grace: Duration) {
        let until = Instant::now() + grace;
        self.external_grace_until = Some(self.external_grace_until.map_or(until, |u| u.max(until)));
    }

    /// Release the grace [`BridgeClient::note_external_job`] claimed (the external load either
    /// finished or is not being watched any more). A bridge job's grace is left alone: it is a
    /// separate field, and only its own expiry or a running-listed job clears it.
    pub fn clear_external_job(&mut self) {
        self.external_grace_until = None;
    }

    fn new_id(&mut self, prefix: &str) -> String {
        self.id_seq += 1;
        format!("{prefix}{}", self.id_seq)
    }

    // -- connection lifecycle --------------------------------------------------------------

    /// (Re)read discovery, connect, run `control.hello`. Retries briefly on `Busy` (the bridge may
    /// still count a just-dropped connection as the peer) and once on `NotAuthenticated` (the
    /// discovery file may have been rewritten between two reads).
    pub async fn connect(&mut self, allow_reconnect: bool) -> Result<Discovery, BridgeError> {
        if self.stream.is_some() {
            if !allow_reconnect {
                return Ok(self
                    .discovery
                    .clone()
                    .expect("discovery set while connected"));
            }
            self.hard_close("reconnect");
        }
        if self.shutting_down {
            return Err(BridgeError::new(
                Kind::ShuttingDown,
                "bridge is shutting down; not reconnecting",
            ));
        }
        let mut unauth_tries = 0u32;
        let mut busy_tries = 0u32;
        loop {
            let disc = Discovery::load(&self.opts.discovery_path)?;
            if disc.transport != "tcp" {
                return Err(BridgeError::new(
                    Kind::BridgeNotRunning,
                    format!("discovery transport {:?} is not supported (loopback TCP is the reference binding)", disc.transport),
                )
                .with_data(disc.to_json()));
            }
            if disc.port == 0 || disc.host.is_empty() {
                return Err(BridgeError::new(
                    Kind::BridgeNotRunning,
                    "discovery file has no endpoint",
                )
                .with_data(disc.to_json()));
            }
            let connect = TcpStream::connect((disc.host.as_str(), disc.port));
            let sock = match tokio::time::timeout(
                Duration::from_millis(self.opts.connect_timeout_ms),
                connect,
            )
            .await
            {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    return Err(Self::connect_error(&disc, &e.to_string(), e.raw_os_error()))
                }
                Err(_) => return Err(Self::connect_error(&disc, "connect timed out", None)),
            };
            let _ = sock.set_nodelay(true);
            self.stream = Some(sock);
            self.buf.clear();
            self.discovery = Some(disc.clone());
            self.connects += 1;
            tracing::info!(host = %disc.host, port = disc.port, bridge = ?disc.bridge_version, pid = ?disc.pid, "bridge: connected");
            match self.hello(&disc.token).await {
                Ok(_) => return Ok(disc),
                Err(e) if e.kind == Kind::NotAuthenticated => {
                    self.hard_close("authentication refused");
                    unauth_tries += 1;
                    if unauth_tries >= 2 {
                        return Err(BridgeError::new(
                            Kind::BridgeNotRunning,
                            "authentication refused twice with fresh discovery reads (stale token / foreign client)",
                        )
                        .with_data(disc.to_json()));
                    }
                }
                Err(e) if e.kind == Kind::Busy => {
                    self.hard_close("busy");
                    busy_tries += 1;
                    if busy_tries > self.opts.busy_retries {
                        return Err(e);
                    }
                    tokio::time::sleep(Duration::from_millis(self.opts.busy_retry_ms)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn connect_error(disc: &Discovery, detail: &str, errno: Option<i32>) -> BridgeError {
        let alive = pid_alive(disc.pid);
        let hint = if alive == Some(false) {
            "stale discovery file (bridge process gone; Hammer may have closed without deleting it)"
        } else {
            "bridge process not reachable (still loading? wrong network namespace?)"
        };
        let mut data = disc.to_json();
        data["pidAlive"] = json!(alive);
        data["hint"] = json!(hint);
        data["errno"] = json!(errno);
        BridgeError::new(
            Kind::BridgeNotRunning,
            format!("cannot connect to {}:{}: {detail}", disc.host, disc.port),
        )
        .with_data(data)
    }

    pub async fn ensure_connected(&mut self) -> Result<Discovery, BridgeError> {
        if self.stream.is_none() {
            return self.connect(false).await;
        }
        Ok(self
            .discovery
            .clone()
            .expect("discovery set while connected"))
    }

    /// Clean close: no RPC (v1 has no client goodbye).
    pub fn close(&mut self) {
        self.hard_close("client close");
    }

    fn hard_close(&mut self, reason: &str) {
        if self.stream.take().is_some() {
            tracing::debug!(reason, "bridge: connection closed");
        }
        self.buf.clear();
    }

    // -- wire I/O --------------------------------------------------------------------------

    async fn read_line(&mut self, deadline_at: Instant) -> Result<ReadOutcome, BridgeError> {
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                let line = &line[..line.len() - 1];
                return String::from_utf8(line.to_vec())
                    .map(ReadOutcome::Line)
                    .map_err(|e| {
                        BridgeError::new(Kind::ProtocolError, format!("non-UTF-8 frame: {e}"))
                    });
            }
            if self.buf.len() > self.max_line_bytes {
                return Err(BridgeError::new(
                    Kind::ProtocolError,
                    "inbound frame exceeds max line length",
                ));
            }
            let remaining = deadline_at.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(ReadOutcome::Timeout);
            }
            let stream = self
                .stream
                .as_mut()
                .ok_or_else(|| BridgeError::new(Kind::ConnectionLost, "not connected"))?;
            let n = match tokio::time::timeout(remaining, stream.read(&mut self.scratch[..])).await
            {
                Err(_) => return Ok(ReadOutcome::Timeout),
                Ok(Ok(0)) => {
                    return Err(BridgeError::new(
                        Kind::ConnectionLost,
                        "bridge closed the connection while a request was in flight",
                    ))
                }
                Ok(Ok(n)) => n,
                Ok(Err(e)) => {
                    return Err(BridgeError::new(
                        Kind::ConnectionLost,
                        format!("socket error while reading: {e}"),
                    ))
                }
            };
            self.buf.extend_from_slice(&self.scratch[..n]);
        }
    }

    async fn send_frame(&mut self, obj: &Value) -> Result<(), BridgeError> {
        let mut data = serde_json::to_vec(obj)
            .map_err(|e| BridgeError::new(Kind::ProtocolError, e.to_string()))?;
        data.push(b'\n');
        if data.len() > self.max_line_bytes {
            return Err(BridgeError::new(
                Kind::ProtocolError,
                "outbound frame exceeds max line length",
            ));
        }
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| BridgeError::new(Kind::ConnectionLost, "not connected"))?;
        if let Err(e) = stream.write_all(&data).await {
            // a partial send cannot be told apart from a complete one: outcome unknown
            self.hard_close("send failed");
            return Err(BridgeError::new(
                Kind::ConnectionLost,
                format!("send failed: {e}"),
            ));
        }
        Ok(())
    }

    /// One request/response round trip on the current connection.
    async fn exchange(
        &mut self,
        method: &str,
        mut params: Map<String, Value>,
        deadline_ms: u64,
        prefix: &str,
    ) -> Result<Response, BridgeError> {
        if self.stream.is_none() {
            return Err(BridgeError::new(Kind::ConnectionLost, "not connected"));
        }
        let req_id = self.new_id(prefix);
        params
            .entry("deadlineMs")
            .or_insert_with(|| json!(deadline_ms));
        let req = json!({"jsonrpc": "2.0", "id": req_id, "method": method, "params": params, "bridge": {"protocol": PROTOCOL}});
        self.send_frame(&req).await?;
        let deadline_at =
            Instant::now() + Duration::from_millis(deadline_ms + self.opts.reply_grace_ms);
        loop {
            let raw = match self.read_line(deadline_at).await {
                Ok(ReadOutcome::Line(l)) => l,
                Ok(ReadOutcome::Timeout) => {
                    self.hard_close("reply deadline exceeded");
                    return Err(BridgeError::new(
                        Kind::OutcomeUnknown,
                        format!(
                            "no reply to {method:?} within {deadline_ms} ms deadline + {} ms grace; the request may have been executed — do NOT retry, verify by save + parse",
                            self.opts.reply_grace_ms
                        ),
                    )
                    .with_data(json!({"method": method, "deadlineMs": deadline_ms, "graceMs": self.opts.reply_grace_ms})));
                }
                Err(e) if e.kind == Kind::ConnectionLost => {
                    self.hard_close("connection lost");
                    return Err(BridgeError::new(
                        Kind::ConnectionLost,
                        format!(
                            "connection lost while {method:?} was in flight: {}",
                            e.message
                        ),
                    )
                    .with_data(json!({"method": method})));
                }
                Err(e) => {
                    self.hard_close("protocol error");
                    return Err(e);
                }
            };
            if raw.trim().is_empty() {
                continue;
            }
            let obj: Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(e) => {
                    self.hard_close("protocol error");
                    let snippet: String = raw.chars().take(200).collect();
                    return Err(BridgeError::new(
                        Kind::ProtocolError,
                        format!("unparseable frame from bridge: {snippet:?} ({e})"),
                    ));
                }
            };
            if !obj.is_object() {
                self.hard_close("protocol error");
                return Err(BridgeError::new(
                    Kind::ProtocolError,
                    "bridge frame is not a JSON object",
                ));
            }
            let id = obj.get("id").cloned().unwrap_or(Value::Null);
            let has_reply = obj.get("result").is_some() || obj.get("error").is_some();
            if id.as_str() == Some(req_id.as_str()) && has_reply {
                return Self::decode_response(method, &req_id, obj, raw);
            }
            if id.is_null() && obj.get("error").is_some() {
                // accept-time / connection-level error (e.g. Busy for a second client); the bridge closes
                self.hard_close("out-of-band error frame");
                return Self::decode_response(method, &req_id, obj, raw);
            }
            if !id.is_null() {
                self.hard_close("protocol error");
                return Err(BridgeError::new(
                    Kind::ProtocolError,
                    format!("unexpected response id {id} while waiting for {req_id:?}"),
                ));
            }
            self.on_notification(obj);
        }
    }

    fn decode_response(
        method: &str,
        req_id: &str,
        obj: Value,
        raw: String,
    ) -> Result<Response, BridgeError> {
        if let Some(err) = obj.get("error") {
            return Err(BridgeError::from_wire(method, err));
        }
        let result = obj.get("result").cloned();
        match result {
            None | Some(Value::Null) => Err(BridgeError::new(
                Kind::ProtocolError,
                format!("response without result for {method:?}"),
            )),
            Some(result) => Ok(Response {
                id: req_id.to_string(),
                method: method.to_string(),
                result,
                bridge: obj.get("bridge").cloned().unwrap_or_else(|| json!({})),
                raw,
            }),
        }
    }

    fn on_notification(&mut self, obj: Value) {
        if obj.get("method").and_then(Value::as_str) == Some("control.shuttingDown") {
            self.shutting_down = true;
        }
        self.notifications.push(obj);
        if self.notifications.len() > 64 {
            let drop_n = self.notifications.len() - 64;
            self.notifications.drain(..drop_n);
        }
    }

    async fn hello(&mut self, token: &str) -> Result<Value, BridgeError> {
        let mut params = Map::new();
        params.insert("protocol".into(), json!(PROTOCOL));
        params.insert("token".into(), json!(token));
        params.insert(
            "client".into(),
            json!({"name": self.opts.client_name, "version": self.opts.client_version}),
        );
        params.insert("deadlineMs".into(), json!(5000));
        let resp = self.exchange("control.hello", params, 5000, "h").await?;
        let result = resp.result.as_object().cloned().unwrap_or_default();
        let proto = result.get("protocol").and_then(Value::as_u64);
        if proto != Some(self.opts.expected_protocol as u64) {
            self.hard_close("protocol mismatch");
            return Err(BridgeError::new(
                Kind::BuildMismatch,
                format!(
                    "bridge protocol {proto:?} != expected {}",
                    self.opts.expected_protocol
                ),
            )
            .with_data(json!({"protocol": proto, "expected": self.opts.expected_protocol})));
        }
        let build = result.get("gameBuild").map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        });
        if let Some(expected) = self.opts.expected_build.clone() {
            if build.as_deref() != Some(expected.as_str()) {
                self.hard_close("game build mismatch");
                return Err(BridgeError::new(
                    Kind::BuildMismatch,
                    format!("game build {build:?} != expected {expected:?}"),
                )
                .with_data(json!({"gameBuild": build, "expected": expected})));
            }
        }
        self.capabilities = result
            .get("capabilities")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        self.caps_revision = self.capabilities.get("revision").cloned();
        let advertised = self
            .limits()
            .get("maxLineBytes")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        self.max_line_bytes = if advertised > 0 {
            advertised.min(MAX_LINE_BYTES)
        } else {
            MAX_LINE_BYTES
        };
        self.shutting_down = result
            .get("shuttingDown")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let n_methods = self
            .capabilities
            .get("methods")
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        tracing::info!(build = ?resp.build(), methods = n_methods, max_line_bytes = self.max_line_bytes, "bridge: hello ok");
        let value = Value::Object(result);
        self.hello_result = Some(value.clone());
        Ok(value)
    }

    // -- request API -------------------------------------------------------------------------

    /// Send one request and return the decoded response. `deadlineMs` is always explicit (never 0).
    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
        opts: RequestOpts,
    ) -> Result<Response, BridgeError> {
        self.ensure_connected().await?;
        let mut p = match params {
            Value::Object(m) => m,
            Value::Null => Map::new(),
            other => {
                return Err(BridgeError::new(
                    Kind::ProtocolError,
                    format!("params must be an object, got {other}"),
                ))
            }
        };
        if let Some(doc) = opts.expect_doc {
            p.insert("expectDoc".into(), doc);
        }
        let timeout_ms = opts.deadline_ms.unwrap_or(self.opts.deadline_ms);
        if timeout_ms == 0 {
            return Err(BridgeError::new(
                Kind::ProtocolError,
                "deadlineMs must be > 0 (0 = no deadline is not used)",
            ));
        }
        p.insert("deadlineMs".into(), json!(timeout_ms));
        let is_control = method.starts_with("control.");
        let do_gate = opts.gate.unwrap_or(self.opts.block_when_stale);
        if do_gate && !is_control {
            self.ensure_not_blocked().await?;
        }
        self.exchange(method, p, timeout_ms, "c").await
    }

    /// `request()` sugar returning just the result.
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, BridgeError> {
        Ok(self
            .request(method, params, RequestOpts::default())
            .await?
            .result)
    }

    // -- control conveniences ----------------------------------------------------------------

    /// Ungated `control.*` round trip (never recurses into the liveness gate).
    async fn control_exchange(
        &mut self,
        method: &str,
        deadline_ms: u64,
    ) -> Result<Value, BridgeError> {
        self.ensure_connected().await?;
        let mut p = Map::new();
        p.insert("deadlineMs".into(), json!(deadline_ms));
        Ok(self.exchange(method, p, deadline_ms, "c").await?.result)
    }
    pub async fn ping(&mut self) -> Result<Value, BridgeError> {
        self.control_exchange("control.ping", 5000).await
    }
    pub async fn status(&mut self) -> Result<Value, BridgeError> {
        self.control_exchange("control.status", 5000).await
    }
    pub async fn capabilities(&mut self) -> Result<Value, BridgeError> {
        self.control_exchange("control.capabilities", 5000).await
    }
    pub async fn version(&mut self) -> Result<Value, BridgeError> {
        self.control_exchange("control.version", 5000).await
    }
    pub async fn doc_active(&mut self, deadline_ms: Option<u64>) -> Result<Value, BridgeError> {
        Ok(self
            .request(
                "doc.active",
                json!({}),
                RequestOpts {
                    deadline_ms,
                    ..Default::default()
                },
            )
            .await?
            .result)
    }

    /// Re-fetch `control.capabilities` (the bridge recomputes the advertised set as module identity
    /// checks finish and bumps `revision`).
    pub async fn refresh_capabilities(&mut self) -> Result<Map<String, Value>, BridgeError> {
        let res = self.capabilities().await?;
        self.capabilities = res.as_object().cloned().unwrap_or_default();
        self.caps_revision = self.capabilities.get("revision").cloned();
        Ok(self.capabilities.clone())
    }

    // -- liveness / modal gate ---------------------------------------------------------------

    /// Any grace that still holds a frozen frame loop excused: a bridge job's, or an external
    /// job's ([`BridgeClient::note_external_job`]). Returns `(excused, external?, remaining_ms)`.
    /// A bridge job's claim is reported FIRST (flag honesty when both graces are alive: the
    /// external flag should say so only when the excuse is really the external window).
    fn recent_job_full(&mut self) -> (bool, bool, Option<u64>) {
        let now = Instant::now();
        if let Some(u) = self.job_grace_until {
            if now < u {
                return (true, false, None);
            }
            // expired: forget it, so a later `note_job` starts from a clean slate
            self.job_grace_until = None;
        }
        if let Some(u) = self.external_grace_until {
            if now < u {
                return (
                    true,
                    true,
                    Some(u.saturating_duration_since(now).as_millis() as u64),
                );
            }
            // expired: forget it, so a later `note_external_job` starts from a clean slate
            self.external_grace_until = None;
        }
        (false, false, None)
    }

    async fn job_states(&mut self) -> Option<Vec<Value>> {
        let res = self.control_exchange("control.status", 3000).await.ok()?;
        res.get("jobs").and_then(Value::as_array).cloned()
    }

    /// Probe `control.ping`; never fails (returns `unknown` + a note instead).
    pub async fn liveness(&mut self) -> Liveness {
        let threshold = self.opts.blocked_age_ms;
        let mut out = Liveness {
            threshold_ms: threshold,
            ..Default::default()
        };
        if self.stream.is_none() {
            out.unknown = true;
            out.note = Some("not connected".into());
            return out;
        }
        if self.shutting_down {
            out.ok = true;
            out.shutting_down = Some(true);
            out.note = Some("bridge is shutting down".into());
            return out;
        }
        let res = match self.control_exchange("control.ping", 3000).await {
            Ok(r) => r,
            Err(e) => {
                out.unknown = true;
                out.note = Some(e.message);
                return out;
            }
        };
        let ok = res.get("ok").and_then(Value::as_bool).unwrap_or(false);
        // pre-m2.3 bridges could report 2^64-1 (unsigned underflow race) for a thread that just ticked
        let age: Option<u64> = match res.get("mainThreadAgeMs") {
            Some(v) if v.as_u64().is_some() => {
                v.as_u64().map(|u| if u >= 1u64 << 63 { 0 } else { u })
            }
            Some(v) if v.as_i64().is_some() => Some(0),
            Some(v) if v.as_f64().is_some() => v.as_f64().map(|f| f.max(0.0) as u64),
            _ => None,
        };
        let drain_blocked = res.get("drainBlocked").and_then(Value::as_bool);
        let inflight = res.get("inflight").cloned();
        let shutting = res
            .get("shuttingDown")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let revision = res.get("revision").cloned();
        if let Some(rev) = &revision {
            if Some(rev) != self.caps_revision.as_ref() {
                let _ = self.refresh_capabilities().await;
            }
        }
        out.ok = ok;
        out.main_thread_age_ms = age;
        out.drain_blocked = drain_blocked;
        out.queue_depth = res.get("queueDepth").and_then(Value::as_i64);
        out.shutting_down = Some(shutting);
        out.inflight = inflight.clone();
        out.revision = revision;
        out.modules = res.get("modules").filter(|m| m.is_object()).cloned();
        let Some(age) = age.filter(|_| ok) else {
            out.unknown = true;
            out.note = Some(
                "control.ping did not report mainThreadAgeMs: liveness cannot be verified".into(),
            );
            return out;
        };
        if shutting {
            out.note = Some("bridge is shutting down".into());
            return out;
        }
        if age > threshold {
            let (recent, external, external_left_ms) = self.recent_job_full();
            let mut excused = recent;
            let mut external_job = external;
            let mut external_remaining: Option<u64> = None;
            if recent && !external {
                // A bridge job is only excused while `control.status` still lists it: a finished
                // job leaves the frame loop quiet for other reasons (the POC's job_grace_s
                // behaviour). An *external* job (run_map's console load) is never listed there,
                // so it is trusted until its own grace expires (review-1 L1).
                if let Some(jobs) = self.job_states().await {
                    if !any_freezing_job_running(&jobs) {
                        self.job_grace_until = None; // job finished; stop excusing it
                    }
                }
                let (still, now_external, now_left) = self.recent_job_full();
                excused = still;
                external_job = now_external;
                external_remaining = if now_external { now_left } else { None };
            } else if recent {
                external_remaining = external_left_ms;
            }
            if excused {
                out.engine_busy = true;
                // review-2 N4: say WHERE the engine-busy grace comes from and how much of it is
                // left, so a caller hitting the gate can tell an external load window from a
                // bridge job's.
                out.external_job = external_job;
                out.external_grace_remaining_ms = external_remaining;
                out.note = Some(format!("main thread quiet for {age} ms while a map job runs (engine load, not a dialog)"));
                return out;
            }
            out.hammer_blocked = true;
            let in_progress = drain_blocked == Some(true)
                || inflight
                    .as_ref()
                    .is_some_and(|v| v.as_i64().unwrap_or(0) != 0 || v.as_bool() == Some(true));
            out.note = Some(if in_progress {
                format!("main thread has not ticked for {age} ms and a bridge request is in progress (it may have opened a modal)")
            } else {
                format!("main thread has not ticked for {age} ms (> {threshold} ms): a modal dialog owns the tools main thread")
            });
        }
        out
    }

    /// Fail closed before dispatching a queued method; nothing is executed here (retry-safe).
    async fn ensure_not_blocked(&mut self) -> Result<(), BridgeError> {
        if !self.opts.block_when_stale {
            return Ok(());
        }
        let live = self.liveness().await;
        if live.hammer_blocked {
            return Err(BridgeError::new(
                Kind::HammerBlocked,
                live.note
                    .clone()
                    .unwrap_or_else(|| "Hammer appears to be blocked by a modal dialog".into()),
            )
            .with_data(live.to_json()));
        }
        if live.engine_busy {
            return Err(BridgeError::new(
                Kind::EngineBusy,
                live.note
                    .clone()
                    .unwrap_or_else(|| "a map job is loading; the frame loop is frozen".into()),
            )
            .with_data(live.to_json()));
        }
        if live.unknown {
            return Err(BridgeError::new(
                Kind::HammerBlocked,
                format!(
                    "cannot verify main-thread liveness before dispatch: {}",
                    live.note.clone().unwrap_or_else(|| "unknown".into())
                ),
            )
            .with_data(live.to_json()));
        }
        Ok(())
    }

    // -- document helpers --------------------------------------------------------------------

    /// `doc.active` with the reason classification (`mcp_bridge.c active_doc`):
    /// `no_session`/`no_document` -> `NoDocument` (or `None` when `!require`);
    /// hammer/tf2 not loaded or verifying -> `HammerNotLoaded`; `*_mismatch`/`bad_*` -> `BuildMismatch`.
    pub async fn active_document(
        &mut self,
        require: bool,
        deadline_ms: Option<u64>,
    ) -> Result<Option<Value>, BridgeError> {
        let res = match self.doc_active(deadline_ms).await {
            Ok(res) => res,
            // Before hammer.dll loads, the real bridge answers `doc.active` with
            // `-32017 PrerequisiteFailed {module:"hammer.dll", state:"not_loaded"|"verifying"}`
            // (LIVE-P1b), not with a `{doc:null, reason}` result: the same "Hammer is not up" case.
            Err(e) if hammer_module_not_ready(&e) => {
                let state = e.data.get("state").and_then(Value::as_str).unwrap_or("").to_string();
                let mut mapped = BridgeError::new(
                    Kind::HammerNotLoaded,
                    format!("Hammer is not loaded (hammer.dll state={state:?})"),
                );
                mapped.data = e.data;
                return Err(mapped);
            }
            Err(e) => return Err(e),
        };
        if !res.is_object() {
            return Err(BridgeError::new(
                Kind::ProtocolError,
                "doc.active result is not an object",
            ));
        }
        if let Some(doc) = res.get("doc").filter(|d| !d.is_null()) {
            return Ok(Some(doc.clone()));
        }
        let reason = res.get("reason").and_then(Value::as_str).unwrap_or("");
        if HAMMER_NOT_LOADED_REASONS.contains(&reason) {
            return Err(BridgeError::new(
                Kind::HammerNotLoaded,
                format!("Hammer is not loaded (reason={reason:?})"),
            )
            .with_data(res));
        }
        if MISMATCH_REASONS.contains(&reason) || reason.starts_with("bad_") {
            return Err(BridgeError::new(
                Kind::BuildMismatch,
                format!(
                    "Hammer/module identity mismatch (reason={reason:?}); the bridge fails closed"
                ),
            )
            .with_data(res));
        }
        if require {
            return Err(BridgeError::new(
                Kind::NoDocument,
                format!("no active document (reason={reason:?})"),
            )
            .with_data(res));
        }
        Ok(None)
    }
}

/// A `-32017 PrerequisiteFailed` that says hammer.dll itself is not loaded (or still being
/// identity-checked): `data.module == "hammer.dll"` and `data.state` `not_loaded`/`verifying`.
fn hammer_module_not_ready(e: &BridgeError) -> bool {
    e.kind == Kind::PrerequisiteFailed
        && e.data.get("module").and_then(Value::as_str) == Some("hammer.dll")
        && matches!(
            e.data.get("state").and_then(Value::as_str),
            Some("not_loaded" | "verifying")
        )
}

/// Does `control.status` list a running job that freezes the frame loop? The bridge's job table
/// (`{id, kind, state}` entries) is the only producer of `jobs`, and today its only kind is
/// `resourcecompiler`: a separate process tree that never stalls Hammer's main thread, so it must
/// not excuse a quiet frame loop as "engine busy" (a real modal dialog during a compile still
/// reads as `HammerBlocked`). An entry WITHOUT a `kind` (an older bridge's shape) is not a freezing
/// job either; only an entry that names another kind counts.
pub(crate) fn any_freezing_job_running(jobs: &[Value]) -> bool {
    jobs.iter().any(|job| {
        job.get("state").and_then(Value::as_str) == Some("running")
            && job
                .get("kind")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind != "resourcecompiler")
    })
}

#[cfg(test)]
mod job_tests {
    use super::any_freezing_job_running;
    use serde_json::json;

    #[test]
    fn a_running_compile_job_does_not_excuse_a_quiet_frame_loop() {
        let compile = json!({"id": "j1", "kind": "resourcecompiler", "state": "running"});
        assert!(!any_freezing_job_running(std::slice::from_ref(&compile)));
        // an entry without a kind is the older bridge's shape: not a freezing job either
        let bare = json!({"id": "x", "state": "running"});
        assert!(!any_freezing_job_running(&[compile.clone(), bare]));
        // only a job that names another kind counts
        let other = json!({"id": "y", "kind": "mapload", "state": "running"});
        assert!(any_freezing_job_running(&[compile.clone(), other]));
        assert!(!any_freezing_job_running(&[json!({"id": "x", "state": "ok"})]));
    }
}
