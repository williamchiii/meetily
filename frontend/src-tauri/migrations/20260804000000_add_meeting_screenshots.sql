-- Screenshots captured during a meeting, with the context an AI vision model read
-- out of them. Feeds the summary so it knows about slides, dashboards and diagrams
-- that were shown but never spoken aloud.
CREATE TABLE IF NOT EXISTS meeting_screenshots (
    id TEXT PRIMARY KEY NOT NULL,
    meeting_id TEXT NOT NULL,
    image_path TEXT,
    extracted_text TEXT NOT NULL,
    note TEXT,
    captured_at TEXT NOT NULL,
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_meeting_screenshots_meeting_id
    ON meeting_screenshots(meeting_id, captured_at);
