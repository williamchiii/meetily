-- Separate model selection for the Chat tab.
-- NULL means "use the summary model" (settings.provider / settings.model).
ALTER TABLE settings ADD COLUMN chatProvider TEXT;
ALTER TABLE settings ADD COLUMN chatModel TEXT;

-- Locally computed transcript-chunk embeddings for semantic chat search.
-- Built lazily when an Ollama embedding model is reachable; keyword search
-- remains the fallback when this table is empty.
CREATE TABLE IF NOT EXISTS chat_embeddings (
    meeting_id TEXT NOT NULL,
    chunk_index INTEGER NOT NULL,
    chunk_text TEXT NOT NULL,
    embedding BLOB NOT NULL,
    model TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (meeting_id, chunk_index),
    FOREIGN KEY (meeting_id) REFERENCES meetings(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_chat_embeddings_model ON chat_embeddings(model);
