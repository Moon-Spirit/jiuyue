//! Call windows: one dedicated OS window per Conversation.
//!
//! A Call must run in a window of its own, decoupled from the main window's
//! lifecycle (AGENTS.md). This module owns that window's creation, its focus
//! behaviour, and the event that tells the call surface which Call it was opened
//! for. Media itself is not here: signalling is ticket #30.

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, Runtime, WebviewUrl, WebviewWindowBuilder};

/// Event the window's own document listens for through
/// `DesktopShell.onCallWindowOpen`.
pub const CALL_OPEN_EVENT: &str = "shell://call-open";

/// Every Call window label starts with this, and nothing else may be closed
/// through the interface.
const CALL_WINDOW_PREFIX: &str = "call-";

const CALL_WIDTH: f64 = 960.0;
const CALL_HEIGHT: f64 = 640.0;
const CALL_MIN_WIDTH: f64 = 640.0;
const CALL_MIN_HEIGHT: f64 = 420.0;

/// The two Call shapes the product supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallKind {
    Audio,
    Video,
}

impl CallKind {
    fn slug(self) -> &'static str {
        match self {
            CallKind::Audio => "audio",
            CallKind::Video => "video",
        }
    }

    fn title(self) -> &'static str {
        match self {
            CallKind::Audio => "语音通话",
            CallKind::Video => "视频通话",
        }
    }
}

/// Which Call to open a window for. Field names match the TypeScript interface.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallWindowRequest {
    pub conversation_id: String,
    pub kind: CallKind,
}

impl CallWindowRequest {
    /// A window label that is safe to pass to the platform.
    ///
    /// Window labels only accept alphanumerics, `-`, `_`, `/` and `:` on Windows;
    /// a Conversation id is sanitised rather than trusted, so a malformed id
    /// cannot produce a window the shell refuses to create.
    fn label(&self) -> String {
        let sanitised: String = self
            .conversation_id
            .chars()
            .filter(|character| {
                character.is_ascii_alphanumeric() || *character == '-' || *character == '_'
            })
            .take(64)
            .collect();

        let id = if sanitised.is_empty() {
            "unknown"
        } else {
            sanitised.as_str()
        };
        format!("{CALL_WINDOW_PREFIX}{id}-{}", self.kind.slug())
    }

    fn title(&self) -> &'static str {
        self.kind.title()
    }

    fn label_is_call_window(label: &str) -> bool {
        label.starts_with(CALL_WINDOW_PREFIX)
    }
}

/// Open (or focus) the Call window for a request, returning its label.
pub fn open<R: Runtime>(app: &AppHandle<R>, request: &CallWindowRequest) -> Result<String, String> {
    let label = request.label();

    if let Some(existing) = app.get_webview_window(&label) {
        // Already talking: re-focus rather than opening a second window.
        existing.set_focus().map_err(|error| error.to_string())?;
        return Ok(label);
    }

    let payload = request.clone();
    let window = WebviewWindowBuilder::new(app, &label, WebviewUrl::App("index.html".into()))
        .title(request.title())
        .inner_size(CALL_WIDTH, CALL_HEIGHT)
        .min_inner_size(CALL_MIN_WIDTH, CALL_MIN_HEIGHT)
        .resizable(true)
        .center()
        .on_page_load(move |window, load| {
            // Emitted on every finished load, so a reload of the call surface
            // re-learns its Conversation instead of coming up blank.
            if matches!(load.event(), tauri::webview::PageLoadEvent::Finished) {
                let _ = window.emit(CALL_OPEN_EVENT, payload.clone());
            }
        })
        .build()
        .map_err(|error| error.to_string())?;

    Ok(window.label().to_string())
}

/// Close a Call window by label. Refuses any window that is not one.
pub fn close<R: Runtime>(app: &AppHandle<R>, label: &str) -> Result<bool, String> {
    if !CallWindowRequest::label_is_call_window(label) {
        return Err("refusing to close a window that is not a Call window".to_string());
    }

    let Some(window) = app.get_webview_window(label) else {
        return Ok(false);
    };

    window.close().map_err(|error| error.to_string())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(conversation_id: &str, kind: CallKind) -> CallWindowRequest {
        CallWindowRequest {
            conversation_id: conversation_id.to_string(),
            kind,
        }
    }

    #[test]
    fn label_names_the_conversation_and_the_call_kind() {
        let audio = request("01J8Z0000000000000000000AB", CallKind::Audio);
        assert_eq!(audio.label(), "call-01J8Z0000000000000000000AB-audio");
    }

    #[test]
    fn video_is_a_different_window_from_audio() {
        let id = "01J8Z0000000000000000000AB";
        assert_ne!(
            request(id, CallKind::Audio).label(),
            request(id, CallKind::Video).label()
        );
    }

    #[test]
    fn label_sanitises_a_hostile_conversation_id() {
        let hostile = request("../../escape:main", CallKind::Video);
        assert_eq!(hostile.label(), "call-escapemain-video");
    }

    #[test]
    fn label_falls_back_when_nothing_usable_remains() {
        assert_eq!(
            request("../..", CallKind::Video).label(),
            "call-unknown-video"
        );
    }

    #[test]
    fn only_call_windows_may_be_closed() {
        assert!(CallWindowRequest::label_is_call_window("call-c1-audio"));
        assert!(!CallWindowRequest::label_is_call_window("main"));
    }

    #[test]
    fn request_round_trips_through_its_wire_shape() {
        let wire = serde_json::to_string(&request("c1", CallKind::Video)).unwrap();
        assert_eq!(wire, r#"{"conversationId":"c1","kind":"video"}"#);

        let parsed: CallWindowRequest = serde_json::from_str(&wire).unwrap();
        assert_eq!(parsed.conversation_id, "c1");
        assert_eq!(parsed.kind, CallKind::Video);
    }

    #[test]
    fn titles_are_the_ui_language() {
        assert_eq!(request("c1", CallKind::Audio).title(), "语音通话");
        assert_eq!(request("c1", CallKind::Video).title(), "视频通话");
    }
}
