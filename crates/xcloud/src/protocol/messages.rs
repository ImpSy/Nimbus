//! JSON protocols of the `control` and `message` data channels.

use serde::Deserialize;
use serde_json::{Value, json};

pub const CONTROL_ACCESS_KEY: &str = "4BDB3609-C1F1-4195-9B37-FEFF45DA8B8E";

pub fn control_authorization() -> String {
    json!({ "message": "authorizationRequest", "accessKey": CONTROL_ACCESS_KEY }).to_string()
}

pub fn control_gamepad_changed(index: u8, added: bool) -> String {
    json!({ "message": "gamepadChanged", "gamepadIndex": index, "wasAdded": added }).to_string()
}

pub fn control_keyframe_request(ifr: bool) -> String {
    json!({ "message": "videoKeyframeRequested", "ifrRequested": ifr }).to_string()
}

pub fn message_handshake() -> String {
    json!({
        "type": "Handshake",
        "version": "messageV1",
        "id": uuid::Uuid::new_v4().to_string(),
        "cv": "0",
    })
    .to_string()
}

pub fn message(target: &str, content: Value) -> String {
    json!({
        "type": "Message",
        "content": content.to_string(),
        "id": uuid::Uuid::new_v4().to_string(),
        "target": target,
        "cv": "",
    })
    .to_string()
}

pub fn transaction_complete(id: &str, content: Value) -> String {
    json!({
        "type": "TransactionComplete",
        "content": content.to_string(),
        "id": id,
        "cv": "",
    })
    .to_string()
}

/// Messages sent right after the message-channel handshake is acknowledged.
pub fn initial_configuration(width: u32, height: u32, install_id: &str) -> Vec<String> {
    vec![
        message(
            "/streaming/systemUi/configuration",
            json!({ "version": [0, 2, 0], "systemUis": [] }),
        ),
        message(
            "/streaming/properties/clientappinstallidchanged",
            json!({ "clientAppInstallId": install_id }),
        ),
        message(
            "/streaming/characteristics/orientationchanged",
            json!({ "orientation": 0 }),
        ),
        message(
            "/streaming/characteristics/touchinputenabledchanged",
            json!({ "touchInputEnabled": false }),
        ),
        message(
            "/streaming/characteristics/clientdevicecapabilities",
            json!({}),
        ),
        message(
            "/streaming/characteristics/dimensionschanged",
            json!({
                "horizontal": width,
                "vertical": height,
                "preferredWidth": width,
                "preferredHeight": height,
                "safeAreaLeft": 0,
                "safeAreaTop": 0,
                "safeAreaRight": width,
                "safeAreaBottom": height,
                "supportsCustomResolution": true,
            }),
        ),
    ]
}

#[derive(Debug, Deserialize)]
pub struct IncomingMessage {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    /// Usually a JSON document encoded as a string.
    #[serde(default)]
    pub content: Option<Value>,
}

/// A modal dialog the console wants us to show (e.g. "quit game?").
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Dialog {
    pub transaction_id: String,
    pub title: String,
    pub text: String,
    pub buttons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MessageEvent {
    HandshakeAck,
    ServerDisconnect { transaction_id: Option<String> },
    TitleInfo(Value),
    Dialog(Dialog),
    Other,
}

pub fn parse_message(data: &[u8]) -> Option<MessageEvent> {
    let msg: IncomingMessage = serde_json::from_slice(data).ok()?;
    match msg.kind.as_str() {
        "HandshakeAck" => Some(MessageEvent::HandshakeAck),
        "TransactionStart" | "Message" => {
            let target = msg.target.as_deref().unwrap_or_default();
            let content: Value = match msg.content {
                Some(Value::String(c)) => serde_json::from_str(&c).unwrap_or(Value::String(c)),
                Some(other) => other,
                None => Value::Null,
            };
            match target {
                "/streaming/sessionLifetimeManagement/serverInitiatedDisconnect" => {
                    Some(MessageEvent::ServerDisconnect {
                        transaction_id: msg.id,
                    })
                }
                "/streaming/properties/titleinfo" => Some(MessageEvent::TitleInfo(content)),
                "/streaming/systemUi/messages/ShowMessageDialog" => {
                    let s = |k: &str| {
                        content
                            .get(k)
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned()
                    };
                    let mut buttons = Vec::new();
                    for k in ["CommandLabel1", "CommandLabel2", "CommandLabel3"] {
                        let label = s(k);
                        if !label.is_empty() {
                            buttons.push(label);
                        }
                    }
                    if buttons.is_empty() {
                        buttons.push("OK".into());
                    }
                    Some(MessageEvent::Dialog(Dialog {
                        transaction_id: msg.id.unwrap_or_default(),
                        title: s("TitleText"),
                        text: s("ContentText"),
                        buttons,
                    }))
                }
                _ => {
                    tracing::debug!(target, "unhandled message channel target");
                    Some(MessageEvent::Other)
                }
            }
        }
        _ => Some(MessageEvent::Other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dialog() {
        let content = json!({"TitleText":"Quit?","ContentText":"Sure?","CommandLabel1":"Yes","CommandLabel2":"No"}).to_string();
        let raw = json!({"type":"TransactionStart","id":"abc","target":"/streaming/systemUi/messages/ShowMessageDialog","content":content}).to_string();
        match parse_message(raw.as_bytes()) {
            Some(MessageEvent::Dialog(d)) => {
                assert_eq!(d.transaction_id, "abc");
                assert_eq!(d.buttons, vec!["Yes", "No"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn handshake_ack() {
        assert_eq!(
            parse_message(br#"{"type":"HandshakeAck","id":"x","cv":"0"}"#),
            Some(MessageEvent::HandshakeAck)
        );
    }
}
