use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Diagnostic {
    pub range: Range,
    #[serde(default)]
    pub severity: Option<u8>,
    #[serde(default)]
    pub code: Option<Value>,
    #[serde(default)]
    pub source: Option<String>,
    pub message: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PublishDiagnosticsParams {
    pub uri: String,
    #[serde(default)]
    pub version: Option<i64>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Serialize, Debug)]
pub struct RpcRequest {
    pub jsonrpc: &'static str,
    pub id: i64,
    pub method: String,
    pub params: Value,
}

#[derive(Serialize, Debug)]
pub struct RpcNotification {
    pub jsonrpc: &'static str,
    pub method: String,
    pub params: Value,
}

#[derive(Deserialize, Debug)]
pub struct IncomingRequest {
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Deserialize, Debug)]
pub struct IncomingNotification {
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Deserialize, Debug)]
pub struct IncomingResponse {
    pub id: Value,
    #[serde(default)]
    pub result: Value,
    #[serde(default)]
    pub error: Value,
}

/// An incoming JSON-RPC message. Order matters: response (has `id`+`result`/`error`),
/// request (has `id`+`method`), notification (has `method`, no `id`).
#[derive(Debug)]
pub enum RpcMessage {
    Response(IncomingResponse),
    Request(IncomingRequest),
    Notification(IncomingNotification),
}

impl<'de> Deserialize<'de> for RpcMessage {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        let has_id = v.get("id").is_some_and(|x| !x.is_null());
        let has_method = v.get("method").is_some();
        if has_id && !has_method {
            Ok(RpcMessage::Response(
                serde_json::from_value(v).map_err(serde::de::Error::custom)?,
            ))
        } else if has_id && has_method {
            Ok(RpcMessage::Request(
                serde_json::from_value(v).map_err(serde::de::Error::custom)?,
            ))
        } else {
            Ok(RpcMessage::Notification(
                serde_json::from_value(v).map_err(serde::de::Error::custom)?,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_publish_diagnostics() {
        let raw = r#"{"uri":"file:///x.rs","version":3,"diagnostics":[
            {"range":{"start":{"line":0,"character":4},"end":{"line":0,"character":9}},
             "severity":1,"message":"mismatched types"}]}"#;
        let p: PublishDiagnosticsParams = serde_json::from_str(raw).unwrap();
        assert_eq!(p.uri, "file:///x.rs");
        assert_eq!(p.diagnostics.len(), 1);
        assert_eq!(p.diagnostics[0].severity, Some(1));
        assert_eq!(p.diagnostics[0].range.start.character, 4);
    }

    #[test]
    fn incoming_notification_parses() {
        let raw = r#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"file:///x","diagnostics":[]}}"#;
        match serde_json::from_str::<RpcMessage>(raw).unwrap() {
            RpcMessage::Notification(n) => assert_eq!(n.method, "textDocument/publishDiagnostics"),
            _ => panic!("expected notification"),
        }
    }
}
