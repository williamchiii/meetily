// audio/call_detector.rs
//
// Auto-stop a recording when the call it belongs to ends.
//
// While a recording is active we poll the OS for meeting apps that are holding the
// microphone (Zoom, Teams, a browser sitting in a Meet call, ...). The watcher only
// *arms* itself once it has actually seen a call, so recordings of in-person meetings
// are never stopped automatically. Once armed, the call has to stay gone for
// END_GRACE before we stop, which rides out the short dropouts that happen when a
// call switches audio devices or reconnects.
//
// The stop itself goes through the exact same path as the tray menu's "Stop
// Recording": `recording_commands::stop_recording` followed by the
// `recording-stop-complete` event, so the frontend runs its normal post-processing
// (transcript flush, SQLite save, navigation, analytics).
//
// Only microphone capture counts as a call, never audio playback: a browser playing a
// video would otherwise look exactly like a browser in a Meet call, and a false arm
// costs the user a recording. Holding the mic is unambiguous - meeting apps keep the
// input stream open for the whole call, software mute included. The cost is that a
// listen-only webinar never arms the watcher, which fails safe.

use log::{debug, error, info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, Runtime};

/// How often the OS is sampled for call activity.
const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// A call has to be visible for this long before auto-stop arms.
const ARM_AFTER: Duration = Duration::from_secs(5);

/// ...and gone for this long before the recording is stopped.
const END_GRACE: Duration = Duration::from_secs(20);

/// Cancellation flag for the watcher belonging to the current recording.
static WATCHER_CANCEL: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

/// Result of a single probe of the operating system.
pub enum CallProbe {
    /// Detection ran; holds the meeting apps currently in a call (possibly empty).
    Apps(Vec<String>),
    /// Detection is unavailable here (unsupported OS/OS version, missing tooling).
    Unsupported(String),
}

// ============================================================================
// LIFECYCLE
// ============================================================================

/// Start watching for the end of the current call.
///
/// Safe to call unconditionally when a recording starts: any previous watcher is
/// cancelled first, and the watcher re-reads the user preference on every poll so a
/// disabled setting simply keeps it idle.
pub fn start<R: Runtime>(app: AppHandle<R>) {
    let cancel = Arc::new(AtomicBool::new(false));

    {
        let mut guard = match WATCHER_CANCEL.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(previous) = guard.replace(cancel.clone()) {
            previous.store(true, Ordering::SeqCst);
        }
    }

    tauri::async_runtime::spawn(async move {
        watch(app, cancel).await;
    });

    info!("📞 Call-end watcher started");
}

/// Stop watching. Idempotent, and safe to call when no watcher is running.
pub fn stop() {
    let mut guard = match WATCHER_CANCEL.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(cancel) = guard.take() {
        cancel.store(true, Ordering::SeqCst);
        debug!("📞 Call-end watcher cancelled");
    }
}

// ============================================================================
// WATCHER
// ============================================================================

async fn watch<R: Runtime>(app: AppHandle<R>, cancel: Arc<AtomicBool>) {
    let mut armed = false;
    let mut call_seen_since: Option<Instant> = None;
    let mut call_gone_since: Option<Instant> = None;
    let mut last_apps: Vec<String> = Vec::new();

    loop {
        tokio::time::sleep(POLL_INTERVAL).await;

        if cancel.load(Ordering::SeqCst) {
            return;
        }

        // A watcher outlives its recording if the recording ended down a path that
        // never reaches `stop_recording` (a fatal capture error, say). Without this it
        // would eventually "stop" a recording that is already gone and push the
        // frontend through post-processing a second time.
        if !super::recording_commands::is_recording().await {
            debug!("📞 Recording is no longer active, call watcher exiting");
            return;
        }

        // Re-read the preference every poll so toggling the setting takes effect
        // during an in-flight recording.
        if !auto_stop_enabled(&app).await {
            armed = false;
            call_seen_since = None;
            call_gone_since = None;
            continue;
        }

        let probe = match tokio::task::spawn_blocking(probe_active_calls).await {
            Ok(probe) => probe,
            Err(e) => {
                warn!("📞 Call detection probe panicked: {}", e);
                continue;
            }
        };

        let apps = match probe {
            CallProbe::Apps(apps) => apps,
            CallProbe::Unsupported(reason) => {
                info!(
                    "📞 Call detection unavailable ({}), auto-stop on call end disabled for this recording",
                    reason
                );
                return;
            }
        };

        if cancel.load(Ordering::SeqCst) {
            return;
        }

        if apps.is_empty() {
            call_seen_since = None;

            if !armed {
                continue;
            }

            let gone_since = *call_gone_since.get_or_insert_with(Instant::now);
            if gone_since.elapsed() < END_GRACE {
                continue;
            }

            info!(
                "📞 Call ended ({}), stopping recording automatically",
                last_apps.join(", ")
            );

            // Detach before stopping: `stop_recording` cancels the watcher, and we do
            // not want the flag we are about to set to abort our own stop sequence.
            detach(&cancel);
            auto_stop(&app, &last_apps).await;
            return;
        }

        call_gone_since = None;
        last_apps = apps;

        if armed {
            continue;
        }

        let seen_since = *call_seen_since.get_or_insert_with(Instant::now);
        if seen_since.elapsed() >= ARM_AFTER {
            armed = true;
            info!(
                "📞 Call detected ({}), recording will stop automatically when it ends",
                last_apps.join(", ")
            );
        }
    }
}

/// Remove this watcher's cancellation flag from the global slot, if it is still the
/// active one.
fn detach(cancel: &Arc<AtomicBool>) {
    let mut guard = match WATCHER_CANCEL.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.as_ref().is_some_and(|current| Arc::ptr_eq(current, cancel)) {
        guard.take();
    }
}

async fn auto_stop_enabled<R: Runtime>(app: &AppHandle<R>) -> bool {
    match super::recording_preferences::load_recording_preferences(app).await {
        Ok(prefs) => prefs.auto_stop_on_call_end,
        Err(e) => {
            warn!("📞 Failed to read auto-stop preference: {}", e);
            false
        }
    }
}

/// Stop the recording the same way the tray menu does, then hand off to the
/// frontend for post-processing.
async fn auto_stop<R: Runtime>(app: &AppHandle<R>, apps: &[String]) {
    let data_dir = match app.path().app_data_dir() {
        Ok(dir) => dir,
        Err(e) => {
            error!("📞 Auto-stop failed to resolve app data dir: {}", e);
            return;
        }
    };

    let timestamp = chrono::Local::now().format("%Y-%m-%dT%H-%M-%S").to_string();
    let save_path = data_dir.join(format!("recording-{}.wav", timestamp));

    // Let the UI explain why the recording is ending before the stop sequence runs.
    let _ = app.emit("recording-auto-stopped", apps.to_vec());

    let result = super::recording_commands::stop_recording(
        app.clone(),
        super::recording_commands::RecordingArgs {
            save_path: save_path.to_string_lossy().to_string(),
        },
    )
    .await;

    match result {
        Ok(_) => {
            info!("📞 Recording stopped automatically after call end");
            if let Err(e) = app.emit("recording-stop-complete", true) {
                error!("📞 Failed to emit recording-stop-complete: {}", e);
            }
        }
        Err(e) => error!("📞 Auto-stop failed to stop recording: {}", e),
    }
}

// ============================================================================
// PLATFORM PROBES
// ============================================================================

/// Meeting apps we recognise, matched case-insensitively as a substring of the
/// platform's process identifier (bundle id on macOS, executable name elsewhere).
///
/// Browsers are included because Google Meet, Teams web and friends run inside them;
/// a browser only shows up here while it actually holds the microphone.
const MEETING_APPS: &[(&str, &str)] = &[
    ("zoom", "Zoom"),
    ("teams", "Microsoft Teams"),
    ("skypeforbusiness", "Skype for Business"),
    ("lync", "Skype for Business"),
    ("skype", "Skype"),
    ("webex", "Webex"),
    ("cisco-systems.spark", "Webex"),
    ("atmgr", "Webex"),
    ("chrome", "Chrome"),
    ("chromium", "Chromium"),
    ("safari", "Safari"),
    ("firefox", "Firefox"),
    ("edgemac", "Microsoft Edge"),
    ("msedge", "Microsoft Edge"),
    ("brave", "Brave"),
    ("thebrowser.browser", "Arc"),
    ("vivaldi", "Vivaldi"),
    ("opera", "Opera"),
    ("slack", "Slack"),
    ("discord", "Discord"),
    ("facetime", "FaceTime"),
    ("avconferenced", "FaceTime"),
    ("gotomeeting", "GoToMeeting"),
    ("bluejeans", "BlueJeans"),
    ("ringcentral", "RingCentral"),
    ("chime", "Amazon Chime"),
    ("whereby", "Whereby"),
    ("dialpad", "Dialpad"),
    ("jitsi", "Jitsi"),
    ("element", "Element"),
];

/// Map a platform process identifier to a meeting app display name.
fn match_meeting_app(identifier: &str) -> Option<&'static str> {
    let lowered = identifier.to_lowercase();
    MEETING_APPS
        .iter()
        .find(|(pattern, _)| lowered.contains(pattern))
        .map(|(_, name)| *name)
}

fn push_app(apps: &mut Vec<String>, name: &str) {
    if !apps.iter().any(|existing| existing == name) {
        apps.push(name.to_string());
    }
}

/// Probe the OS for meeting apps that are currently in a call.
pub fn probe_active_calls() -> CallProbe {
    #[cfg(target_os = "macos")]
    {
        probe_macos()
    }

    #[cfg(target_os = "windows")]
    {
        probe_windows()
    }

    #[cfg(target_os = "linux")]
    {
        probe_linux()
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        CallProbe::Unsupported("platform not supported".to_string())
    }
}

/// macOS: ask Core Audio which processes are currently capturing input.
///
/// Requires the process-object API added in macOS 14.2; on older systems the
/// property query fails and detection reports itself as unsupported.
#[cfg(target_os = "macos")]
fn probe_macos() -> CallProbe {
    use cidre::core_audio as ca;

    let processes = match ca::System::processes() {
        Ok(processes) => processes,
        Err(e) => {
            return CallProbe::Unsupported(format!(
                "Core Audio process list unavailable (requires macOS 14.2+): {:?}",
                e
            ))
        }
    };

    let own_pid = std::process::id() as i32;
    let mut apps = Vec::new();

    for process in processes {
        let pid = match process.pid() {
            Ok(pid) => pid,
            Err(_) => continue,
        };

        // Our own capture must never count as a call.
        if pid == own_pid {
            continue;
        }

        if !process.is_running_input().unwrap_or(false) {
            continue;
        }

        let identifier = match process.bundle_id() {
            Ok(bundle_id) => bundle_id.to_string(),
            Err(_) => cidre::ns::RunningApp::with_pid(pid)
                .and_then(|app| app.localized_name())
                .map(|name| name.to_string())
                .unwrap_or_default(),
        };

        if identifier.is_empty() {
            continue;
        }

        match match_meeting_app(&identifier) {
            Some(name) => push_app(&mut apps, name),
            None => debug!("📞 Ignoring non-meeting app using the microphone: {}", identifier),
        }
    }

    CallProbe::Apps(apps)
}

/// Windows: the capability-access consent store records microphone usage per app.
/// A `LastUsedTimeStop` of 0 means the app is holding the microphone right now.
#[cfg(target_os = "windows")]
fn probe_windows() -> CallProbe {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CONSENT_STORE: &str = r"HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";

    let output = match std::process::Command::new("reg")
        .args(["query", CONSENT_STORE, "/s"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(output) => output,
        Err(e) => {
            return CallProbe::Unsupported(format!("could not query the consent store: {}", e))
        }
    };

    if !output.status.success() {
        return CallProbe::Unsupported("microphone consent store is not present".to_string());
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut apps = Vec::new();
    let mut current_key = String::new();

    for line in text.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("HKEY_") {
            // Subkeys are app identities: a package family name, or an executable path
            // with '#' standing in for the path separator.
            current_key = trimmed
                .rsplit(['\\', '#'])
                .next()
                .unwrap_or_default()
                .to_string();
            continue;
        }

        if !trimmed.starts_with("LastUsedTimeStop") || current_key.is_empty() {
            continue;
        }

        // "LastUsedTimeStop    REG_QWORD    0x0" - zero means "still in use".
        let value = trimmed.rsplit_whitespace().next().unwrap_or_default();
        let in_use = value
            .strip_prefix("0x")
            .and_then(|hex| u64::from_str_radix(hex, 16).ok())
            .is_some_and(|stopped_at| stopped_at == 0);

        if !in_use {
            continue;
        }

        match match_meeting_app(&current_key) {
            Some(name) => push_app(&mut apps, name),
            None => debug!("📞 Ignoring non-meeting app using the microphone: {}", current_key),
        }
    }

    CallProbe::Apps(apps)
}

/// Linux: PulseAudio / PipeWire expose one "source output" per app recording audio.
#[cfg(target_os = "linux")]
fn probe_linux() -> CallProbe {
    // LC_ALL=C keeps the field names we parse below out of the user's locale.
    let output = match std::process::Command::new("pactl")
        .args(["list", "source-outputs"])
        .env("LC_ALL", "C")
        .output()
    {
        Ok(output) => output,
        Err(e) => return CallProbe::Unsupported(format!("pactl is unavailable: {}", e)),
    };

    if !output.status.success() {
        return CallProbe::Unsupported("pactl could not list source outputs".to_string());
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut apps = Vec::new();
    let mut corked = false;
    let mut identifiers: Vec<String> = Vec::new();

    // Each "Source Output #N" block describes one app capturing audio. Corked streams
    // are open but idle, so they do not count as an active call.
    fn flush(corked: bool, identifiers: &mut Vec<String>, apps: &mut Vec<String>) {
        if !corked {
            for identifier in identifiers.iter() {
                if let Some(name) = match_meeting_app(identifier) {
                    push_app(apps, name);
                }
            }
        }
        identifiers.clear();
    }

    for line in text.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("Source Output #") {
            flush(corked, &mut identifiers, &mut apps);
            corked = false;
            continue;
        }

        if let Some(value) = trimmed.strip_prefix("Corked:") {
            corked = value.trim() == "yes";
            continue;
        }

        for key in ["application.process.binary", "application.name"] {
            if let Some(value) = trimmed.strip_prefix(key) {
                if let Some((_, value)) = value.split_once('=') {
                    identifiers.push(value.trim().trim_matches('"').to_string());
                }
            }
        }
    }

    flush(corked, &mut identifiers, &mut apps);

    CallProbe::Apps(apps)
}

// ============================================================================
// COMMANDS
// ============================================================================

#[derive(serde::Serialize)]
pub struct CallDetectionSupport {
    pub supported: bool,
    pub reason: Option<String>,
}

/// Report whether call detection works on this machine, so settings can explain
/// why the toggle would have no effect.
#[tauri::command]
pub async fn get_call_detection_support() -> Result<CallDetectionSupport, String> {
    let probe = tokio::task::spawn_blocking(probe_active_calls)
        .await
        .map_err(|e| format!("Call detection probe failed: {}", e))?;

    Ok(match probe {
        CallProbe::Apps(_) => CallDetectionSupport {
            supported: true,
            reason: None,
        },
        CallProbe::Unsupported(reason) => CallDetectionSupport {
            supported: false,
            reason: Some(reason),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_known_meeting_apps() {
        assert_eq!(match_meeting_app("us.zoom.xos"), Some("Zoom"));
        assert_eq!(match_meeting_app("com.microsoft.teams2"), Some("Microsoft Teams"));
        assert_eq!(match_meeting_app("Chrome.exe"), Some("Chrome"));
        assert_eq!(match_meeting_app("com.tinyspeck.slackmacgap"), Some("Slack"));
    }

    #[test]
    fn ignores_unrelated_apps() {
        assert_eq!(match_meeting_app("com.apple.VoiceMemos"), None);
        assert_eq!(match_meeting_app("com.spotify.client"), None);
    }

    #[test]
    #[ignore] // Run manually: reports what this machine currently sees as a call
    fn probe_reports_current_calls() {
        match probe_active_calls() {
            CallProbe::Apps(apps) => println!("Meeting apps in a call: {:?}", apps),
            CallProbe::Unsupported(reason) => println!("Call detection unsupported: {}", reason),
        }

        // Raw view of what the OS reports, for tuning MEETING_APPS.
        #[cfg(target_os = "macos")]
        {
            use cidre::core_audio as ca;
            for process in ca::System::processes().expect("process list") {
                let identifier = process
                    .bundle_id()
                    .map(|id| id.to_string())
                    .unwrap_or_else(|_| "<no bundle id>".to_string());
                println!(
                    "{:<45} input={} output={}",
                    identifier,
                    process.is_running_input().unwrap_or(false),
                    process.is_running_output().unwrap_or(false),
                );
            }
        }
    }

    #[test]
    fn push_app_deduplicates() {
        let mut apps = Vec::new();
        push_app(&mut apps, "Zoom");
        push_app(&mut apps, "Zoom");
        push_app(&mut apps, "Slack");
        assert_eq!(apps, vec!["Zoom".to_string(), "Slack".to_string()]);
    }
}
