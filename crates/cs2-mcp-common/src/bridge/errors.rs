//! Error taxonomy (wire codes + client-side classes), ported from `bridge_client.py`.
//!
//! `retry_safe` is true only when nothing was executed; `outcome_unknown` is true when the
//! request may have executed (verify by save + parse / re-read, never retry blindly).
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    // client-side conditions the wire cannot express
    BridgeNotRunning,
    BuildMismatch,
    HammerNotLoaded,
    NoDocument,
    ReadOnlyMap,
    HammerBlocked,
    EngineBusy,
    OutcomeUnknown,
    ConnectionLost,
    ProtocolError,
    // wire (bridge-protocol.md §8)
    NotAuthenticated,
    VersionMismatch,
    DocumentChanged,
    StaleHandle,
    UnknownClass,
    Busy,
    QueueTimeout,
    Cancelled,
    NotCancellable,
    NotImplemented,
    PartialFailure,
    ShuttingDown,
    AssetNotFound,
    UntitledDocument,
    UnsavedChanges,
    PrerequisiteFailed,
    DocumentNotEditable,
    NoViewport,
    RenderFailed,
    NoGameWorld,
    NodeNotFound,
    /// -32023: the game's schema scope for client.dll is not available yet (nothing ran)
    SchemaUnavailable,
    /// -32024: no local player controller/pawn (not in a game, dead/spectating); nothing ran
    NoLocalPlayer,
    /// -32026 (m2.18): `entity.removeConnections` `match` with no live hit; nothing was opened
    ConnectionNotFound,
    MethodNotFound,
    /// an application code this client does not know: treated as "may have executed"
    UnknownWire,
}

impl Kind {
    pub fn from_code(code: i64) -> Option<Kind> {
        Some(match code {
            -32001 => Kind::NotAuthenticated,
            -32002 => Kind::VersionMismatch,
            -32003 => Kind::NoDocument, // wire name NoActiveDocument
            -32004 => Kind::DocumentChanged,
            -32005 => Kind::StaleHandle,
            -32006 => Kind::UnknownClass,
            -32007 => Kind::Busy,
            -32008 => Kind::QueueTimeout,
            -32009 => Kind::Cancelled,
            -32010 => Kind::NotCancellable,
            -32011 => Kind::NotImplemented,
            -32012 => Kind::PartialFailure,
            -32013 => Kind::ShuttingDown,
            -32014 => Kind::AssetNotFound,
            -32015 => Kind::UntitledDocument,
            -32016 => Kind::UnsavedChanges,
            -32017 => Kind::PrerequisiteFailed,
            -32018 => Kind::DocumentNotEditable,
            -32019 => Kind::NoViewport,
            -32020 => Kind::RenderFailed,
            -32021 => Kind::NoGameWorld,
            -32022 => Kind::NodeNotFound,
            -32023 => Kind::SchemaUnavailable,
            -32024 => Kind::NoLocalPlayer,
            -32026 => Kind::ConnectionNotFound,
            -32601 => Kind::MethodNotFound,
            -32600 | -32602 | -32603 | -32700 => Kind::ProtocolError,
            _ => return None,
        })
    }

    /// The POC's own code for this class (wire code where it is one).
    pub fn code(self) -> Option<i64> {
        Some(match self {
            Kind::NotAuthenticated => -32001,
            Kind::VersionMismatch => -32002,
            Kind::DocumentChanged => -32004,
            Kind::StaleHandle => -32005,
            Kind::UnknownClass => -32006,
            Kind::Busy => -32007,
            Kind::QueueTimeout => -32008,
            Kind::Cancelled => -32009,
            Kind::NotCancellable => -32010,
            Kind::NotImplemented => -32011,
            Kind::PartialFailure => -32012,
            Kind::ShuttingDown => -32013,
            Kind::AssetNotFound => -32014,
            Kind::UntitledDocument => -32015,
            Kind::UnsavedChanges => -32016,
            Kind::PrerequisiteFailed => -32017,
            Kind::DocumentNotEditable => -32018,
            Kind::NoViewport => -32019,
            Kind::RenderFailed => -32020,
            Kind::NoGameWorld => -32021,
            Kind::NodeNotFound => -32022,
            Kind::SchemaUnavailable => -32023,
            Kind::NoLocalPlayer => -32024,
            Kind::ConnectionNotFound => -32026,
            Kind::MethodNotFound => -32601,
            _ => return None,
        })
    }

    /// Taxonomy name (the POC's `name`; what tool consumers switch on).
    pub fn name(self) -> &'static str {
        match self {
            Kind::BridgeNotRunning => "BridgeNotRunning",
            Kind::BuildMismatch => "BuildMismatch",
            Kind::HammerNotLoaded => "HammerNotLoaded",
            Kind::NoDocument => "NoDocument",
            Kind::ReadOnlyMap => "ReadOnlyMap",
            Kind::HammerBlocked => "HammerBlocked",
            Kind::EngineBusy => "EngineBusy",
            Kind::OutcomeUnknown => "OutcomeUnknown",
            Kind::ConnectionLost => "ConnectionLost",
            Kind::ProtocolError => "ProtocolError",
            Kind::NotAuthenticated => "NotAuthenticated",
            Kind::VersionMismatch => "VersionMismatch",
            Kind::DocumentChanged => "DocumentChanged",
            Kind::StaleHandle => "StaleHandle",
            Kind::UnknownClass => "UnknownClass",
            Kind::Busy => "Busy",
            Kind::QueueTimeout => "QueueTimeout",
            Kind::Cancelled => "Cancelled",
            Kind::NotCancellable => "NotCancellable",
            Kind::NotImplemented => "NotImplemented",
            Kind::PartialFailure => "PartialFailure",
            Kind::ShuttingDown => "ShuttingDown",
            Kind::AssetNotFound => "AssetNotFound",
            Kind::UntitledDocument => "UntitledDocument",
            Kind::UnsavedChanges => "UnsavedChanges",
            Kind::PrerequisiteFailed => "PrerequisiteFailed",
            Kind::DocumentNotEditable => "DocumentNotEditable",
            Kind::NoViewport => "NoViewport",
            Kind::RenderFailed => "RenderFailed",
            Kind::NoGameWorld => "NoGameWorld",
            Kind::NodeNotFound => "NodeNotFound",
            Kind::SchemaUnavailable => "SchemaUnavailable",
            Kind::NoLocalPlayer => "NoLocalPlayer",
            Kind::ConnectionNotFound => "ConnectionNotFound",
            Kind::MethodNotFound => "MethodNotFound",
            Kind::UnknownWire => "BridgeError",
        }
    }

    /// True only when nothing was executed.
    pub fn retry_safe(self) -> bool {
        matches!(
            self,
            Kind::BridgeNotRunning
                | Kind::HammerNotLoaded
                | Kind::ReadOnlyMap
                | Kind::HammerBlocked
                | Kind::EngineBusy
                | Kind::PrerequisiteFailed
                | Kind::DocumentChanged
                | Kind::StaleHandle
                | Kind::UnknownClass
                | Kind::Busy
                | Kind::QueueTimeout
                | Kind::Cancelled
                | Kind::NotImplemented
                | Kind::ShuttingDown
                | Kind::AssetNotFound
                | Kind::UntitledDocument
                | Kind::UnsavedChanges
                | Kind::DocumentNotEditable
                | Kind::NoViewport
                | Kind::RenderFailed
                | Kind::NoGameWorld
                | Kind::NodeNotFound
                | Kind::SchemaUnavailable
                | Kind::NoLocalPlayer
                | Kind::ConnectionNotFound
                | Kind::MethodNotFound
        )
    }

    /// True when the request may have executed.
    pub fn outcome_unknown(self) -> bool {
        matches!(
            self,
            Kind::OutcomeUnknown
                | Kind::ConnectionLost
                | Kind::NotCancellable
                | Kind::PartialFailure
                | Kind::UnknownWire
        )
    }

    /// `VersionMismatch` and `PrerequisiteFailed` are build mismatches (POC subclassing).
    pub fn is_build_mismatch(self) -> bool {
        matches!(
            self,
            Kind::BuildMismatch | Kind::VersionMismatch | Kind::PrerequisiteFailed
        )
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{}{}: {message}", kind.name(), code.map(|c| format!(" [{c}]")).unwrap_or_default())]
pub struct BridgeError {
    pub kind: Kind,
    pub message: String,
    /// wire code when the error came from the wire (or the class's own code)
    pub code: Option<i64>,
    /// error message string on the wire
    pub wire_name: Option<String>,
    pub data: Map<String, Value>,
}

impl BridgeError {
    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        BridgeError {
            kind,
            message: message.into(),
            code: kind.code(),
            wire_name: None,
            data: Map::new(),
        }
    }
    pub fn with_code(mut self, code: i64) -> Self {
        self.code = Some(code);
        self
    }
    pub fn with_wire_name(mut self, name: impl Into<String>) -> Self {
        self.wire_name = Some(name.into());
        self
    }
    pub fn with_data(mut self, data: Value) -> Self {
        if let Value::Object(m) = data {
            self.data.extend(m);
        }
        self
    }
    pub fn retry_safe(&self) -> bool {
        self.kind.retry_safe()
    }
    pub fn outcome_unknown(&self) -> bool {
        self.kind.outcome_unknown()
    }
    pub fn is_build_mismatch(&self) -> bool {
        self.kind.is_build_mismatch()
    }

    /// Decode a wire `error` member (`{code, message, data?}`) for `method`.
    pub fn from_wire(method: &str, err: &Value) -> BridgeError {
        let code = err.get("code").and_then(Value::as_i64);
        let wire_name = err
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut data = err
            .get("data")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        data.entry("method")
            .or_insert_with(|| Value::String(method.to_string()));
        match code.and_then(Kind::from_code) {
            Some(kind) => {
                let detail = data
                    .get("detail")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let message = detail
                    .or_else(|| wire_name.clone())
                    .unwrap_or_else(|| kind.name().to_string());
                BridgeError {
                    kind,
                    message,
                    code,
                    wire_name,
                    data,
                }
            }
            None => BridgeError {
                kind: Kind::UnknownWire,
                message: format!(
                    "bridge error {}: {}",
                    code.map(|c| c.to_string()).unwrap_or_default(),
                    wire_name.clone().unwrap_or_default()
                ),
                code,
                wire_name,
                data,
            },
        }
    }

    /// The POC payload shape (`BridgeError.to_dict`).
    pub fn to_json(&self) -> Value {
        json!({
            "type": format!("{:?}", self.kind),
            "name": self.kind.name(),
            "code": self.code,
            "wireName": self.wire_name,
            "message": self.message,
            "data": Value::Object(self.data.clone()),
            "retrySafe": self.retry_safe(),
            "outcomeUnknown": self.outcome_unknown(),
        })
    }
}
