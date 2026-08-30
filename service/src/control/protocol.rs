use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize)]
pub struct Request {
    pub op: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    #[serde(default)]
    pub language: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusPayload {
    pub session_id: Option<String>,
    pub session_started: Option<String>,
    pub microphone: Option<String>,
    pub output: Option<String>,
    pub monitor: Option<String>,
    pub audio_server: String,
    pub provider: String,
    pub pending_jobs: i64,
    pub processing_jobs: i64,
    pub completed_jobs: i64,
    pub stored_audio: String,
    pub free_disk: String,
    pub transcript_path: Option<String>,
    pub capture_active: bool,
    pub capture_stop_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Response {
    OkStatus {
        ok: bool,
        status: Box<StatusPayload>,
    },
    OkStop {
        ok: bool,
    },
    Err {
        ok: bool,
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fields: Option<std::collections::BTreeMap<String, String>>,
    },
}

impl Response {
    pub fn status(status: StatusPayload) -> Self {
        Self::OkStatus {
            ok: true,
            status: Box::new(status),
        }
    }

    pub fn stop() -> Self {
        Self::OkStop { ok: true }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::Err {
            ok: false,
            error: message.into(),
            code: None,
            details: None,
            fields: None,
        }
    }

    pub fn error_code(code: &str, message: impl Into<String>) -> Self {
        Self::Err {
            ok: false,
            error: message.into(),
            code: Some(code.to_string()),
            details: None,
            fields: None,
        }
    }

    pub fn error_fields(
        message: impl Into<String>,
        fields: std::collections::BTreeMap<String, String>,
    ) -> Self {
        Self::Err {
            ok: false,
            error: message.into(),
            code: Some("validation".into()),
            details: None,
            fields: Some(fields),
        }
    }
}

pub fn parse_request(line: &str) -> Result<Request, String> {
    serde_json::from_str(line).map_err(|error| format!("invalid control request: {error}"))
}

pub fn encode_response(response: &Response) -> String {
    match serde_json::to_string(response) {
        Ok(json) => json,
        Err(_) => "{\"ok\":false,\"error\":\"failed to encode control response\"}".to_string(),
    }
}

pub fn encode_ok(extra: serde_json::Value) -> String {
    let mut map = serde_json::Map::new();
    map.insert("ok".into(), serde_json::json!(true));
    if let serde_json::Value::Object(fields) = extra {
        for (key, value) in fields {
            map.insert(key, value);
        }
    }
    serde_json::Value::Object(map).to_string()
}

#[cfg(test)]
mod tests {
    use super::{encode_response, parse_request, Response};

    #[test]
    fn parse_ops() {
        assert_eq!(parse_request(r#"{"op":"status"}"#).unwrap().op, "status");
        assert_eq!(parse_request(r#"{"op":"stop"}"#).unwrap().op, "stop");
        assert_eq!(
            parse_request(r#"{"op":"start_recording","language":"fr"}"#)
                .unwrap()
                .language
                .as_deref(),
            Some("fr")
        );
        assert!(parse_request("not-json").is_err());
    }

    #[test]
    fn unknown_op_error_shape() {
        let json = encode_response(&Response::error("unknown op 'nope'"));
        assert!(json.contains("\"ok\":false"));
        assert!(json.contains("unknown op"));
    }
}
