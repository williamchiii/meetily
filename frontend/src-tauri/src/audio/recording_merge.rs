// audio/recording_merge.rs
//
// Fold a resumed recording's folder into the meeting it continues.
//
// Resuming a meeting appends the new transcripts to the existing meeting row, but the
// audio lands in a folder of its own. Without this the meeting's `folder_path` still
// points at only the first stretch, so playback and retranscription silently cover
// half the conversation.
//
// The merge concatenates the two audio files, updates the sidecar metadata.json and
// transcripts.json, and then retires the now-redundant folder.
//
// Safety rules this module holds to:
//   - Nothing is removed until the merged audio has been written AND its duration
//     verified against the sum of its parts.
//   - The folder being retired must live inside the user's recordings folder; a path
//     anywhere else is refused rather than deleted.
//   - Retiring means moving to the Trash, never an unrecoverable delete.

use anyhow::{anyhow, Context, Result};
use log::{info, warn};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Tolerance when checking the merged duration against its parts. Container
/// timestamps and encoder padding make this inexact by a frame or two.
const DURATION_TOLERANCE_SECS: f64 = 1.5;

#[derive(Debug, Clone, Serialize)]
pub struct MergeReport {
    /// Whether audio was actually concatenated.
    pub merged: bool,
    /// Human-readable account of what happened, for logs and the UI.
    pub detail: String,
    /// Duration of the merged recording, when one was produced.
    pub duration_seconds: Option<f64>,
    /// Where the retired folder went, if it was retired.
    pub retired_to: Option<String>,
}

impl MergeReport {
    fn skipped(detail: impl Into<String>) -> Self {
        Self {
            merged: false,
            detail: detail.into(),
            duration_seconds: None,
            retired_to: None,
        }
    }
}

/// Merge `resumed_folder`'s recording into `meeting_folder`.
///
/// `recordings_root` is the user's configured recordings folder; nothing outside it
/// is ever retired.
///
/// Audio is only one of the things being folded together, and it is optional - with
/// "Save Audio Recordings" switched off there is no audio.mp4 at all, but the
/// transcripts.json/metadata.json sidecars and the leftover folder still need
/// reconciling. Treating audio as a precondition would leave an orphan folder behind
/// after every single resume.
///
/// Returns a report rather than an error when there is genuinely nothing to do;
/// errors are reserved for a merge that was attempted and failed.
pub fn merge_recording_folders(
    meeting_folder: &Path,
    resumed_folder: &Path,
    recordings_root: &Path,
    home_dir: Option<&Path>,
) -> Result<MergeReport> {
    if meeting_folder == resumed_folder {
        return Ok(MergeReport::skipped(
            "Resumed recording already saved into the meeting folder",
        ));
    }

    if !meeting_folder.is_dir() {
        return Ok(MergeReport::skipped(format!(
            "Meeting folder no longer exists: {}",
            meeting_folder.display()
        )));
    }

    if !resumed_folder.is_dir() {
        return Ok(MergeReport::skipped(format!(
            "Resumed recording folder no longer exists: {}",
            resumed_folder.display()
        )));
    }

    let original_audio = meeting_folder.join("audio.mp4");
    let resumed_audio = resumed_folder.join("audio.mp4");

    let merged_duration = if !resumed_audio.is_file() {
        // Auto-save off, or a take too short to produce audio. The sidecars below
        // still describe the conversation, so the merge carries on without it.
        info!(
            "No audio in {}, merging transcripts and metadata only",
            resumed_folder.display()
        );
        None
    } else if original_audio.is_file() {
        Some(concat_audio(&original_audio, &resumed_audio, meeting_folder)?)
    } else {
        // The meeting never had audio of its own, so the resumed take becomes it
        std::fs::copy(&resumed_audio, &original_audio)
            .with_context(|| format!("copying audio into {}", meeting_folder.display()))?;
        Some(probe_duration(&original_audio).unwrap_or(0.0))
    };

    merge_transcript_sidecars(meeting_folder, resumed_folder)?;

    if let Some(duration) = merged_duration {
        update_metadata_duration(meeting_folder, duration)?;
    }

    let retired_to = retire_folder(resumed_folder, recordings_root, home_dir)
        .map_err(|e| warn!("Merged {} but could not retire it: {}", resumed_folder.display(), e))
        .ok()
        .flatten();

    let detail = match merged_duration {
        Some(duration) => format!(
            "Merged resumed recording into {} ({:.1}s of audio)",
            meeting_folder.display(),
            duration
        ),
        None => format!(
            "Merged resumed transcripts into {} (no audio was saved)",
            meeting_folder.display()
        ),
    };

    Ok(MergeReport {
        merged: true,
        detail,
        duration_seconds: merged_duration,
        retired_to: retired_to.map(|p| p.display().to_string()),
    })
}

// ============================================================================
// AUDIO
// ============================================================================

/// Concatenate `second` onto `first` in place, returning the merged duration.
///
/// The original is only replaced once the merged file exists and its duration adds
/// up, so a failed or truncated encode leaves the meeting's audio untouched.
fn concat_audio(first: &Path, second: &Path, work_dir: &Path) -> Result<f64> {
    let ffmpeg = crate::audio::ffmpeg::find_ffmpeg_path()
        .ok_or_else(|| anyhow!("ffmpeg is unavailable, cannot merge recordings"))?;

    let expected = probe_duration(first).unwrap_or(0.0) + probe_duration(second).unwrap_or(0.0);

    let merged_tmp = work_dir.join(".audio.merged.tmp.mp4");
    let list_file = work_dir.join(".concat-list.txt");

    // ffmpeg's concat demuxer takes a file of paths; single quotes are escaped by
    // doubling, per its own quoting rules
    let list = format!(
        "file '{}'\nfile '{}'\n",
        first.display().to_string().replace('\'', r"'\''"),
        second.display().to_string().replace('\'', r"'\''")
    );
    std::fs::write(&list_file, list).context("writing ffmpeg concat list")?;

    // Both files come from the same encoder with the same settings, so a stream copy
    // works and avoids a generational quality loss
    let copied = run_ffmpeg(
        &ffmpeg,
        &[
            "-hide_banner", "-loglevel", "error", "-y",
            "-f", "concat", "-safe", "0",
            "-i", &list_file.display().to_string(),
            "-c", "copy",
            &merged_tmp.display().to_string(),
        ],
    );

    let stream_copy_ok = copied.is_ok() && duration_matches(&merged_tmp, expected);

    // The concat list is only used by the stream-copy attempt above; clean it up now
    // regardless of what happens next, so a re-encode failure below doesn't skip it.
    let _ = std::fs::remove_file(&list_file);

    if !stream_copy_ok {
        // Parameters differ (a device change mid-meeting, an imported file); fall
        // back to re-encoding, which does not care whether they line up
        warn!("Stream-copy concat unusable, re-encoding the merge");
        run_ffmpeg(
            &ffmpeg,
            &[
                "-hide_banner", "-loglevel", "error", "-y",
                "-i", &first.display().to_string(),
                "-i", &second.display().to_string(),
                "-filter_complex", "[0:a][1:a]concat=n=2:v=0:a=1[out]",
                "-map", "[out]",
                "-c:a", "aac", "-b:a", "192k",
                &merged_tmp.display().to_string(),
            ],
        )
        .context("re-encoding merged audio")?;
    }

    let merged_duration = probe_duration(&merged_tmp)
        .ok_or_else(|| anyhow!("merged audio has no readable duration"))?;

    if (merged_duration - expected).abs() > DURATION_TOLERANCE_SECS {
        let _ = std::fs::remove_file(&merged_tmp);
        return Err(anyhow!(
            "merged audio is {:.2}s but the parts total {:.2}s; leaving the original alone",
            merged_duration,
            expected
        ));
    }

    // Swap via a backup so a failure between the two renames still leaves the
    // original recoverable
    let backup = work_dir.join(".audio.premerge.mp4");
    std::fs::rename(first, &backup).context("backing up the original audio")?;

    if let Err(e) = std::fs::rename(&merged_tmp, first) {
        let _ = std::fs::rename(&backup, first);
        return Err(anyhow::Error::new(e).context("installing the merged audio"));
    }

    let _ = std::fs::remove_file(&backup);

    info!(
        "Merged audio: {:.2}s + {:.2}s = {:.2}s",
        expected - probe_duration(second).unwrap_or(0.0),
        probe_duration(second).unwrap_or(0.0),
        merged_duration
    );

    Ok(merged_duration)
}

fn run_ffmpeg(ffmpeg: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new(ffmpeg)
        .args(args)
        .output()
        .context("running ffmpeg")?;

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        Ok(stderr)
    } else {
        Err(anyhow!("ffmpeg failed: {}", stderr.trim()))
    }
}

fn duration_matches(path: &Path, expected: f64) -> bool {
    probe_duration(path)
        .map(|actual| (actual - expected).abs() <= DURATION_TOLERANCE_SECS)
        .unwrap_or(false)
}

/// Read a media file's duration by asking ffmpeg to describe it.
///
/// ffmpeg reports the input then exits non-zero for want of an output, so the
/// interesting text is on stderr either way.
fn probe_duration(path: &Path) -> Option<f64> {
    let ffmpeg = crate::audio::ffmpeg::find_ffmpeg_path()?;

    let output = std::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-i", &path.display().to_string()])
        .output()
        .ok()?;

    parse_ffmpeg_duration(&String::from_utf8_lossy(&output.stderr))
}

/// Pull `Duration: HH:MM:SS.ss` out of ffmpeg's description of an input.
pub fn parse_ffmpeg_duration(stderr: &str) -> Option<f64> {
    let after = stderr.split("Duration:").nth(1)?;
    let stamp = after.split(',').next()?.trim();

    if stamp.starts_with("N/A") {
        return None;
    }

    let mut parts = stamp.split(':');
    let hours: f64 = parts.next()?.trim().parse().ok()?;
    let minutes: f64 = parts.next()?.trim().parse().ok()?;
    let seconds: f64 = parts.next()?.trim().parse().ok()?;

    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

// ============================================================================
// SIDECARS
// ============================================================================

/// Append the resumed recording's transcript segments to the meeting's sidecar,
/// shifted onto the merged timeline the same way the database rows were.
fn merge_transcript_sidecars(meeting_folder: &Path, resumed_folder: &Path) -> Result<()> {
    let target_path = meeting_folder.join("transcripts.json");
    let source_path = resumed_folder.join("transcripts.json");

    if !source_path.is_file() {
        return Ok(());
    }

    let mut target: serde_json::Value = if target_path.is_file() {
        serde_json::from_slice(&std::fs::read(&target_path)?).unwrap_or_else(|_| empty_sidecar())
    } else {
        empty_sidecar()
    };

    let source: serde_json::Value = match serde_json::from_slice(&std::fs::read(&source_path)?) {
        Ok(value) => value,
        Err(e) => {
            warn!("Resumed transcripts.json is unreadable, skipping sidecar merge: {}", e);
            return Ok(());
        }
    };

    let existing = target["segments"].as_array().cloned().unwrap_or_default();
    let incoming = source["segments"].as_array().cloned().unwrap_or_default();

    let offset = existing
        .iter()
        .filter_map(|s| s["audio_end_time"].as_f64().or_else(|| s["audio_start_time"].as_f64()))
        .filter(|v| v.is_finite() && *v > 0.0)
        .fold(0.0f64, f64::max);

    let mut merged = existing;
    for mut segment in incoming {
        for key in ["audio_start_time", "audio_end_time"] {
            if let Some(value) = segment[key].as_f64() {
                segment[key] = serde_json::json!(value + offset);
            }
        }
        segment["sequence_id"] = serde_json::json!(merged.len());
        merged.push(segment);
    }

    target["total_segments"] = serde_json::json!(merged.len());
    target["segments"] = serde_json::Value::Array(merged);
    target["last_updated"] = serde_json::json!(chrono::Utc::now().to_rfc3339());

    write_json_atomically(&target_path, &target)
}

fn empty_sidecar() -> serde_json::Value {
    serde_json::json!({
        "version": "1.0",
        "segments": [],
        "total_segments": 0,
    })
}

/// Bring metadata.json in line with the merged recording.
fn update_metadata_duration(meeting_folder: &Path, duration_seconds: f64) -> Result<()> {
    let path = meeting_folder.join("metadata.json");

    if !path.is_file() {
        return Ok(());
    }

    let mut metadata: serde_json::Value = match serde_json::from_slice(&std::fs::read(&path)?) {
        Ok(value) => value,
        Err(e) => {
            warn!("metadata.json is unreadable, leaving it as is: {}", e);
            return Ok(());
        }
    };

    metadata["duration_seconds"] = serde_json::json!((duration_seconds * 100.0).round() / 100.0);
    metadata["completed_at"] = serde_json::json!(chrono::Utc::now().to_rfc3339());
    metadata["status"] = serde_json::json!("completed");
    metadata["audio_file"] = serde_json::json!("audio.mp4");

    write_json_atomically(&path, &metadata)
}

fn write_json_atomically(path: &Path, value: &serde_json::Value) -> Result<()> {
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, serde_json::to_string_pretty(value)?)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

// ============================================================================
// RETIRING THE MERGED-FROM FOLDER
// ============================================================================

/// Move a folder whose contents now live elsewhere into the Trash.
///
/// Refuses anything outside `recordings_root`, and prefers the Trash to an outright
/// delete so a bad merge is always recoverable by hand.
///
/// The root is passed in rather than derived here: recordings go to the user's
/// configured save folder, which the Settings UI can move away from the per-platform
/// default, and deriving the wrong root would refuse every legitimate retirement.
fn retire_folder(
    folder: &Path,
    recordings_root: &Path,
    home_dir: Option<&Path>,
) -> Result<Option<PathBuf>> {
    let folder_real = folder.canonicalize().context("resolving folder to retire")?;
    let root_real = recordings_root
        .canonicalize()
        .unwrap_or_else(|_| recordings_root.to_path_buf());

    if !folder_real.starts_with(&root_real) || folder_real == root_real {
        return Err(anyhow!(
            "refusing to retire {}, which is not a recording folder",
            folder_real.display()
        ));
    }

    let Some(trash) = trash_dir(home_dir) else {
        return Ok(None);
    };
    std::fs::create_dir_all(&trash).ok();

    let name = folder_real
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "meetily-recording".to_string());

    // Trash may already hold a folder by this name from an earlier merge
    let mut destination = trash.join(&name);
    let mut suffix = 1;
    while destination.exists() {
        destination = trash.join(format!("{} ({})", name, suffix));
        suffix += 1;
    }

    std::fs::rename(&folder_real, &destination)
        .with_context(|| format!("moving {} to the Trash", folder_real.display()))?;

    info!("Retired merged recording folder to {}", destination.display());
    Ok(Some(destination))
}

/// The user's Trash, on platforms that have one we can move into directly.
/// `home_dir` should come from Tauri's path resolver (`AppHandle::path().home_dir()`)
/// rather than being resolved here, per this project's path-API convention.
fn trash_dir(home_dir: Option<&Path>) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        home_dir.map(|home| home.join(".Trash"))
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = home_dir;
        // Elsewhere the merged-from folder is left in place rather than guessing at
        // a trash implementation; the caller reports the path instead.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ffmpeg_duration() {
        let stderr = "Input #0, mov,mp4,m4a,3gp,3g2,mj2, from 'audio.mp4':\n  \
                      Duration: 00:02:53.42, start: 0.000000, bitrate: 198 kb/s\n  \
                      Stream #0:0[0x1](und): Audio: aac (LC)";
        let seconds = parse_ffmpeg_duration(stderr).unwrap();
        assert!((seconds - 173.42).abs() < 0.001, "got {}", seconds);
    }

    #[test]
    fn parses_durations_over_an_hour() {
        let seconds = parse_ffmpeg_duration("  Duration: 01:30:05.50, start: 0.0").unwrap();
        assert!((seconds - 5405.5).abs() < 0.001, "got {}", seconds);
    }

    #[test]
    fn rejects_unreadable_durations() {
        assert_eq!(parse_ffmpeg_duration(""), None);
        assert_eq!(parse_ffmpeg_duration("Duration: N/A, start: 0.0"), None);
        assert_eq!(parse_ffmpeg_duration("no duration here"), None);
        assert_eq!(parse_ffmpeg_duration("Duration: garbage,"), None);
    }

    /// Generate a real AAC/mp4 file matching what the recorder produces, so the
    /// concat path is exercised against actual media rather than a stand-in.
    fn write_tone(path: &Path, seconds: f64) -> bool {
        let Some(ffmpeg) = crate::audio::ffmpeg::find_ffmpeg_path() else {
            return false;
        };

        std::process::Command::new(ffmpeg)
            .args([
                "-hide_banner", "-loglevel", "error", "-y",
                "-f", "lavfi",
                "-i", &format!("sine=frequency=440:sample_rate=48000:duration={}", seconds),
                "-ac", "1", "-c:a", "aac", "-b:a", "196k",
                &path.display().to_string(),
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn merges_two_real_recordings_end_to_end() {
        let meeting = tempfile::tempdir().unwrap();
        let resumed = tempfile::tempdir().unwrap();

        if !write_tone(&meeting.path().join("audio.mp4"), 4.0)
            || !write_tone(&resumed.path().join("audio.mp4"), 3.0)
        {
            eprintln!("skipping: ffmpeg unavailable in this environment");
            return;
        }

        std::fs::write(
            meeting.path().join("metadata.json"),
            serde_json::json!({ "version": "1.0", "duration_seconds": 4.0 }).to_string(),
        )
        .unwrap();

        let report =
            merge_recording_folders(meeting.path(), resumed.path(), meeting.path(), None).unwrap();

        assert!(report.merged, "{}", report.detail);

        let duration = report.duration_seconds.unwrap();
        assert!(
            (duration - 7.0).abs() < DURATION_TOLERANCE_SECS,
            "merged duration was {:.3}s, expected about 7s",
            duration
        );

        // The merged file really is the meeting's audio now
        let installed = probe_duration(&meeting.path().join("audio.mp4")).unwrap();
        assert!((installed - 7.0).abs() < DURATION_TOLERANCE_SECS, "got {}", installed);

        // No scratch files left lying around next to the recording
        for leftover in [".audio.merged.tmp.mp4", ".audio.premerge.mp4", ".concat-list.txt"] {
            assert!(
                !meeting.path().join(leftover).exists(),
                "{} should have been cleaned up",
                leftover
            );
        }

        // metadata.json reflects the whole recording
        let metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(meeting.path().join("metadata.json")).unwrap())
                .unwrap();
        let recorded = metadata["duration_seconds"].as_f64().unwrap();
        assert!((recorded - 7.0).abs() < DURATION_TOLERANCE_SECS, "got {}", recorded);

        // The resumed folder sits outside the recordings root, so it is refused
        // rather than trashed - and must therefore still be intact
        assert!(resumed.path().join("audio.mp4").is_file());
        assert_eq!(report.retired_to, None);
    }

    #[test]
    fn a_meeting_with_no_audio_adopts_the_resumed_take() {
        let meeting = tempfile::tempdir().unwrap();
        let resumed = tempfile::tempdir().unwrap();

        if !write_tone(&resumed.path().join("audio.mp4"), 3.0) {
            eprintln!("skipping: ffmpeg unavailable in this environment");
            return;
        }

        let report =
            merge_recording_folders(meeting.path(), resumed.path(), meeting.path(), None).unwrap();

        assert!(report.merged, "{}", report.detail);
        let duration = probe_duration(&meeting.path().join("audio.mp4")).unwrap();
        assert!((duration - 3.0).abs() < DURATION_TOLERANCE_SECS, "got {}", duration);
    }

    #[test]
    fn a_corrupt_resumed_file_leaves_the_original_intact() {
        let meeting = tempfile::tempdir().unwrap();
        let resumed = tempfile::tempdir().unwrap();

        if !write_tone(&meeting.path().join("audio.mp4"), 4.0) {
            eprintln!("skipping: ffmpeg unavailable in this environment");
            return;
        }
        std::fs::write(resumed.path().join("audio.mp4"), b"this is not media").unwrap();

        // Whether this errors or salvages something, the meeting's own audio must
        // never be left destroyed or truncated
        let _ = merge_recording_folders(meeting.path(), resumed.path(), meeting.path(), None);

        let surviving = probe_duration(&meeting.path().join("audio.mp4"));
        assert!(
            surviving.map(|d| (d - 4.0).abs() < DURATION_TOLERANCE_SECS).unwrap_or(false),
            "original audio was damaged, got {:?}",
            surviving
        );
        assert!(!meeting.path().join(".audio.premerge.mp4").exists());
    }

    /// The regression this module shipped with: "Save Audio Recordings" is off, so
    /// no audio.mp4 ever exists. Audio must not be a precondition for the rest of the
    /// merge, or every resume strands an orphan folder.
    #[test]
    fn merges_sidecars_and_retires_the_folder_when_no_audio_was_saved() {
        let root = tempfile::tempdir().unwrap();
        let meeting = root.path().join("Meeting A");
        let resumed = root.path().join("Meeting B");
        std::fs::create_dir_all(&meeting).unwrap();
        std::fs::create_dir_all(&resumed).unwrap();

        // Exactly what an auto_save=false folder holds: sidecars, no audio
        std::fs::write(
            meeting.join("transcripts.json"),
            serde_json::json!({
                "segments": [{ "text": "first", "audio_start_time": 0.0, "audio_end_time": 18.6 }]
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            meeting.join("metadata.json"),
            serde_json::json!({ "version": "1.0", "audio_file": "" }).to_string(),
        )
        .unwrap();
        std::fs::write(
            resumed.join("transcripts.json"),
            serde_json::json!({
                "segments": [{ "text": "second", "audio_start_time": 0.0, "audio_end_time": 24.8 }]
            })
            .to_string(),
        )
        .unwrap();

        let home = tempfile::tempdir().unwrap();
        let report =
            merge_recording_folders(&meeting, &resumed, root.path(), Some(home.path())).unwrap();

        assert!(report.merged, "merge must proceed without audio: {}", report.detail);
        assert_eq!(report.duration_seconds, None, "there was no audio to time");

        // The sidecar really was folded together, on a shifted timeline
        let merged: serde_json::Value =
            serde_json::from_slice(&std::fs::read(meeting.join("transcripts.json")).unwrap())
                .unwrap();
        let segments = merged["segments"].as_array().unwrap();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[1]["text"], "second");
        assert_eq!(segments[1]["audio_start_time"], 18.6);

        // And the leftover folder did not survive as an orphan
        assert!(
            report.retired_to.is_some(),
            "the resumed folder should have been retired, got {:?}",
            report.retired_to
        );
        assert!(!resumed.exists(), "resumed folder should be gone");
    }

    #[test]
    fn retirement_honours_a_relocated_recordings_folder() {
        // A user who moved their save folder in Settings must still get retirement
        let root = tempfile::tempdir().unwrap();
        let resumed = root.path().join("Meeting B");
        std::fs::create_dir_all(&resumed).unwrap();

        // The folder lives under the configured root, so this is allowed
        assert!(retire_folder(&resumed, root.path(), None).is_ok());
    }

    #[test]
    fn identical_folders_are_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let report = merge_recording_folders(dir.path(), dir.path(), dir.path(), None).unwrap();
        assert!(!report.merged);
    }

    #[test]
    fn a_resumed_folder_without_audio_still_merges_and_leaves_existing_audio_alone() {
        let root = tempfile::tempdir().unwrap();
        let meeting = root.path().join("Meeting A");
        let resumed = root.path().join("Meeting B");
        std::fs::create_dir_all(&meeting).unwrap();
        std::fs::create_dir_all(&resumed).unwrap();

        // The meeting has audio from before auto-save was turned off; the resumed
        // take has none. There is nothing to concatenate, but the merge proceeds.
        std::fs::write(meeting.join("audio.mp4"), b"existing audio bytes").unwrap();

        let report = merge_recording_folders(&meeting, &resumed, root.path(), None).unwrap();

        assert!(report.merged, "got {}", report.detail);
        assert_eq!(report.duration_seconds, None);
        // Whatever audio the meeting already had must be untouched, never truncated
        assert_eq!(
            std::fs::read(meeting.join("audio.mp4")).unwrap(),
            b"existing audio bytes"
        );
    }

    #[test]
    fn refuses_to_retire_a_folder_outside_the_recordings_root() {
        let stray = tempfile::tempdir().unwrap();
        assert!(retire_folder(stray.path(), stray.path(), None).is_err());
        assert!(stray.path().is_dir(), "the folder must be left alone");
    }

    #[test]
    fn transcript_sidecars_are_appended_on_a_shifted_timeline() {
        let meeting = tempfile::tempdir().unwrap();
        let resumed = tempfile::tempdir().unwrap();

        std::fs::write(
            meeting.path().join("transcripts.json"),
            serde_json::json!({
                "version": "1.0",
                "total_segments": 1,
                "segments": [
                    { "text": "first", "audio_start_time": 0.0, "audio_end_time": 25.0, "sequence_id": 0 }
                ]
            })
            .to_string(),
        )
        .unwrap();

        std::fs::write(
            resumed.path().join("transcripts.json"),
            serde_json::json!({
                "version": "1.0",
                "total_segments": 1,
                "segments": [
                    { "text": "second", "audio_start_time": 0.0, "audio_end_time": 8.0, "sequence_id": 0 }
                ]
            })
            .to_string(),
        )
        .unwrap();

        merge_transcript_sidecars(meeting.path(), resumed.path()).unwrap();

        let merged: serde_json::Value =
            serde_json::from_slice(&std::fs::read(meeting.path().join("transcripts.json")).unwrap())
                .unwrap();

        assert_eq!(merged["total_segments"], 2);
        let segments = merged["segments"].as_array().unwrap();
        assert_eq!(segments[0]["text"], "first");
        assert_eq!(segments[1]["text"], "second");
        // Shifted past the 25s already on the timeline, not left overlapping at zero
        assert_eq!(segments[1]["audio_start_time"], 25.0);
        assert_eq!(segments[1]["audio_end_time"], 33.0);
        assert_eq!(segments[1]["sequence_id"], 1);
    }

    #[test]
    fn a_missing_target_sidecar_is_created_from_the_resumed_one() {
        let meeting = tempfile::tempdir().unwrap();
        let resumed = tempfile::tempdir().unwrap();

        std::fs::write(
            resumed.path().join("transcripts.json"),
            serde_json::json!({
                "segments": [{ "text": "only", "audio_start_time": 0.0, "audio_end_time": 4.0 }]
            })
            .to_string(),
        )
        .unwrap();

        merge_transcript_sidecars(meeting.path(), resumed.path()).unwrap();

        let merged: serde_json::Value =
            serde_json::from_slice(&std::fs::read(meeting.path().join("transcripts.json")).unwrap())
                .unwrap();
        assert_eq!(merged["total_segments"], 1);
        // Nothing to shift past, so it keeps its own timeline
        assert_eq!(merged["segments"][0]["audio_start_time"], 0.0);
    }

    #[test]
    fn metadata_duration_is_refreshed() {
        let meeting = tempfile::tempdir().unwrap();
        std::fs::write(
            meeting.path().join("metadata.json"),
            serde_json::json!({
                "version": "1.0",
                "duration_seconds": 130.09,
                "status": "completed",
                "audio_file": "audio.mp4"
            })
            .to_string(),
        )
        .unwrap();

        update_metadata_duration(meeting.path(), 245.678).unwrap();

        let metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(meeting.path().join("metadata.json")).unwrap())
                .unwrap();
        assert_eq!(metadata["duration_seconds"], 245.68);
        assert_eq!(metadata["status"], "completed");
        assert!(metadata["completed_at"].is_string());
    }

    #[test]
    fn a_missing_metadata_file_is_not_an_error() {
        let meeting = tempfile::tempdir().unwrap();
        assert!(update_metadata_duration(meeting.path(), 12.0).is_ok());
    }
}
