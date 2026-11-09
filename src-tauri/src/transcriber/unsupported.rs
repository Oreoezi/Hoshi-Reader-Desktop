use tauri::AppHandle;
use tokio_util::sync::CancellationToken;

use super::Events;

pub struct Audio {
    pub duration: f64,
}

pub fn prepare(_path: &str) -> Result<Audio, String> {
    Err("Audio transcription is not available on this platform".into())
}

pub fn start(
    _app: &AppHandle,
    _audio: Audio,
    _from: f64,
    _performance: i32,
    _events: Events,
    _cancel: CancellationToken,
) -> Result<(), String> {
    Err("Audio transcription is not available on this platform".into())
}

pub async fn status(_app: AppHandle) -> &'static str {
    "unavailable"
}
