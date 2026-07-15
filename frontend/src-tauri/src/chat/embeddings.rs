/// Semantic search layer for chat, powered by Ollama's embeddings endpoint.
///
/// Transcripts are chunked and embedded lazily into the `chat_embeddings`
/// table the first time chat runs while Ollama (with the embedding model
/// pulled) is reachable. When Ollama is not available this module degrades to
/// a no-op and chat falls back to keyword retrieval alone.
use log::{info, warn};
use reqwest::Client;
use serde::Deserialize;
use sqlx::SqlitePool;
use std::time::Duration;

/// Well-known compact embedding model; enable with `ollama pull nomic-embed-text`.
pub(crate) const EMBEDDING_MODEL: &str = "nomic-embed-text";

const CHUNK_CHARS: usize = 900;
const CHUNK_OVERLAP_CHARS: usize = 150;
/// Bound first-question latency: index at most this many meetings per ask.
const MAX_MEETINGS_INDEXED_PER_ASK: i64 = 3;
const TOP_CHUNKS: usize = 12;
/// Cosine floor below which a chunk is considered unrelated to the question.
const MIN_SCORE: f32 = 0.35;

pub(crate) struct SemanticHit {
    pub meeting_id: String,
    pub title: String,
    pub created_at: String,
    pub chunks: Vec<String>,
    pub score: f32,
}

#[derive(Deserialize)]
struct EmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

fn default_endpoint(endpoint: Option<&str>) -> String {
    endpoint
        .map(|e| e.trim_end_matches('/').to_string())
        .unwrap_or_else(|| "http://localhost:11434".to_string())
}

async fn ollama_reachable(client: &Client, endpoint: &str) -> bool {
    let url = format!("{}/api/version", endpoint);
    matches!(
        client
            .get(&url)
            .timeout(Duration::from_millis(1500))
            .send()
            .await,
        Ok(resp) if resp.status().is_success()
    )
}

async fn embed_texts(
    client: &Client,
    endpoint: &str,
    texts: &[String],
) -> Result<Vec<Vec<f32>>, String> {
    let url = format!("{}/api/embed", endpoint);
    let response = client
        .post(&url)
        .json(&serde_json::json!({ "model": EMBEDDING_MODEL, "input": texts }))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| format!("Embedding request failed: {}", e))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "Embedding request returned {}: {}",
            status,
            body.chars().take(200).collect::<String>()
        ));
    }

    let parsed: EmbedResponse = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse embedding response: {}", e))?;
    if parsed.embeddings.len() != texts.len() {
        return Err(format!(
            "Embedding count mismatch: sent {}, got {}",
            texts.len(),
            parsed.embeddings.len()
        ));
    }
    Ok(parsed.embeddings)
}

/// Split a transcript into overlapping character chunks on segment boundaries.
pub(crate) fn chunk_transcript(segments: &[String]) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();

    for segment in segments {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        if !current.is_empty() && current.chars().count() + segment.chars().count() > CHUNK_CHARS {
            // Carry a tail of the previous chunk forward for overlap
            let tail: String = current
                .chars()
                .skip(current.chars().count().saturating_sub(CHUNK_OVERLAP_CHARS))
                .collect();
            chunks.push(std::mem::take(&mut current));
            current = tail;
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(segment);
    }
    if !current.trim().is_empty() {
        chunks.push(current);
    }
    chunks
}

pub(crate) fn embedding_to_bytes(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
}

pub(crate) fn bytes_to_embedding(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

pub(crate) fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut norm_a, mut norm_b) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

/// Embed and store any recent meetings that are not in the index yet.
async fn index_missing_meetings(
    pool: &SqlitePool,
    client: &Client,
    endpoint: &str,
) -> Result<(), String> {
    let missing: Vec<(String, String)> = sqlx::query_as(
        "SELECT m.id, m.created_at FROM meetings m
         WHERE NOT EXISTS (
             SELECT 1 FROM chat_embeddings e
             WHERE e.meeting_id = m.id AND e.model = ?
         )
         AND EXISTS (SELECT 1 FROM transcripts t WHERE t.meeting_id = m.id)
         ORDER BY m.created_at DESC
         LIMIT ?",
    )
    .bind(EMBEDDING_MODEL)
    .bind(MAX_MEETINGS_INDEXED_PER_ASK)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("Failed to find unindexed meetings: {}", e))?;

    for (meeting_id, _) in missing {
        let segments: Vec<(String,)> =
            sqlx::query_as("SELECT transcript FROM transcripts WHERE meeting_id = ? ORDER BY rowid")
                .bind(&meeting_id)
                .fetch_all(pool)
                .await
                .map_err(|e| format!("Failed to load transcript for indexing: {}", e))?;
        let segments: Vec<String> = segments.into_iter().map(|(s,)| s).collect();
        let chunks = chunk_transcript(&segments);
        if chunks.is_empty() {
            continue;
        }

        let embeddings = embed_texts(client, endpoint, &chunks).await?;
        let now = chrono::Utc::now().to_rfc3339();

        for (index, (chunk, embedding)) in chunks.iter().zip(embeddings.iter()).enumerate() {
            sqlx::query(
                "INSERT OR REPLACE INTO chat_embeddings
                 (meeting_id, chunk_index, chunk_text, embedding, model, created_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(&meeting_id)
            .bind(index as i64)
            .bind(chunk)
            .bind(embedding_to_bytes(embedding))
            .bind(EMBEDDING_MODEL)
            .bind(&now)
            .execute(pool)
            .await
            .map_err(|e| format!("Failed to store embedding: {}", e))?;
        }
        info!(
            "chat embeddings: indexed meeting {} ({} chunks)",
            meeting_id,
            chunks.len()
        );
    }
    Ok(())
}

/// Semantic retrieval: returns per-meeting best chunks for the question, or an
/// empty list when Ollama/the embedding model is unavailable. Never errors the
/// chat request - all failures degrade to keyword-only retrieval.
pub(crate) async fn semantic_candidates(
    pool: &SqlitePool,
    client: &Client,
    ollama_endpoint: Option<&str>,
    question: &str,
) -> Vec<SemanticHit> {
    let endpoint = default_endpoint(ollama_endpoint);
    if !ollama_reachable(client, &endpoint).await {
        return Vec::new();
    }

    if let Err(e) = index_missing_meetings(pool, client, &endpoint).await {
        // Typically: embedding model not pulled. Log once per ask and fall back.
        warn!("chat embeddings: indexing unavailable ({})", e);
        return Vec::new();
    }

    let question_embedding = match embed_texts(client, &endpoint, &[question.to_string()]).await {
        Ok(mut embeddings) => embeddings.remove(0),
        Err(e) => {
            warn!("chat embeddings: question embedding failed ({})", e);
            return Vec::new();
        }
    };

    let rows: Vec<(String, String, String, String, Vec<u8>)> = match sqlx::query_as(
        "SELECT e.meeting_id, m.title, m.created_at, e.chunk_text, e.embedding
         FROM chat_embeddings e
         JOIN meetings m ON m.id = e.meeting_id
         WHERE e.model = ?",
    )
    .bind(EMBEDDING_MODEL)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            warn!("chat embeddings: failed to load index ({})", e);
            return Vec::new();
        }
    };

    let mut scored: Vec<(f32, String, String, String, String)> = rows
        .into_iter()
        .filter_map(|(meeting_id, title, created_at, chunk_text, blob)| {
            let embedding = bytes_to_embedding(&blob);
            let score = cosine_similarity(&question_embedding, &embedding);
            (score >= MIN_SCORE).then_some((score, meeting_id, title, created_at, chunk_text))
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(TOP_CHUNKS);

    // Group chunks by meeting, keeping the meeting's best score for ranking.
    let mut hits: Vec<SemanticHit> = Vec::new();
    for (score, meeting_id, title, created_at, chunk_text) in scored {
        if let Some(hit) = hits.iter_mut().find(|h| h.meeting_id == meeting_id) {
            hit.chunks.push(chunk_text);
        } else {
            hits.push(SemanticHit {
                meeting_id,
                title,
                created_at,
                chunks: vec![chunk_text],
                score,
            });
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_transcript_splits_and_overlaps() {
        let segment = "word ".repeat(60).trim().to_string(); // ~300 chars
        let segments = vec![segment.clone(), segment.clone(), segment.clone(), segment];
        let chunks = chunk_transcript(&segments);
        assert!(chunks.len() >= 2, "expected multiple chunks");
        // Overlap: the tail of chunk N appears at the head of chunk N+1
        let tail: String = chunks[0]
            .chars()
            .skip(chunks[0].chars().count() - 50)
            .collect();
        assert!(chunks[1].starts_with(tail.chars().take(20).collect::<String>().as_str()));
    }

    #[test]
    fn chunk_transcript_skips_empty_segments() {
        let chunks = chunk_transcript(&["".to_string(), "  ".to_string(), "hello".to_string()]);
        assert_eq!(chunks, vec!["hello".to_string()]);
    }

    #[test]
    fn embedding_bytes_roundtrip() {
        let embedding = vec![0.5f32, -1.25, 3.75, 0.0];
        let bytes = embedding_to_bytes(&embedding);
        assert_eq!(bytes.len(), 16);
        assert_eq!(bytes_to_embedding(&bytes), embedding);
    }

    #[test]
    fn cosine_similarity_basics() {
        let a = vec![1.0f32, 0.0];
        let b = vec![1.0f32, 0.0];
        let c = vec![0.0f32, 1.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);
        assert!(cosine_similarity(&a, &c).abs() < 1e-6);
        // Mismatched dimensions are treated as unrelated, not an error
        assert_eq!(cosine_similarity(&a, &[1.0, 0.0, 0.0]), 0.0);
    }
}
