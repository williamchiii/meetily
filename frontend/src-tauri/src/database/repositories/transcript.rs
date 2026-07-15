use crate::api::{TranscriptSearchResult, TranscriptSegment};
use chrono::{DateTime, Utc};
use sqlx::{Connection, Error as SqlxError, SqlitePool};
use tracing::{error, info};
use uuid::Uuid;

/// When the meeting started, derived from the recording-relative timestamps of
/// its segments: segments carry audio_start/end_time in seconds from recording
/// start, and saving happens at recording stop - so start = stop - recording
/// length. Falls back to `now` when segments carry no audio timestamps.
fn meeting_start_time(now: DateTime<Utc>, transcripts: &[TranscriptSegment]) -> DateTime<Utc> {
    let recording_seconds = transcripts
        .iter()
        .filter_map(|t| t.audio_end_time.or(t.audio_start_time))
        .filter(|s| s.is_finite() && *s >= 0.0)
        .fold(0.0f64, f64::max);
    now - chrono::Duration::milliseconds((recording_seconds * 1000.0) as i64)
}

pub struct TranscriptsRepository;

impl TranscriptsRepository {
    /// Saves a new meeting and its associated transcript segments.
    /// This function uses a transaction to ensure that either both the meeting
    /// and all its transcripts are saved, or none of them are.
    pub async fn save_transcript(
        pool: &SqlitePool,
        meeting_title: &str,
        transcripts: &[TranscriptSegment],
        folder_path: Option<String>,
    ) -> Result<String, SqlxError> {
        let meeting_id = format!("meeting-{}", Uuid::new_v4());

        let mut conn = pool.acquire().await?;
        let mut transaction = conn.begin().await?;

        let now = Utc::now();
        // Date the meeting from when recording STARTED, not when it was saved
        let created_at = meeting_start_time(now, transcripts);

        // 1. Create the new meeting
        let result = sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at, folder_path) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&meeting_id)
        .bind(meeting_title)
        .bind(created_at)
        .bind(now)
        .bind(&folder_path)
        .execute(&mut *transaction)
        .await;

        if let Err(e) = result {
            error!("Failed to create meeting '{}': {}", meeting_title, e);
            transaction.rollback().await?;
            return Err(e);
        }

        info!("Successfully created meeting with id: {}", meeting_id);

        // 2. Save each transcript segment with audio timing fields
        for segment in transcripts {
            let transcript_id = format!("transcript-{}", Uuid::new_v4());
            let result = sqlx::query(
                "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, audio_start_time, audio_end_time, duration)
                 VALUES (?, ?, ?, ?, ?, ?, ?)"
            )
            .bind(&transcript_id)
            .bind(&meeting_id)
            .bind(&segment.text)
            .bind(&segment.timestamp)
            .bind(segment.audio_start_time)
            .bind(segment.audio_end_time)
            .bind(segment.duration)
            .execute(&mut *transaction)
            .await;

            if let Err(e) = result {
                error!(
                    "Failed to save transcript segment for meeting {}: {}",
                    meeting_id, e
                );
                transaction.rollback().await?;
                return Err(e);
            }
        }

        info!(
            "Successfully saved {} transcript segments for meeting {}",
            transcripts.len(),
            meeting_id
        );

        // Commit the transaction
        transaction.commit().await?;

        Ok(meeting_id)
    }

    /// Searches for a query string within the transcripts.
    /// It returns a list of matching transcripts with context.
    pub async fn search_transcripts(
        pool: &SqlitePool,
        query: &str,
    ) -> Result<Vec<TranscriptSearchResult>, SqlxError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        let search_query = format!("%{}%", query.to_lowercase());

        let rows = sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT m.id, m.title, t.transcript, t.timestamp
             FROM meetings m
             JOIN transcripts t ON m.id = t.meeting_id
             WHERE LOWER(t.transcript) LIKE ?",
        )
        .bind(&search_query)
        .fetch_all(pool)
        .await?;

        let results = rows
            .into_iter()
            .map(|(id, title, transcript, timestamp)| {
                let match_context = Self::get_match_context(&transcript, query);
                TranscriptSearchResult {
                    id,
                    title,
                    match_context,
                    timestamp,
                }
            })
            .collect();

        Ok(results)
    }

    /// Helper function to extract a snippet of text around the first match of a query.
    fn get_match_context(transcript: &str, query: &str) -> String {
        let transcript_lower = transcript.to_lowercase();
        let query_lower = query.to_lowercase();

        match transcript_lower.find(&query_lower) {
            Some(match_index) => {
                let start_index = match_index.saturating_sub(100);
                let end_index = (match_index + query.len() + 100).min(transcript.len());

                let mut context = String::new();
                if start_index > 0 {
                    context.push_str("...");
                }
                context.push_str(&transcript[start_index..end_index]);
                if end_index < transcript.len() {
                    context.push_str("...");
                }
                context
            }
            None => transcript.chars().take(200).collect(), // Fallback to the start of the transcript
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(audio_start: Option<f64>, audio_end: Option<f64>) -> TranscriptSegment {
        TranscriptSegment {
            id: "t1".to_string(),
            text: "hello".to_string(),
            timestamp: "14:30:05".to_string(),
            audio_start_time: audio_start,
            audio_end_time: audio_end,
            duration: None,
        }
    }

    #[test]
    fn meeting_start_time_subtracts_recording_length() {
        let now = Utc::now();
        let transcripts = vec![
            segment(Some(0.0), Some(12.5)),
            segment(Some(60.0), Some(90.0)),
            segment(Some(1800.0), Some(1845.5)),
        ];
        let start = meeting_start_time(now, &transcripts);
        assert_eq!((now - start).num_milliseconds(), 1_845_500);
    }

    #[test]
    fn meeting_start_time_falls_back_to_now_without_audio_times() {
        let now = Utc::now();
        assert_eq!(meeting_start_time(now, &[]), now);
        assert_eq!(meeting_start_time(now, &[segment(None, None)]), now);
    }

    #[test]
    fn meeting_start_time_ignores_invalid_values() {
        let now = Utc::now();
        let transcripts = vec![segment(Some(f64::NAN), Some(-5.0)), segment(None, Some(30.0))];
        let start = meeting_start_time(now, &transcripts);
        assert_eq!((now - start).num_milliseconds(), 30_000);
    }
}
