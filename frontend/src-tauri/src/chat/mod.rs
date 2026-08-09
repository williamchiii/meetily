/// Chat module - answers user questions about their meetings.
///
/// Retrieval combines three signals, all local: keyword search over
/// transcripts and titles (with neighbor-segment windows), saved AI
/// summaries, and - when an Ollama embedding model is reachable - semantic
/// search over embedded transcript chunks. The prompt budget scales with the
/// context window of whichever model answers: the chat-specific model if one
/// is configured, otherwise the summary model.
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use log::{error, info};
use once_cell::sync::Lazy;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tauri::{AppHandle, Manager, Runtime};

use crate::{
    database::repositories::setting::SettingsRepository,
    ollama::metadata::ModelMetadataCache,
    state::AppState,
    summary::llm_client::{generate_summary, LLMProvider},
};

pub(crate) mod embeddings;

/// One prior turn of the conversation, sent from the frontend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// A meeting whose summary/transcript was used to ground the answer.
#[derive(Debug, Serialize)]
pub struct ChatSource {
    pub id: String,
    pub title: String,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct ChatResponse {
    pub answer: String,
    pub sources: Vec<ChatSource>,
}

#[derive(Debug, Serialize)]
pub struct ChatModelConfig {
    pub provider: Option<String>,
    pub model: Option<String>,
}

// Question words and filler that would match almost every transcript segment.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "that", "this", "these", "those", "with", "what", "when", "where",
    "which", "who", "whom", "how", "why", "did", "does", "was", "were", "are", "have", "has",
    "had", "about", "from", "they", "them", "their", "there", "then", "than", "you", "your",
    "our", "out", "not", "but", "all", "any", "can", "could", "would", "should", "will", "just",
    "into", "over", "under", "after", "before", "last", "next", "week", "today", "yesterday",
    "tomorrow", "meeting", "meetings", "say", "said", "says", "talk", "talked", "discuss",
    "discussed", "tell", "told", "mention", "mentioned", "please", "give", "get", "got", "went",
    "come", "came", "some", "something", "anything", "everything", "recap", "summary",
    "summarize", "happened", "find", "found", "look", "looking", "know", "need", "want", "show",
    "explain", "more", "info", "information", "detail", "details", "latest", "recent",
];

const MAX_KEYWORDS: usize = 8;
const MAX_CONTEXT_MEETINGS: usize = 5;
/// Segments of context pulled in around every keyword-matched segment.
const WINDOW_RADIUS: usize = 2;
/// Most segments a single meeting contributes excerpts from.
const MAX_SEGMENTS_LOADED: i64 = 800;
const MAX_HISTORY_MESSAGES: usize = 6;
const MAX_HISTORY_CHARS_PER_MESSAGE: usize = 1000;
const RECENT_MEETINGS_LISTED: i64 = 12;
/// Rough chars-per-token used to convert model context windows into prompt
/// character budgets; 3 is conservative for English + transcription noise.
const CHARS_PER_TOKEN: usize = 3;
/// Tokens reserved for the system prompt, question, history, scaffolding, and
/// generation headroom before the rest of the window is given to context.
const RESERVED_TOKENS: usize = 2000;
const MIN_CONTEXT_CHARS: usize = 9_000;
const MAX_CONTEXT_CHARS: usize = 80_000;

/// Ollama context sizes are fetched per model and cached briefly.
static CHAT_METADATA_CACHE: Lazy<ModelMetadataCache> =
    Lazy::new(|| ModelMetadataCache::new(Duration::from_secs(300)));

fn extract_keywords(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    text.split(|c: char| !c.is_alphanumeric())
        .map(|w| w.trim().to_lowercase())
        .filter(|w| w.len() >= 3 && !STOPWORDS.contains(&w.as_str()))
        .filter(|w| seen.insert(w.clone()))
        .take(MAX_KEYWORDS)
        .collect()
}

/// Keywords for retrieval: from the current question, topped up from recent
/// user turns so follow-ups like "find it" inherit the conversation's topic.
fn gather_keywords(question: &str, history: &[ChatMessage]) -> Vec<String> {
    let mut keywords = extract_keywords(question);
    if keywords.len() < 2 {
        for message in history.iter().rev().filter(|m| m.role == "user") {
            for keyword in extract_keywords(&message.content) {
                if !keywords.contains(&keyword) {
                    keywords.push(keyword);
                }
            }
            if keywords.len() >= 2 {
                break;
            }
        }
    }
    keywords.truncate(MAX_KEYWORDS);
    keywords
}

fn escape_like_pattern(keyword: &str) -> String {
    keyword
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Convert a model context window (tokens) into a prompt character budget.
fn context_chars_for_tokens(context_tokens: usize) -> usize {
    (context_tokens.saturating_sub(RESERVED_TOKENS) * CHARS_PER_TOKEN)
        .clamp(MIN_CONTEXT_CHARS, MAX_CONTEXT_CHARS)
}

/// Resolve how many characters of meeting context the answering model can take.
async fn resolve_context_budget(
    provider: &LLMProvider,
    model_name: &str,
    ollama_endpoint: Option<&str>,
) -> usize {
    let tokens = match provider {
        LLMProvider::BuiltInAI => {
            crate::summary::summary_engine::models::get_model_by_name(model_name)
                .map(|m| m.context_size as usize)
                .unwrap_or(8192)
        }
        LLMProvider::Ollama => CHAT_METADATA_CACHE
            .get_or_fetch(model_name, ollama_endpoint)
            .await
            .map(|m| m.context_size)
            .unwrap_or(8192),
        LLMProvider::CustomOpenAI => 16_384,
        // Hosted frontier models: cap by cost/latency, not by their windows.
        LLMProvider::Claude
        | LLMProvider::OpenAI
        | LLMProvider::Groq
        | LLMProvider::OpenRouter => 32_768,
    };
    context_chars_for_tokens(tokens)
}

#[derive(Default)]
struct MeetingContext {
    title: String,
    created_at: String,
    keyword_count: usize,
    hit_count: i64,
    semantic_score: f32,
    excerpt: String,
    semantic_chunks: Vec<String>,
    summary: Option<String>,
}

/// Stage 1 of keyword retrieval: rank meetings by how many distinct keywords
/// they match (in transcript text or title) without fetching transcript rows.
async fn rank_meetings_by_keywords(
    pool: &SqlitePool,
    keywords: &[String],
) -> Result<Vec<(String, MeetingContext)>, String> {
    let mut ranked: HashMap<String, MeetingContext> = HashMap::new();

    for keyword in keywords {
        let pattern = format!("%{}%", escape_like_pattern(keyword));
        let rows: Vec<(String, String, String, i64)> = sqlx::query_as(
            "SELECT m.id, m.title, m.created_at, COUNT(t.rowid)
             FROM meetings m
             JOIN transcripts t ON m.id = t.meeting_id
             WHERE LOWER(t.transcript) LIKE ?1 ESCAPE '\\'
                OR LOWER(m.title) LIKE ?1 ESCAPE '\\'
             GROUP BY m.id, m.title, m.created_at
             ORDER BY m.created_at DESC
             LIMIT 50",
        )
        .bind(&pattern)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("Transcript search failed: {}", e))?;

        for (id, title, created_at, hits) in rows {
            let entry = ranked.entry(id).or_insert_with(|| MeetingContext {
                title,
                created_at,
                ..Default::default()
            });
            entry.keyword_count += 1;
            entry.hit_count += hits;
        }
    }

    let mut ranked: Vec<(String, MeetingContext)> = ranked.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.keyword_count
            .cmp(&a.1.keyword_count)
            .then_with(|| b.1.hit_count.cmp(&a.1.hit_count))
            .then_with(|| b.1.created_at.cmp(&a.1.created_at))
    });
    Ok(ranked)
}

/// Stage 2: build a windowed excerpt for one meeting - every segment matching
/// a keyword plus WINDOW_RADIUS segments around it, gaps marked with […].
/// With no keyword matches (title match / recency fallback) the head of the
/// meeting is used instead.
fn select_windows(segments: &[String], keywords: &[String], max_chars: usize) -> String {
    if segments.is_empty() || max_chars == 0 {
        return String::new();
    }
    let lower_keywords: Vec<String> = keywords.iter().map(|k| k.to_lowercase()).collect();
    let mut include = vec![false; segments.len()];
    let mut any_match = false;

    if !lower_keywords.is_empty() {
        for (i, segment) in segments.iter().enumerate() {
            let lower = segment.to_lowercase();
            if lower_keywords.iter().any(|k| lower.contains(k)) {
                any_match = true;
                let start = i.saturating_sub(WINDOW_RADIUS);
                let end = (i + WINDOW_RADIUS).min(segments.len() - 1);
                for flag in include.iter_mut().take(end + 1).skip(start) {
                    *flag = true;
                }
            }
        }
    }
    if !any_match {
        for flag in include.iter_mut() {
            *flag = true;
        }
    }

    let mut out = String::new();
    let mut previous_included = true;
    for (i, segment) in segments.iter().enumerate() {
        if out.chars().count() >= max_chars {
            break;
        }
        if include[i] {
            let segment = segment.trim();
            if segment.is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push('\n');
                if !previous_included {
                    out.push_str("[…]\n");
                }
            }
            out.push_str(segment);
            previous_included = true;
        } else {
            previous_included = false;
        }
    }
    truncate_chars(&out, max_chars)
}

async fn load_segments(pool: &SqlitePool, meeting_id: &str) -> Result<Vec<String>, String> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT transcript FROM transcripts WHERE meeting_id = ? ORDER BY rowid LIMIT ?",
    )
    .bind(meeting_id)
    .bind(MAX_SEGMENTS_LOADED)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("Failed to load transcript: {}", e))?;
    Ok(rows.into_iter().map(|(s,)| s).collect())
}

/// Pull the saved AI summary for a meeting, as plain text, if one exists.
async fn fetch_summary_text(pool: &SqlitePool, meeting_id: &str) -> Option<String> {
    let row = sqlx::query_as::<_, (Option<String>,)>(
        "SELECT result FROM summary_processes WHERE meeting_id = ?",
    )
    .bind(meeting_id)
    .fetch_optional(pool)
    .await
    .ok()??;
    extract_summary_text(&row.0?)
}

/// Extract readable text from a stored summary result. Handles both the
/// current format ({"markdown": ...}) and the legacy sectioned format
/// ({"MeetingNotes": {"sections": [{"title", "blocks": [{"content"}]}]}}).
fn extract_summary_text(result_json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(result_json).ok()?;

    if let Some(markdown) = value.get("markdown").and_then(|v| v.as_str()) {
        if !markdown.trim().is_empty() {
            return Some(markdown.trim().to_string());
        }
    }

    let sections = value.get("MeetingNotes")?.get("sections")?.as_array()?;
    let mut out = String::new();
    for section in sections {
        if let Some(title) = section.get("title").and_then(|v| v.as_str()) {
            if !title.trim().is_empty() {
                out.push_str(&format!("## {}\n", title.trim()));
            }
        }
        if let Some(blocks) = section.get("blocks").and_then(|v| v.as_array()) {
            for block in blocks {
                if let Some(content) = block.get("content").and_then(|v| v.as_str()) {
                    if !content.trim().is_empty() {
                        out.push_str(content.trim());
                        out.push('\n');
                    }
                }
            }
        }
    }
    let out = out.trim();
    if out.is_empty() {
        None
    } else {
        Some(out.to_string())
    }
}

async fn recent_meetings(pool: &SqlitePool) -> Result<Vec<(String, String, String)>, String> {
    sqlx::query_as::<_, (String, String, String)>(
        "SELECT id, title, created_at FROM meetings ORDER BY created_at DESC LIMIT ?",
    )
    .bind(RECENT_MEETINGS_LISTED)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("Failed to list meetings: {}", e))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_chars).collect();
    format!("{}…", truncated)
}

/// Keep only the date part of an RFC 3339 / SQLite timestamp for the prompt.
fn date_only(timestamp: &str) -> &str {
    timestamp.split(['T', ' ']).next().unwrap_or(timestamp)
}

fn build_user_prompt(
    question: &str,
    today: &str,
    history: &[ChatMessage],
    recents: &[(String, String, String)],
    context: &[(String, MeetingContext)],
    total_budget_chars: usize,
) -> String {
    let mut prompt = String::new();

    prompt.push_str(&format!("Today's date: {}\n\n", today));

    prompt.push_str("## Recent meetings (newest first)\n");
    if recents.is_empty() {
        prompt.push_str("(no meetings recorded yet)\n");
    }
    for (_, title, created_at) in recents {
        prompt.push_str(&format!("- {} ({})\n", title, date_only(created_at)));
    }

    prompt.push_str("\n## Meeting details relevant to the question\n");
    if context.is_empty() {
        prompt.push_str("(nothing matched the question)\n");
    } else {
        // Split the budget across meetings; a lone meeting gets everything.
        let per_meeting = total_budget_chars / context.len().max(1);
        for (_, meeting) in context {
            prompt.push_str(&format!(
                "### {} ({})\n",
                meeting.title,
                date_only(&meeting.created_at)
            ));

            let mut remaining = per_meeting;
            if let Some(summary) = &meeting.summary {
                // Summaries are dense: give them up to half the meeting's share.
                let summary_budget = remaining / 2;
                let text = truncate_chars(summary, summary_budget);
                remaining = remaining.saturating_sub(text.chars().count());
                prompt.push_str(&format!("Saved summary:\n{}\n", text));
            }

            let mut transcript_parts: Vec<&str> = Vec::new();
            if !meeting.excerpt.is_empty() {
                transcript_parts.push(meeting.excerpt.as_str());
            }
            for chunk in &meeting.semantic_chunks {
                // Skip semantic chunks already covered by the keyword excerpt
                if !meeting.excerpt.contains(chunk.as_str()) {
                    transcript_parts.push(chunk.as_str());
                }
            }
            if !transcript_parts.is_empty() && remaining > 0 {
                let text = truncate_chars(&transcript_parts.join("\n[…]\n"), remaining);
                prompt.push_str(&format!("Transcript excerpts:\n{}\n", text));
            }
            prompt.push('\n');
        }
    }

    if !history.is_empty() {
        prompt.push_str("\n## Conversation so far\n");
        for message in history.iter().rev().take(MAX_HISTORY_MESSAGES).rev() {
            let speaker = if message.role == "assistant" {
                "Assistant"
            } else {
                "User"
            };
            prompt.push_str(&format!(
                "{}: {}\n",
                speaker,
                truncate_chars(&message.content, MAX_HISTORY_CHARS_PER_MESSAGE)
            ));
        }
    }

    prompt.push_str(&format!("\n## Question\n{}\n", question));
    prompt
}

const SYSTEM_PROMPT: &str = "You are Meetily's meeting assistant. Answer the user's question using \
the meeting list, saved summaries, and transcript excerpts provided. Prefer saved summaries for \
overviews and transcript excerpts for specifics; ground every claim in that context and refer to \
meetings by their title and date. Use today's date to resolve words like 'today' or 'last week'. \
Transcripts are raw speech-to-text, so tolerate transcription errors; [\u{2026}] marks skipped \
passages. If the context does not contain the answer, say so plainly instead of guessing. Be \
concise and answer in Markdown.";

/// The model that answers chat: the chat-specific override when configured,
/// otherwise the summary model.
fn resolve_chat_model(setting: &crate::database::models::Setting) -> (String, String) {
    match (&setting.chat_provider, &setting.chat_model) {
        (Some(provider), Some(model))
            if !provider.trim().is_empty() && !model.trim().is_empty() =>
        {
            (provider.clone(), model.clone())
        }
        _ => (setting.provider.clone(), setting.model.clone()),
    }
}

#[tauri::command]
pub async fn api_get_chat_model_config(
    state: tauri::State<'_, AppState>,
) -> Result<ChatModelConfig, String> {
    let setting = SettingsRepository::get_model_config(state.db_manager.pool())
        .await
        .map_err(|e| format!("Failed to load settings: {}", e))?;
    Ok(ChatModelConfig {
        provider: setting.as_ref().and_then(|s| s.chat_provider.clone()),
        model: setting.as_ref().and_then(|s| s.chat_model.clone()),
    })
}

#[tauri::command]
pub async fn api_save_chat_model_config(
    state: tauri::State<'_, AppState>,
    provider: Option<String>,
    model: Option<String>,
) -> Result<(), String> {
    SettingsRepository::save_chat_model_config(
        state.db_manager.pool(),
        provider.as_deref().filter(|p| !p.trim().is_empty()),
        model.as_deref().filter(|m| !m.trim().is_empty()),
    )
    .await
    .map_err(|e| format!("Failed to save chat model config: {}", e))
}

#[tauri::command]
pub async fn chat_ask<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    question: String,
    history: Option<Vec<ChatMessage>>,
) -> Result<ChatResponse, String> {
    let question = question.trim().to_string();
    if question.is_empty() {
        return Err("Question cannot be empty".to_string());
    }
    let history = history.unwrap_or_default();
    let pool = state.db_manager.pool();

    // Resolve the answering model first - the retrieval budget depends on it.
    let setting = SettingsRepository::get_model_config(pool)
        .await
        .map_err(|e| format!("Failed to load model settings: {}", e))?
        .ok_or_else(|| {
            "No summarization model configured. Pick one in Settings first.".to_string()
        })?;
    let (provider_name, model_name) = resolve_chat_model(&setting);
    let provider = LLMProvider::from_str(&provider_name)?;
    if model_name.trim().is_empty() {
        return Err("No model selected. Pick one in Settings first.".to_string());
    }

    let client = Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    let budget_chars = resolve_context_budget(
        &provider,
        &model_name,
        setting.ollama_endpoint.as_deref(),
    )
    .await;

    // 1. Retrieve grounding context: keywords + semantic search in parallel.
    let keywords = gather_keywords(&question, &history);
    info!(
        "chat_ask model={}:{} budget={} chars keywords={:?}",
        provider_name, model_name, budget_chars, keywords
    );

    let (keyword_ranked, semantic_hits) = tokio::join!(
        async {
            if keywords.is_empty() {
                Ok(Vec::new())
            } else {
                rank_meetings_by_keywords(pool, &keywords).await
            }
        },
        embeddings::semantic_candidates(
            pool,
            &client,
            setting.ollama_endpoint.as_deref(),
            &question
        )
    );
    let keyword_ranked = keyword_ranked?;
    if !semantic_hits.is_empty() {
        info!(
            "chat_ask semantic hits: {:?}",
            semantic_hits
                .iter()
                .map(|h| (&h.title, h.score))
                .collect::<Vec<_>>()
        );
    }

    // Merge: meetings found by both signals rank first, then semantic-only
    // (by score), then keyword-only (already ranked) - all three tiers are
    // sorted together so a strong semantic-only match can't be pushed out by
    // truncation before a weak keyword-only one.
    let mut context: Vec<(String, MeetingContext)> = Vec::new();
    let keyword_ids: HashSet<String> = keyword_ranked.iter().map(|(id, _)| id.clone()).collect();
    for (id, mut meeting) in keyword_ranked {
        if let Some(hit) = semantic_hits.iter().find(|h| h.meeting_id == id) {
            meeting.semantic_score = hit.score;
            meeting.semantic_chunks = hit.chunks.clone();
        }
        context.push((id, meeting));
    }
    for hit in semantic_hits {
        if !keyword_ids.contains(&hit.meeting_id) {
            context.push((
                hit.meeting_id.clone(),
                MeetingContext {
                    title: hit.title,
                    created_at: hit.created_at,
                    semantic_score: hit.score,
                    semantic_chunks: hit.chunks,
                    ..Default::default()
                },
            ));
        }
    }
    context.sort_by(|a, b| {
        fn tier(m: &MeetingContext) -> u8 {
            match (m.semantic_score > 0.0, m.keyword_count > 0) {
                (true, true) => 0,
                (true, false) => 1,
                (false, true) => 2,
                (false, false) => 3,
            }
        }
        tier(&a.1)
            .cmp(&tier(&b.1))
            .then_with(|| b.1.keyword_count.cmp(&a.1.keyword_count))
            .then_with(|| {
                b.1.semantic_score
                    .partial_cmp(&a.1.semantic_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| b.1.hit_count.cmp(&a.1.hit_count))
    });
    context.truncate(MAX_CONTEXT_MEETINGS);

    let recents = recent_meetings(pool).await?;

    // Nothing matched (or the question was all stopwords, e.g. "what was my
    // meeting today about?") - ground in the most recent meeting instead.
    if context.is_empty() {
        if let Some((id, title, created_at)) = recents.first() {
            context.push((
                id.clone(),
                MeetingContext {
                    title: title.clone(),
                    created_at: created_at.clone(),
                    ..Default::default()
                },
            ));
        }
    }

    // 2. Build excerpts and attach summaries within the model's budget.
    let per_meeting_chars = budget_chars / context.len().max(1);
    for (id, meeting) in context.iter_mut() {
        let segments = load_segments(pool, id).await?;
        meeting.excerpt = select_windows(&segments, &keywords, per_meeting_chars);
        meeting.summary = fetch_summary_text(pool, id).await;
    }

    // 3. Resolve credentials/endpoints and ask the model.
    let api_key = if provider == LLMProvider::Ollama
        || provider == LLMProvider::BuiltInAI
        || provider == LLMProvider::CustomOpenAI
    {
        String::new()
    } else {
        match SettingsRepository::get_api_key(pool, &provider_name).await {
            Ok(Some(key)) if !key.is_empty() => key,
            Ok(_) => return Err(format!("API key not found for {}", provider_name)),
            Err(e) => {
                return Err(format!(
                    "Failed to retrieve API key for {}: {}",
                    provider_name, e
                ))
            }
        }
    };

    let ollama_endpoint = if provider == LLMProvider::Ollama {
        setting.ollama_endpoint.clone()
    } else {
        None
    };

    let (custom_endpoint, custom_api_key, custom_max_tokens, custom_temperature, custom_top_p) =
        if provider == LLMProvider::CustomOpenAI {
            match SettingsRepository::get_custom_openai_config(pool).await {
                Ok(Some(config)) => (
                    Some(config.endpoint),
                    config.api_key,
                    config.max_tokens.map(|t| t as u32),
                    config.temperature,
                    config.top_p,
                ),
                Ok(None) => {
                    return Err(
                        "Custom OpenAI provider selected but no configuration found".to_string()
                    )
                }
                Err(e) => return Err(format!("Failed to retrieve custom OpenAI config: {}", e)),
            }
        } else {
            (None, None, None, None, None)
        };
    let final_api_key = if provider == LLMProvider::CustomOpenAI {
        custom_api_key.unwrap_or_default()
    } else {
        api_key
    };

    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {}", e))?;

    let today = chrono::Local::now().format("%Y-%m-%d (%A)").to_string();
    let user_prompt = build_user_prompt(
        &question,
        &today,
        &history,
        &recents,
        &context,
        budget_chars,
    );

    let answer = generate_summary(
        &client,
        &provider,
        &model_name,
        &final_api_key,
        SYSTEM_PROMPT,
        &user_prompt,
        ollama_endpoint.as_deref(),
        custom_endpoint.as_deref(),
        custom_max_tokens,
        custom_temperature,
        custom_top_p,
        Some(&app_data_dir),
        None,
    )
    .await
    .map_err(|e| {
        error!("chat_ask LLM request failed: {}", e);
        e
    })?;

    let sources = context
        .into_iter()
        .map(|(id, meeting)| ChatSource {
            id,
            title: meeting.title,
            created_at: meeting.created_at,
        })
        .collect();

    Ok(ChatResponse { answer, sources })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_message(content: &str) -> ChatMessage {
        ChatMessage {
            role: "user".to_string(),
            content: content.to_string(),
        }
    }

    #[test]
    fn extract_keywords_filters_stopwords_and_short_words() {
        let keywords = extract_keywords("What did we say about the roadmap and Q3 budget?");
        assert_eq!(keywords, vec!["roadmap", "budget"]);
    }

    #[test]
    fn extract_keywords_stops_all_filler_questions() {
        assert!(extract_keywords("What was my meeting today about?").is_empty());
        assert!(extract_keywords("find it").is_empty());
    }

    #[test]
    fn gather_keywords_falls_back_to_prior_user_turns() {
        let history = vec![
            user_message("What did we decide about the Jiro cadence?"),
            ChatMessage {
                role: "assistant".to_string(),
                content: "You discussed the cadence.".to_string(),
            },
        ];
        let keywords = gather_keywords("find it", &history);
        assert!(keywords.contains(&"jiro".to_string()));
        assert!(keywords.contains(&"cadence".to_string()));
    }

    #[test]
    fn escape_like_pattern_escapes_wildcards() {
        assert_eq!(escape_like_pattern("50%_a\\b"), "50\\%\\_a\\\\b");
    }

    #[test]
    fn context_chars_scale_with_model_window() {
        // 8K model (qwen3.5:9b): (8192 - 2000) * 3 = 18576
        assert_eq!(context_chars_for_tokens(8192), 18_576);
        // 32K model (qwen3.5:4b): (32768 - 2000) * 3 = 92304 -> clamped
        assert_eq!(context_chars_for_tokens(32_768), MAX_CONTEXT_CHARS);
        // Tiny/unknown windows never starve retrieval below the floor
        assert_eq!(context_chars_for_tokens(2048), MIN_CONTEXT_CHARS);
    }

    #[test]
    fn select_windows_includes_neighbors_and_marks_gaps() {
        // Two matches far apart -> two windows with an interior gap between them
        let segments: Vec<String> = (0..15)
            .map(|i| {
                if i == 2 || i == 12 {
                    format!("budget point {}", i)
                } else {
                    format!("segment number {}", i)
                }
            })
            .collect();
        let windows = select_windows(&segments, &["budget".to_string()], 10_000);

        // Each match brings WINDOW_RADIUS neighbors on both sides
        assert!(windows.contains("segment number 0"));
        assert!(windows.contains("budget point 2"));
        assert!(windows.contains("segment number 4"));
        assert!(windows.contains("segment number 10"));
        assert!(windows.contains("budget point 12"));
        assert!(windows.contains("segment number 14"));
        // The stretch between the two windows is skipped, with a gap marker
        assert!(!windows.contains("segment number 7"));
        assert!(windows.contains("[…]"));
    }

    #[test]
    fn select_windows_uses_head_when_no_keyword_matches() {
        let segments: Vec<String> = (0..5).map(|i| format!("segment {}", i)).collect();
        let windows = select_windows(&segments, &["nomatch".to_string()], 10_000);
        assert!(windows.contains("segment 0"));
        assert!(windows.contains("segment 4"));
        assert!(!windows.contains("[…]"));
    }

    #[test]
    fn select_windows_respects_char_budget() {
        let segments: Vec<String> = (0..100).map(|i| format!("segment {}", i)).collect();
        let windows = select_windows(&segments, &[], 120);
        assert!(windows.chars().count() <= 121); // budget + ellipsis
    }

    #[test]
    fn extract_summary_text_reads_markdown_format() {
        let json = r##"{"markdown": "# Standup\n- shipped the roadmap", "summary_json": []}"##;
        assert_eq!(
            extract_summary_text(json).unwrap(),
            "# Standup\n- shipped the roadmap"
        );
    }

    #[test]
    fn extract_summary_text_reads_legacy_sections() {
        let json = r#"{"MeetingName": "Standup", "MeetingNotes": {"sections": [
            {"title": "Decisions", "blocks": [{"type": "text", "content": "Ship Friday"}]},
            {"title": "", "blocks": [{"type": "text", "content": ""}]}
        ]}}"#;
        let text = extract_summary_text(json).unwrap();
        assert!(text.contains("## Decisions"));
        assert!(text.contains("Ship Friday"));
    }

    #[test]
    fn extract_summary_text_rejects_garbage() {
        assert!(extract_summary_text("not json").is_none());
        assert!(extract_summary_text(r#"{"markdown": "  "}"#).is_none());
    }

    #[test]
    fn build_user_prompt_includes_sections() {
        let recents = vec![(
            "id1".to_string(),
            "Standup".to_string(),
            "2026-07-01T10:00:00+00:00".to_string(),
        )];
        let context = vec![(
            "id1".to_string(),
            MeetingContext {
                title: "Standup".to_string(),
                created_at: "2026-07-01T10:00:00+00:00".to_string(),
                excerpt: "we shipped the roadmap".to_string(),
                summary: Some("- roadmap shipped".to_string()),
                ..Default::default()
            },
        )];
        let history = vec![user_message("earlier question")];

        let prompt = build_user_prompt(
            "what about the roadmap?",
            "2026-07-10 (Friday)",
            &history,
            &recents,
            &context,
            18_000,
        );

        assert!(prompt.contains("Today's date: 2026-07-10 (Friday)"));
        assert!(prompt.contains("- Standup (2026-07-01)"));
        assert!(prompt.contains("### Standup (2026-07-01)"));
        assert!(prompt.contains("Saved summary:\n- roadmap shipped"));
        assert!(prompt.contains("Transcript excerpts:\nwe shipped the roadmap"));
        assert!(prompt.contains("User: earlier question"));
        assert!(prompt.contains("## Question\nwhat about the roadmap?"));
    }

    #[test]
    fn resolve_chat_model_prefers_override_and_falls_back() {
        let mut setting = crate::database::models::Setting {
            id: "1".to_string(),
            provider: "builtin-ai".to_string(),
            model: "qwen3.5:4b".to_string(),
            whisper_model: "large-v3".to_string(),
            groq_api_key: None,
            openai_api_key: None,
            anthropic_api_key: None,
            ollama_api_key: None,
            open_router_api_key: None,
            ollama_endpoint: None,
            custom_openai_config: None,
            chat_provider: None,
            chat_model: None,
        };
        assert_eq!(
            resolve_chat_model(&setting),
            ("builtin-ai".to_string(), "qwen3.5:4b".to_string())
        );

        setting.chat_provider = Some("claude".to_string());
        setting.chat_model = Some("claude-sonnet-5".to_string());
        assert_eq!(
            resolve_chat_model(&setting),
            ("claude".to_string(), "claude-sonnet-5".to_string())
        );

        // Half-configured override is ignored
        setting.chat_model = Some("  ".to_string());
        assert_eq!(
            resolve_chat_model(&setting),
            ("builtin-ai".to_string(), "qwen3.5:4b".to_string())
        );
    }
}
