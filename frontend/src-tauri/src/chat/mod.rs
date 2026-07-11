/// Chat module - answers user questions about their meetings.
///
/// Retrieves relevant meeting summaries and transcript excerpts via keyword
/// search over the local database, then asks the configured summarization LLM
/// (built-in sidecar, Ollama, or a cloud provider) to answer grounded in that
/// context.
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use log::{error, info};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tauri::{AppHandle, Manager, Runtime};

use crate::{
    database::repositories::setting::SettingsRepository,
    state::AppState,
    summary::llm_client::{generate_summary, LLMProvider},
};

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

/// Per-meeting retrieval limits keep the prompt inside every model's context.
const MAX_KEYWORDS: usize = 8;
const MAX_SEGMENTS_PER_MEETING: usize = 6;
const MAX_CONTEXT_MEETINGS: usize = 4;
const MAX_SUMMARY_CHARS_PER_MEETING: usize = 1500;
const MAX_CHARS_PER_MEETING: usize = 2000;
const MAX_TOTAL_CONTEXT_CHARS: usize = 9000;
const MAX_HISTORY_MESSAGES: usize = 6;
const MAX_HISTORY_CHARS_PER_MESSAGE: usize = 1000;
const RECENT_MEETINGS_LISTED: i64 = 12;
const FALLBACK_TRANSCRIPT_SEGMENTS: i64 = 40;

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

struct MeetingHits {
    title: String,
    created_at: String,
    keywords: HashSet<String>,
    segments: Vec<String>,
    summary: Option<String>,
}

/// Search transcripts AND meeting titles for each keyword and group the
/// matching segments by meeting, ranked by distinct keywords matched.
async fn retrieve_context(
    pool: &SqlitePool,
    keywords: &[String],
) -> Result<Vec<(String, MeetingHits)>, String> {
    let mut hits: HashMap<String, MeetingHits> = HashMap::new();

    for keyword in keywords {
        let pattern = format!("%{}%", escape_like_pattern(keyword));
        let rows = sqlx::query_as::<_, (String, String, String, String)>(
            "SELECT m.id, m.title, m.created_at, t.transcript
             FROM meetings m
             JOIN transcripts t ON m.id = t.meeting_id
             WHERE LOWER(t.transcript) LIKE ?1 ESCAPE '\\'
                OR LOWER(m.title) LIKE ?1 ESCAPE '\\'
             ORDER BY m.created_at DESC
             LIMIT 100",
        )
        .bind(&pattern)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("Transcript search failed: {}", e))?;

        for (id, title, created_at, transcript) in rows {
            let entry = hits.entry(id).or_insert_with(|| MeetingHits {
                title,
                created_at,
                keywords: HashSet::new(),
                segments: Vec::new(),
                summary: None,
            });
            entry.keywords.insert(keyword.clone());
            let segment = transcript.trim().to_string();
            if !segment.is_empty()
                && entry.segments.len() < MAX_SEGMENTS_PER_MEETING
                && !entry.segments.contains(&segment)
            {
                entry.segments.push(segment);
            }
        }
    }

    let mut ranked: Vec<(String, MeetingHits)> = hits.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.keywords
            .len()
            .cmp(&a.1.keywords.len())
            .then_with(|| b.1.segments.len().cmp(&a.1.segments.len()))
            .then_with(|| b.1.created_at.cmp(&a.1.created_at))
    });
    ranked.truncate(MAX_CONTEXT_MEETINGS);
    Ok(ranked)
}

/// Fallback when keyword search finds nothing (e.g. "what was my meeting
/// today about?"): ground the answer in the most recent meeting.
async fn load_recent_meeting_hits(
    pool: &SqlitePool,
    meeting_id: &str,
    title: &str,
    created_at: &str,
) -> Result<MeetingHits, String> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT transcript FROM transcripts WHERE meeting_id = ? ORDER BY rowid LIMIT ?",
    )
    .bind(meeting_id)
    .bind(FALLBACK_TRANSCRIPT_SEGMENTS)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("Failed to load recent meeting transcript: {}", e))?;

    Ok(MeetingHits {
        title: title.to_string(),
        created_at: created_at.to_string(),
        keywords: HashSet::new(),
        segments: rows
            .into_iter()
            .map(|(t,)| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect(),
        summary: None,
    })
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
    context: &[(String, MeetingHits)],
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
    }
    let mut used_chars = 0usize;
    for (_, meeting) in context {
        if used_chars >= MAX_TOTAL_CONTEXT_CHARS {
            break;
        }
        prompt.push_str(&format!(
            "### {} ({})\n",
            meeting.title,
            date_only(&meeting.created_at)
        ));

        if let Some(summary) = &meeting.summary {
            let budget =
                MAX_SUMMARY_CHARS_PER_MEETING.min(MAX_TOTAL_CONTEXT_CHARS - used_chars);
            let text = truncate_chars(summary, budget);
            used_chars += text.chars().count();
            prompt.push_str(&format!("Saved summary:\n{}\n", text));
        }

        if !meeting.segments.is_empty() && used_chars < MAX_TOTAL_CONTEXT_CHARS {
            let budget = MAX_CHARS_PER_MEETING.min(MAX_TOTAL_CONTEXT_CHARS - used_chars);
            let text = truncate_chars(&meeting.segments.join("\n"), budget);
            used_chars += text.chars().count();
            prompt.push_str(&format!("Transcript excerpts:\n{}\n", text));
        }
        prompt.push('\n');
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
Transcripts are raw speech-to-text, so tolerate transcription errors. If the context does not \
contain the answer, say so plainly instead of guessing. Be concise and answer in Markdown.";

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

    // 1. Retrieve grounding context from the local database.
    let keywords = gather_keywords(&question, &history);
    info!("chat_ask keywords: {:?}", keywords);
    let mut context = if keywords.is_empty() {
        Vec::new()
    } else {
        retrieve_context(pool, &keywords).await?
    };
    let recents = recent_meetings(pool).await?;

    // Nothing matched (or the question was all stopwords, e.g. "what was my
    // meeting today about?") - ground in the most recent meeting instead.
    if context.is_empty() {
        if let Some((id, title, created_at)) = recents.first() {
            let fallback = load_recent_meeting_hits(pool, id, title, created_at).await?;
            context.push((id.clone(), fallback));
        }
    }

    // Attach saved AI summaries - the densest context available per meeting.
    for (id, meeting) in context.iter_mut() {
        meeting.summary = fetch_summary_text(pool, id).await;
    }

    // 2. Resolve the configured LLM (same source of truth as summaries).
    let setting = SettingsRepository::get_model_config(pool)
        .await
        .map_err(|e| format!("Failed to load model settings: {}", e))?
        .ok_or_else(|| {
            "No summarization model configured. Pick one in Settings first.".to_string()
        })?;
    let provider = LLMProvider::from_str(&setting.provider)?;
    let model_name = setting.model.clone();
    if model_name.trim().is_empty() {
        return Err("No model selected. Pick one in Settings first.".to_string());
    }

    let api_key = if provider == LLMProvider::Ollama
        || provider == LLMProvider::BuiltInAI
        || provider == LLMProvider::CustomOpenAI
    {
        String::new()
    } else {
        match SettingsRepository::get_api_key(pool, &setting.provider).await {
            Ok(Some(key)) if !key.is_empty() => key,
            Ok(_) => return Err(format!("API key not found for {}", setting.provider)),
            Err(e) => {
                return Err(format!(
                    "Failed to retrieve API key for {}: {}",
                    setting.provider, e
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

    // 3. Ask the model.
    let today = chrono::Local::now().format("%Y-%m-%d (%A)").to_string();
    let user_prompt = build_user_prompt(&question, &today, &history, &recents, &context);
    let client = Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

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
    fn extract_keywords_dedupes_and_caps() {
        let keywords = extract_keywords(
            "alpha alpha bravo charlie delta echo foxtrot golf hotel india juliett",
        );
        assert_eq!(keywords.len(), MAX_KEYWORDS);
        assert_eq!(keywords[0], "alpha");
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
    fn gather_keywords_prefers_current_question() {
        let history = vec![user_message("tell me about the roadmap")];
        let keywords = gather_keywords("what about the budget and hiring plan?", &history);
        assert_eq!(keywords[0], "budget");
        assert!(keywords.contains(&"hiring".to_string()));
    }

    #[test]
    fn escape_like_pattern_escapes_wildcards() {
        assert_eq!(escape_like_pattern("50%_a\\b"), "50\\%\\_a\\\\b");
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
            MeetingHits {
                title: "Standup".to_string(),
                created_at: "2026-07-01T10:00:00+00:00".to_string(),
                keywords: HashSet::new(),
                segments: vec!["we shipped the roadmap".to_string()],
                summary: Some("- roadmap shipped".to_string()),
            },
        )];
        let history = vec![user_message("earlier question")];

        let prompt = build_user_prompt(
            "what about the roadmap?",
            "2026-07-10 (Friday)",
            &history,
            &recents,
            &context,
        );

        assert!(prompt.contains("Today's date: 2026-07-10 (Friday)"));
        assert!(prompt.contains("- Standup (2026-07-01)"));
        assert!(prompt.contains("### Standup (2026-07-01)"));
        assert!(prompt.contains("Saved summary:\n- roadmap shipped"));
        assert!(prompt.contains("Transcript excerpts:\nwe shipped the roadmap"));
        assert!(prompt.contains("User: earlier question"));
        assert!(prompt.contains("## Question\nwhat about the roadmap?"));
    }
}
