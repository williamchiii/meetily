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

/// Where a resumed recording's segments should pick up on the meeting's timeline,
/// given the furthest point already stored. Segments can carry NULL, NaN or negative
/// audio times, none of which are a usable offset.
fn append_offset(stored_max: Option<f64>) -> f64 {
    stored_max
        .filter(|offset| offset.is_finite() && *offset > 0.0)
        .unwrap_or(0.0)
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

    /// Appends segments to a meeting that already exists, for a recording the user
    /// resumed after it stopped.
    ///
    /// Segment timestamps are relative to the start of *their* recording, and a
    /// resumed recording starts counting from zero again. Meeting transcripts are
    /// read back `ORDER BY audio_start_time`, so the new segments are shifted past
    /// the last one already stored - otherwise the second half of a meeting would
    /// interleave with the first.
    pub async fn append_transcript(
        pool: &SqlitePool,
        meeting_id: &str,
        transcripts: &[TranscriptSegment],
    ) -> Result<(), SqlxError> {
        if meeting_id.trim().is_empty() {
            return Err(SqlxError::Protocol("meeting_id cannot be empty".to_string()));
        }

        let exists: Option<(String,)> = sqlx::query_as("SELECT id FROM meetings WHERE id = ?")
            .bind(meeting_id)
            .fetch_optional(pool)
            .await?;

        if exists.is_none() {
            error!("Cannot append transcripts, meeting {} not found", meeting_id);
            return Err(SqlxError::RowNotFound);
        }

        let (offset,): (Option<f64>,) = sqlx::query_as(
            "SELECT MAX(COALESCE(audio_end_time, audio_start_time)) FROM transcripts WHERE meeting_id = ?",
        )
        .bind(meeting_id)
        .fetch_one(pool)
        .await?;

        let offset = append_offset(offset);

        let mut conn = pool.acquire().await?;
        let mut transaction = conn.begin().await?;

        for segment in transcripts {
            let transcript_id = format!("transcript-{}", Uuid::new_v4());
            let result = sqlx::query(
                "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, audio_start_time, audio_end_time, duration)
                 VALUES (?, ?, ?, ?, ?, ?, ?)"
            )
            .bind(&transcript_id)
            .bind(meeting_id)
            .bind(&segment.text)
            .bind(&segment.timestamp)
            .bind(segment.audio_start_time.map(|t| t + offset))
            .bind(segment.audio_end_time.map(|t| t + offset))
            .bind(segment.duration)
            .execute(&mut *transaction)
            .await;

            if let Err(e) = result {
                error!(
                    "Failed to append transcript segment to meeting {}: {}",
                    meeting_id, e
                );
                transaction.rollback().await?;
                return Err(e);
            }
        }

        let result = sqlx::query("UPDATE meetings SET updated_at = ? WHERE id = ?")
            .bind(Utc::now())
            .bind(meeting_id)
            .execute(&mut *transaction)
            .await;

        if let Err(e) = result {
            error!("Failed to touch meeting {} after append: {}", meeting_id, e);
            transaction.rollback().await?;
            return Err(e);
        }

        transaction.commit().await?;

        info!(
            "Appended {} transcript segments to meeting {} (shifted by {:.2}s)",
            transcripts.len(),
            meeting_id,
            offset
        );

        Ok(())
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

    fn text_segment(text: &str, audio_start: f64, audio_end: f64) -> TranscriptSegment {
        TranscriptSegment {
            id: format!("t-{}", text),
            text: text.to_string(),
            timestamp: "14:30:05".to_string(),
            audio_start_time: Some(audio_start),
            audio_end_time: Some(audio_end),
            duration: Some(audio_end - audio_start),
        }
    }

    /// A real database on the real schema, so the append is exercised against the
    /// same constraints production runs under.
    async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations");
        pool
    }

    /// Transcripts as the meeting page reads them back: ordered by audio_start_time.
    async fn stored_timeline(pool: &SqlitePool, meeting_id: &str) -> Vec<(String, f64, f64)> {
        sqlx::query_as::<_, (String, f64, f64)>(
            "SELECT transcript, audio_start_time, audio_end_time FROM transcripts
             WHERE meeting_id = ? ORDER BY audio_start_time ASC",
        )
        .bind(meeting_id)
        .fetch_all(pool)
        .await
        .expect("read back transcripts")
    }

    #[tokio::test]
    async fn append_continues_the_timeline_instead_of_interleaving() {
        let pool = test_pool().await;

        let meeting_id = TranscriptsRepository::save_transcript(
            &pool,
            "Standup",
            &[
                text_segment("first half a", 0.0, 10.0),
                text_segment("first half b", 10.0, 25.0),
            ],
            None,
        )
        .await
        .expect("initial save");

        // The resumed recording counts from zero all over again
        TranscriptsRepository::append_transcript(
            &pool,
            &meeting_id,
            &[
                text_segment("second half a", 0.0, 8.0),
                text_segment("second half b", 8.0, 20.0),
            ],
        )
        .await
        .expect("append");

        let timeline = stored_timeline(&pool, &meeting_id).await;
        let texts: Vec<&str> = timeline.iter().map(|(t, _, _)| t.as_str()).collect();

        // Without the offset, "second half a" (0.0) would sort to the very front
        assert_eq!(
            texts,
            vec!["first half a", "first half b", "second half a", "second half b"]
        );

        // Appended segments are shifted past the furthest one already stored (25.0)
        assert_eq!(timeline[2].1, 25.0);
        assert_eq!(timeline[2].2, 33.0);
        assert_eq!(timeline[3].1, 33.0);
        assert_eq!(timeline[3].2, 45.0);
    }

    #[tokio::test]
    async fn appending_twice_keeps_stacking() {
        let pool = test_pool().await;

        let meeting_id =
            TranscriptsRepository::save_transcript(&pool, "Standup", &[text_segment("a", 0.0, 10.0)], None)
                .await
                .expect("initial save");

        for text in ["b", "c"] {
            TranscriptsRepository::append_transcript(
                &pool,
                &meeting_id,
                &[text_segment(text, 0.0, 10.0)],
            )
            .await
            .expect("append");
        }

        let timeline = stored_timeline(&pool, &meeting_id).await;
        assert_eq!(
            timeline,
            vec![
                ("a".to_string(), 0.0, 10.0),
                ("b".to_string(), 10.0, 20.0),
                ("c".to_string(), 20.0, 30.0),
            ]
        );
    }

    #[tokio::test]
    async fn append_refuses_a_meeting_that_does_not_exist() {
        let pool = test_pool().await;

        let result = TranscriptsRepository::append_transcript(
            &pool,
            "meeting-does-not-exist",
            &[text_segment("orphan", 0.0, 5.0)],
        )
        .await;

        assert!(matches!(result, Err(SqlxError::RowNotFound)));

        // Nothing was written on the way to failing
        let orphans: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transcripts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(orphans.0, 0);
    }

    #[tokio::test]
    async fn append_rejects_a_blank_meeting_id() {
        let pool = test_pool().await;
        assert!(TranscriptsRepository::append_transcript(&pool, "   ", &[])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn append_touches_the_meeting_so_it_resurfaces() {
        let pool = test_pool().await;

        let meeting_id =
            TranscriptsRepository::save_transcript(&pool, "Standup", &[text_segment("a", 0.0, 10.0)], None)
                .await
                .expect("initial save");

        let (before,): (DateTime<Utc>,) =
            sqlx::query_as("SELECT updated_at FROM meetings WHERE id = ?")
                .bind(&meeting_id)
                .fetch_one(&pool)
                .await
                .unwrap();

        TranscriptsRepository::append_transcript(&pool, &meeting_id, &[text_segment("b", 0.0, 5.0)])
            .await
            .expect("append");

        let (after,): (DateTime<Utc>,) =
            sqlx::query_as("SELECT updated_at FROM meetings WHERE id = ?")
                .bind(&meeting_id)
                .fetch_one(&pool)
                .await
                .unwrap();

        assert!(after >= before, "updated_at should move forward on append");
    }

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
    fn append_offset_continues_from_the_last_stored_segment() {
        assert_eq!(append_offset(Some(1845.5)), 1845.5);
    }

    #[test]
    fn append_offset_falls_back_to_zero_for_unusable_values() {
        // An empty meeting reports NULL, and bad audio times must not shift anything
        assert_eq!(append_offset(None), 0.0);
        assert_eq!(append_offset(Some(0.0)), 0.0);
        assert_eq!(append_offset(Some(-12.0)), 0.0);
        assert_eq!(append_offset(Some(f64::NAN)), 0.0);
        assert_eq!(append_offset(Some(f64::INFINITY)), 0.0);
    }

    #[test]
    fn meeting_start_time_ignores_invalid_values() {
        let now = Utc::now();
        let transcripts = vec![segment(Some(f64::NAN), Some(-5.0)), segment(None, Some(30.0))];
        let start = meeting_start_time(now, &transcripts);
        assert_eq!((now - start).num_milliseconds(), 30_000);
    }
}
