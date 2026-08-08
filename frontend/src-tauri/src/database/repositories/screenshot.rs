use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{Connection, Error as SqlxError, FromRow, SqlitePool};
use tracing::info;
use uuid::Uuid;

/// A screenshot shared during a meeting, plus what a vision model read out of it.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct MeetingScreenshot {
    pub id: String,
    pub meeting_id: String,
    pub image_path: Option<String>,
    pub extracted_text: String,
    /// Optional user note giving the screenshot a label in the summary.
    pub note: Option<String>,
    pub captured_at: String,
}

/// What the frontend hands over once a meeting exists to attach screenshots to.
#[derive(Debug, Clone, Deserialize)]
pub struct NewScreenshot {
    pub image_path: Option<String>,
    pub extracted_text: String,
    pub note: Option<String>,
    pub captured_at: Option<String>,
}

pub struct ScreenshotsRepository;

impl ScreenshotsRepository {
    /// Attach screenshots captured during a recording to the meeting it produced.
    ///
    /// Screenshots are taken while recording, when no meeting row exists yet, so they
    /// are held by the frontend and land here once the meeting has been saved.
    pub async fn attach_to_meeting(
        pool: &SqlitePool,
        meeting_id: &str,
        screenshots: &[NewScreenshot],
    ) -> Result<usize, SqlxError> {
        if meeting_id.trim().is_empty() {
            return Err(SqlxError::Protocol("meeting_id cannot be empty".to_string()));
        }

        if screenshots.is_empty() {
            return Ok(0);
        }

        let exists: Option<(String,)> = sqlx::query_as("SELECT id FROM meetings WHERE id = ?")
            .bind(meeting_id)
            .fetch_optional(pool)
            .await?;

        if exists.is_none() {
            return Err(SqlxError::RowNotFound);
        }

        let mut conn = pool.acquire().await?;
        let mut transaction = conn.begin().await?;

        for shot in screenshots {
            let captured_at = shot
                .captured_at
                .clone()
                .unwrap_or_else(|| Utc::now().to_rfc3339());

            let result = sqlx::query(
                "INSERT INTO meeting_screenshots
                 (id, meeting_id, image_path, extracted_text, note, captured_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(format!("screenshot-{}", Uuid::new_v4()))
            .bind(meeting_id)
            .bind(&shot.image_path)
            .bind(&shot.extracted_text)
            .bind(&shot.note)
            .bind(&captured_at)
            .execute(&mut *transaction)
            .await;

            if let Err(e) = result {
                transaction.rollback().await?;
                return Err(e);
            }
        }

        transaction.commit().await?;

        info!(
            "Attached {} screenshot(s) to meeting {}",
            screenshots.len(),
            meeting_id
        );

        Ok(screenshots.len())
    }

    /// Screenshots for a meeting, oldest first so they read in the order shown.
    pub async fn for_meeting(
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<Vec<MeetingScreenshot>, SqlxError> {
        sqlx::query_as::<_, MeetingScreenshot>(
            "SELECT id, meeting_id, image_path, extracted_text, note, captured_at
             FROM meeting_screenshots WHERE meeting_id = ? ORDER BY captured_at ASC",
        )
        .bind(meeting_id)
        .fetch_all(pool)
        .await
    }

    pub async fn delete(pool: &SqlitePool, screenshot_id: &str) -> Result<bool, SqlxError> {
        let result = sqlx::query("DELETE FROM meeting_screenshots WHERE id = ?")
            .bind(screenshot_id)
            .execute(pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Render a meeting's screenshots as a prompt block for the summary.
    ///
    /// Returns None when there is nothing to add, so callers can leave the prompt
    /// untouched rather than injecting an empty section.
    pub async fn context_block(
        pool: &SqlitePool,
        meeting_id: &str,
    ) -> Result<Option<String>, SqlxError> {
        let screenshots = Self::for_meeting(pool, meeting_id).await?;
        Ok(build_context_block(&screenshots))
    }
}

/// Format screenshots into the block that gets appended to the summary prompt.
pub fn build_context_block(screenshots: &[MeetingScreenshot]) -> Option<String> {
    let usable: Vec<&MeetingScreenshot> = screenshots
        .iter()
        .filter(|s| !s.extracted_text.trim().is_empty())
        .collect();

    if usable.is_empty() {
        return None;
    }

    let mut block = String::from(
        "The following was shown on screen during the meeting and read by an AI vision \
         model. Treat it as factual meeting content that may never have been spoken aloud.\n\n\
         <shared_screens>\n",
    );

    for (index, shot) in usable.iter().enumerate() {
        block.push_str(&format!("<screen index=\"{}\"", index + 1));
        if let Some(note) = shot.note.as_ref().filter(|n| !n.trim().is_empty()) {
            // Escape quotes so a user note cannot break out of the attribute
            block.push_str(&format!(" note=\"{}\"", note.trim().replace('"', "'")));
        }
        block.push_str(">\n");
        block.push_str(shot.extracted_text.trim());
        block.push_str("\n</screen>\n");
    }

    block.push_str("</shared_screens>");
    Some(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shot(text: &str, note: Option<&str>) -> MeetingScreenshot {
        MeetingScreenshot {
            id: "s1".to_string(),
            meeting_id: "m1".to_string(),
            image_path: None,
            extracted_text: text.to_string(),
            note: note.map(str::to_string),
            captured_at: "2026-08-04T00:00:00Z".to_string(),
        }
    }

    /// A real database on the real schema, so attach is exercised against the same
    /// constraints production runs under.
    async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.expect("in-memory sqlite");
        sqlx::migrate!("./migrations").run(&pool).await.expect("migrations");
        pool
    }

    async fn seed_meeting(pool: &SqlitePool, id: &str) {
        sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at) VALUES (?, ?, ?, ?)",
        )
        .bind(id)
        .bind("Test meeting")
        .bind(Utc::now())
        .bind(Utc::now())
        .execute(pool)
        .await
        .expect("seed meeting");
    }

    fn new_shot(text: &str, note: Option<&str>, at: &str) -> NewScreenshot {
        NewScreenshot {
            image_path: Some(format!("/tmp/{}.png", text.len())),
            extracted_text: text.to_string(),
            note: note.map(str::to_string),
            captured_at: Some(at.to_string()),
        }
    }

    #[tokio::test]
    async fn attach_then_read_round_trips_in_capture_order() {
        let pool = test_pool().await;
        seed_meeting(&pool, "m-1").await;

        // Deliberately inserted out of order to prove ordering is by captured_at
        let count = ScreenshotsRepository::attach_to_meeting(
            &pool,
            "m-1",
            &[
                new_shot("second slide", None, "2026-08-04T10:05:00Z"),
                new_shot("first slide", Some("intro"), "2026-08-04T10:00:00Z"),
            ],
        )
        .await
        .expect("attach");
        assert_eq!(count, 2);

        let stored = ScreenshotsRepository::for_meeting(&pool, "m-1").await.expect("read");
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].extracted_text, "first slide");
        assert_eq!(stored[0].note.as_deref(), Some("intro"));
        assert_eq!(stored[1].extracted_text, "second slide");

        // And the prompt block reflects that same order
        let block = ScreenshotsRepository::context_block(&pool, "m-1")
            .await
            .expect("block")
            .expect("some block");
        let first = block.find("first slide").unwrap();
        let second = block.find("second slide").unwrap();
        assert!(first < second, "prompt block is out of order:\n{}", block);
    }

    #[tokio::test]
    async fn a_meeting_with_no_screenshots_has_no_context_block() {
        let pool = test_pool().await;
        seed_meeting(&pool, "m-2").await;
        assert_eq!(ScreenshotsRepository::context_block(&pool, "m-2").await.unwrap(), None);
    }

    #[tokio::test]
    async fn attaching_to_a_missing_meeting_is_refused_without_writing() {
        let pool = test_pool().await;

        let result = ScreenshotsRepository::attach_to_meeting(
            &pool,
            "m-does-not-exist",
            &[new_shot("orphan", None, "2026-08-04T10:00:00Z")],
        )
        .await;

        assert!(matches!(result, Err(SqlxError::RowNotFound)));

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM meeting_screenshots")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "nothing should be written on the way to failing");
    }

    #[tokio::test]
    async fn attaching_nothing_is_a_no_op() {
        let pool = test_pool().await;
        seed_meeting(&pool, "m-3").await;
        assert_eq!(
            ScreenshotsRepository::attach_to_meeting(&pool, "m-3", &[]).await.unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn deleting_a_meeting_takes_its_screenshots_with_it() {
        let pool = test_pool().await;
        sqlx::query("PRAGMA foreign_keys = ON").execute(&pool).await.unwrap();
        seed_meeting(&pool, "m-4").await;

        ScreenshotsRepository::attach_to_meeting(
            &pool,
            "m-4",
            &[new_shot("slide", None, "2026-08-04T10:00:00Z")],
        )
        .await
        .unwrap();

        sqlx::query("DELETE FROM meetings WHERE id = ?").bind("m-4").execute(&pool).await.unwrap();

        let remaining = ScreenshotsRepository::for_meeting(&pool, "m-4").await.unwrap();
        assert!(remaining.is_empty(), "screenshots should cascade away with the meeting");
    }

    #[tokio::test]
    async fn a_screenshot_can_be_deleted_individually() {
        let pool = test_pool().await;
        seed_meeting(&pool, "m-5").await;
        ScreenshotsRepository::attach_to_meeting(
            &pool,
            "m-5",
            &[new_shot("slide", None, "2026-08-04T10:00:00Z")],
        )
        .await
        .unwrap();

        let stored = ScreenshotsRepository::for_meeting(&pool, "m-5").await.unwrap();
        assert!(ScreenshotsRepository::delete(&pool, &stored[0].id).await.unwrap());
        assert!(ScreenshotsRepository::for_meeting(&pool, "m-5").await.unwrap().is_empty());
        // Deleting again reports that nothing matched
        assert!(!ScreenshotsRepository::delete(&pool, &stored[0].id).await.unwrap());
    }

    #[test]
    fn nothing_to_add_produces_no_block() {
        assert!(build_context_block(&[]).is_none());
        // Whitespace-only extractions must not inject an empty section
        assert!(build_context_block(&[shot("   ", None)]).is_none());
    }

    #[test]
    fn screenshots_are_numbered_in_order() {
        let block = build_context_block(&[shot("Q3 revenue: 4.2M", None), shot("Roadmap slide", None)])
            .unwrap();
        assert!(block.contains("<screen index=\"1\">"));
        assert!(block.contains("Q3 revenue: 4.2M"));
        assert!(block.contains("<screen index=\"2\">"));
        assert!(block.contains("</shared_screens>"));
    }

    #[test]
    fn a_user_note_labels_the_screen() {
        let block = build_context_block(&[shot("bars and axes", Some("Q3 chart"))]).unwrap();
        assert!(block.contains("note=\"Q3 chart\""), "got {}", block);
    }

    #[test]
    fn a_note_cannot_break_out_of_its_attribute() {
        let block = build_context_block(&[shot("text", Some("say \"hi\" now"))]).unwrap();
        assert!(!block.contains("note=\"say \"hi\""), "unescaped quote in {}", block);
        assert!(block.contains("note=\"say 'hi' now\""), "got {}", block);
    }

    #[test]
    fn blank_extractions_are_dropped_but_others_survive() {
        let block = build_context_block(&[shot("", None), shot("real content", None)]).unwrap();
        assert!(block.contains("real content"));
        assert!(block.contains("index=\"1\""));
        assert!(!block.contains("index=\"2\""), "blank shot should not be numbered");
    }
}
