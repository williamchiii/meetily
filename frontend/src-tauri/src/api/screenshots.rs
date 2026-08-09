// api/screenshots.rs
//
// Screenshots shared during a meeting.
//
// A screenshot is read by the user's selected summary model the moment it is added,
// so they can see what was understood while the meeting is still running. The
// extracted text is held by the frontend until the recording stops and a meeting row
// exists to attach it to, then it flows into the summary prompt.

use base64::Engine;
use tauri::{AppHandle, Runtime};

use crate::database::repositories::{
    screenshot::{MeetingScreenshot, NewScreenshot, ScreenshotsRepository},
    setting::SettingsRepository,
};
use crate::state::AppState;
use crate::summary::llm_client::LLMProvider;
use crate::summary::vision;
use crate::{log_error, log_info};

/// Result of reading one screenshot, before it belongs to any meeting.
#[derive(Debug, serde::Serialize)]
pub struct ScreenshotExtraction {
    pub extracted_text: String,
    /// Where the image was written, when there was an active recording to file it under.
    pub image_path: Option<String>,
    /// Which provider/model did the reading.
    pub model: String,
}

/// Read a screenshot with the selected summary model and return what it found.
///
/// `image_base64` is the raw payload; a `data:` prefix is tolerated and stripped.
#[tauri::command]
pub async fn api_extract_screenshot_context<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    image_base64: String,
    mime_type: Option<String>,
    file_name: Option<String>,
) -> Result<ScreenshotExtraction, String> {
    let mime_type = mime_type.unwrap_or_else(|| "image/png".to_string());

    // Browsers hand over a full data URI; the APIs want only the payload
    let payload = image_base64
        .split_once("base64,")
        .map(|(_, rest)| rest)
        .unwrap_or(&image_base64)
        .trim();

    if payload.is_empty() {
        return Err("The screenshot was empty".to_string());
    }

    let pool = state.db_manager.pool();

    let setting = SettingsRepository::get_model_config(pool)
        .await
        .map_err(|e| format!("Could not read the model configuration: {}", e))?
        .ok_or_else(|| {
            "No AI model is configured. Choose a summary model in Settings first.".to_string()
        })?;

    let provider = LLMProvider::from_str(&setting.provider)?;

    // Same key resolution the summary uses, so screenshots follow the selected model
    let api_key = match provider {
        LLMProvider::Ollama | LLMProvider::BuiltInAI | LLMProvider::CustomOpenAI => String::new(),
        _ => SettingsRepository::get_api_key(pool, &setting.provider)
            .await
            .map_err(|e| format!("Could not read the API key for {}: {}", setting.provider, e))?
            .ok_or_else(|| {
                format!(
                    "No API key saved for {}. Add one in Settings to read screenshots.",
                    setting.provider
                )
            })?,
    };

    let client = reqwest::Client::new();

    let extracted = vision::extract_image_context(
        &client,
        &provider,
        &setting.model,
        &api_key,
        payload,
        &mime_type,
        setting.ollama_endpoint.as_deref(),
        None,
    )
    .await
    .map_err(|e| {
        log_error!("Screenshot extraction failed: {}", e);
        e.to_string()
    })?;

    // Keep the image next to the recording it belongs to, when one is running
    let image_path = save_alongside_recording(payload, &mime_type, file_name.as_deref()).await;

    log_info!(
        "Extracted {} chars of screenshot context via {}",
        extracted.text.len(),
        extracted.model
    );

    Ok(ScreenshotExtraction {
        extracted_text: extracted.text,
        image_path,
        model: extracted.model,
    })
}

/// Write the image into the active recording's folder.
///
/// Best-effort: a screenshot is still useful without its image on disk, and there may
/// be no recording running at all (the user can add screenshots to a saved meeting).
async fn save_alongside_recording(
    payload: &str,
    mime_type: &str,
    file_name: Option<&str>,
) -> Option<String> {
    let folder = crate::audio::recording_commands::get_meeting_folder_path()
        .await
        .ok()
        .flatten()?;

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;

    let dir = std::path::Path::new(&folder).join("screenshots");

    let extension = match mime_type {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    };

    // Millisecond stamps keep several screenshots in one meeting from colliding
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S%.3f");
    let stem = file_name
        .and_then(|n| std::path::Path::new(n).file_stem().map(|s| s.to_string_lossy().to_string()))
        .map(|s| sanitize(&s))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "screenshot".to_string());

    let path = dir.join(format!("{}-{}.{}", stamp, stem, extension));

    // Disk I/O off the async runtime thread, same as recording_commands.rs does
    // for equivalent work - this can run while a recording is actively writing to
    // the same folder.
    tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&dir).ok()?;
        match std::fs::write(&path, &bytes) {
            Ok(_) => Some(path.to_string_lossy().to_string()),
            Err(e) => {
                log_error!("Could not save screenshot image: {}", e);
                None
            }
        }
    })
    .await
    .ok()
    .flatten()
}

/// Keep a user-supplied file name from escaping the screenshots directory.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .take(40)
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Attach screenshots gathered during a recording to the meeting it produced.
#[tauri::command]
pub async fn api_attach_meeting_screenshots<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_id: String,
    screenshots: Vec<NewScreenshot>,
) -> Result<usize, String> {
    log_info!(
        "Attaching {} screenshot(s) to meeting {}",
        screenshots.len(),
        meeting_id
    );

    ScreenshotsRepository::attach_to_meeting(state.db_manager.pool(), &meeting_id, &screenshots)
        .await
        .map_err(|e| {
            log_error!("Failed to attach screenshots to {}: {}", meeting_id, e);
            format!("Failed to save screenshots: {}", e)
        })
}

#[tauri::command]
pub async fn api_get_meeting_screenshots<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    meeting_id: String,
) -> Result<Vec<MeetingScreenshot>, String> {
    ScreenshotsRepository::for_meeting(state.db_manager.pool(), &meeting_id)
        .await
        .map_err(|e| format!("Failed to load screenshots: {}", e))
}

#[tauri::command]
pub async fn api_delete_meeting_screenshot<R: Runtime>(
    _app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    screenshot_id: String,
) -> Result<bool, String> {
    ScreenshotsRepository::delete(state.db_manager.pool(), &screenshot_id)
        .await
        .map_err(|e| format!("Failed to delete screenshot: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_path_separators() {
        assert_eq!(sanitize("../../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize("Screen Shot 2026"), "Screen-Shot-2026");
        assert_eq!(sanitize("safe_name-1"), "safe_name-1");
    }

    #[test]
    fn sanitize_is_bounded() {
        assert!(sanitize(&"a".repeat(200)).len() <= 40);
    }
}
