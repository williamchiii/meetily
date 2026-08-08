// summary/vision.rs
//
// Reads an image with whichever LLM the user has selected for summaries.
//
// Screenshots shared during a meeting - a slide, a dashboard, an architecture
// diagram - carry information nobody says out loud, so the transcript alone cannot
// capture it. This module turns one image into text that the summary prompt can then
// treat like any other context.
//
// Each provider wants its image in a different shape, so the request bodies are built
// per provider rather than through the shared `ChatRequest`, whose `content` is a
// plain String with nowhere to put an image.

use anyhow::{anyhow, Result};
use reqwest::{header, Client};
use serde_json::json;
use std::time::Duration;
use tracing::info;

use super::llm_client::LLMProvider;

const VISION_TIMEOUT: Duration = Duration::from_secs(120);

/// Ask for the facts, not for prose: this text is going into a summary prompt, so
/// speculation or hedging would end up presented to the user as meeting content.
const EXTRACTION_SYSTEM_PROMPT: &str = "\
You extract information from a screenshot that someone shared during a meeting.

Report ONLY what is actually visible. Transcribe text, table values, chart axes and \
labels, code, and diagram relationships exactly as shown. Note the kind of artefact \
it is (slide, dashboard, spreadsheet, code editor, design mock, error message).

Do not speculate about intent, do not invent numbers, and do not add commentary. If \
the image is unreadable or empty, say exactly: NO_READABLE_CONTENT.

Be thorough but compact - this becomes context for a meeting summary.";

const EXTRACTION_USER_PROMPT: &str =
    "Extract everything informative from this screenshot shared during the meeting.";

/// What the caller gets back for one image.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExtractedContext {
    pub text: String,
    /// The provider/model that read it, so the UI can say where this came from.
    pub model: String,
}

/// Providers that cannot accept an image at all, regardless of model.
fn rejects_images(provider: &LLMProvider) -> Option<&'static str> {
    match provider {
        LLMProvider::BuiltInAI => Some(
            "The built-in local model cannot read images. \
             Pick a vision-capable model (Claude, GPT-4o, Gemini, or an Ollama model \
             like llama3.2-vision) in Settings to use screenshots.",
        ),
        _ => None,
    }
}

/// Read an image with the given provider/model and return the text it found.
///
/// `image_base64` must be the raw base64 payload with no `data:` prefix.
pub async fn extract_image_context(
    client: &Client,
    provider: &LLMProvider,
    model_name: &str,
    api_key: &str,
    image_base64: &str,
    mime_type: &str,
    ollama_endpoint: Option<&str>,
    custom_openai_endpoint: Option<&str>,
) -> Result<ExtractedContext> {
    if let Some(reason) = rejects_images(provider) {
        return Err(anyhow!(reason.to_string()));
    }

    info!(
        "Extracting screenshot context with {:?}/{} ({} bytes of base64)",
        provider,
        model_name,
        image_base64.len()
    );

    let text = match provider {
        LLMProvider::Claude => {
            claude_vision(client, model_name, api_key, image_base64, mime_type).await?
        }
        LLMProvider::Ollama => {
            ollama_vision(client, model_name, image_base64, ollama_endpoint).await?
        }
        LLMProvider::OpenAI => {
            openai_vision(
                client,
                model_name,
                api_key,
                image_base64,
                mime_type,
                "https://api.openai.com/v1/chat/completions",
                None,
            )
            .await?
        }
        LLMProvider::Groq => {
            openai_vision(
                client,
                model_name,
                api_key,
                image_base64,
                mime_type,
                "https://api.groq.com/openai/v1/chat/completions",
                None,
            )
            .await?
        }
        LLMProvider::OpenRouter => {
            openai_vision(
                client,
                model_name,
                api_key,
                image_base64,
                mime_type,
                "https://openrouter.ai/api/v1/chat/completions",
                None,
            )
            .await?
        }
        LLMProvider::CustomOpenAI => {
            let base = custom_openai_endpoint
                .ok_or_else(|| anyhow!("No endpoint configured for the custom OpenAI provider"))?;
            let url = format!("{}/chat/completions", base.trim_end_matches('/'));
            openai_vision(client, model_name, api_key, image_base64, mime_type, &url, None).await?
        }
        LLMProvider::BuiltInAI => unreachable!("rejected above"),
    };

    let text = text.trim().to_string();

    if text.is_empty() || text.contains("NO_READABLE_CONTENT") {
        return Err(anyhow!(
            "No readable content was found in that screenshot"
        ));
    }

    Ok(ExtractedContext {
        text,
        model: format!("{:?}/{}", provider, model_name),
    })
}

/// OpenAI-compatible vision: an `image_url` part holding a data URI.
/// Groq, OpenRouter and custom OpenAI endpoints all speak this shape.
async fn openai_vision(
    client: &Client,
    model_name: &str,
    api_key: &str,
    image_base64: &str,
    mime_type: &str,
    url: &str,
    extra_header: Option<(&str, &str)>,
) -> Result<String> {
    // Older models take `max_tokens`; newer OpenAI ones reject it and demand
    // `max_completion_tokens`. Rather than track which model wants which, send the
    // common one and let the provider's own complaint drive a single retry.
    let mut token_param = "max_tokens";

    loop {
        let body = json!({
            "model": model_name,
            token_param: 1500,
            "messages": [
                { "role": "system", "content": EXTRACTION_SYSTEM_PROMPT },
                { "role": "user", "content": [
                    { "type": "text", "text": EXTRACTION_USER_PROMPT },
                    { "type": "image_url", "image_url": {
                        "url": format!("data:{};base64,{}", mime_type, image_base64)
                    }}
                ]}
            ]
        });

        let mut request = client
            .post(url)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {}", api_key))
            .timeout(VISION_TIMEOUT)
            .json(&body);

        if let Some((name, value)) = extra_header {
            request = request.header(name, value);
        }

        let response = request.send().await?;
        let status = response.status();
        let raw = response.text().await.unwrap_or_default();

        if !status.is_success() {
            if token_param == "max_tokens" && wants_max_completion_tokens(&raw) {
                info!("{} rejects max_tokens; retrying with max_completion_tokens", model_name);
                token_param = "max_completion_tokens";
                continue;
            }
            return Err(provider_error(&raw, status, model_name));
        }

        let payload: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| anyhow!("Could not parse the vision response: {} (body: {})", e, raw))?;

        return payload["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("Unexpected vision response shape: {}", payload));
    }
}

/// Whether a 400 is the provider telling us to rename the token limit parameter.
fn wants_max_completion_tokens(body: &str) -> bool {
    let lowered = body.to_lowercase();
    lowered.contains("max_completion_tokens") && lowered.contains("max_tokens")
}

/// Claude vision: a base64 `image` source block.
async fn claude_vision(
    client: &Client,
    model_name: &str,
    api_key: &str,
    image_base64: &str,
    mime_type: &str,
) -> Result<String> {
    let body = json!({
        "model": model_name,
        "max_tokens": 1500,
        "system": EXTRACTION_SYSTEM_PROMPT,
        "messages": [
            { "role": "user", "content": [
                { "type": "image", "source": {
                    "type": "base64",
                    "media_type": mime_type,
                    "data": image_base64
                }},
                { "type": "text", "text": EXTRACTION_USER_PROMPT }
            ]}
        ]
    });

    let response = client
        .post("https://api.anthropic.com/v1/messages")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .timeout(VISION_TIMEOUT)
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    let payload: serde_json::Value = parse_response(response, status, model_name).await?;

    payload["content"][0]["text"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("Unexpected Claude vision response shape: {}", payload))
}

/// Ollama vision: images ride alongside the message as a base64 array.
async fn ollama_vision(
    client: &Client,
    model_name: &str,
    image_base64: &str,
    endpoint: Option<&str>,
) -> Result<String> {
    let base = endpoint.unwrap_or("http://localhost:11434");
    let url = format!("{}/api/chat", base.trim_end_matches('/'));

    let body = json!({
        "model": model_name,
        "stream": false,
        "messages": [
            { "role": "system", "content": EXTRACTION_SYSTEM_PROMPT },
            { "role": "user", "content": EXTRACTION_USER_PROMPT, "images": [image_base64] }
        ]
    });

    let response = client
        .post(&url)
        .header(header::CONTENT_TYPE, "application/json")
        .timeout(VISION_TIMEOUT)
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    let payload: serde_json::Value = parse_response(response, status, model_name).await?;

    payload["message"]["content"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("Unexpected Ollama vision response shape: {}", payload))
}

/// Turn a provider error into something a user can act on.
///
/// "Model does not support images" is by far the most likely failure here, and the
/// raw provider JSON does not make that obvious, so it is called out explicitly.
async fn parse_response(
    response: reqwest::Response,
    status: reqwest::StatusCode,
    model_name: &str,
) -> Result<serde_json::Value> {
    let body = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(provider_error(&body, status, model_name));
    }

    serde_json::from_str(&body)
        .map_err(|e| anyhow!("Could not parse the vision response: {} (body: {})", e, body))
}

/// Translate a provider failure into something the user can act on.
///
/// "This model cannot see images" is the likeliest failure and the raw JSON does not
/// make it obvious, so it is called out by name.
fn provider_error(body: &str, status: reqwest::StatusCode, model_name: &str) -> anyhow::Error {
    let lowered = body.to_lowercase();
    if lowered.contains("image")
        && (lowered.contains("not support")
            || lowered.contains("unsupported")
            || lowered.contains("invalid_type"))
    {
        return anyhow!(
            "{} cannot read images. Choose a vision-capable model in Settings.",
            model_name
        );
    }
    anyhow!("Vision request failed ({}): {}", status, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live end-to-end check against a real provider.
    ///
    /// Ignored by default because it costs money and needs credentials. Run with:
    ///   MEETILY_VISION_PROVIDER=openai MEETILY_VISION_MODEL=gpt-4o-mini \
    ///   MEETILY_VISION_KEY=sk-... MEETILY_VISION_IMAGE=/path/to.png \
    ///   cargo test --lib vision::tests::live -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_extraction_reads_a_real_screenshot() {
        use base64::Engine;

        let provider_name = std::env::var("MEETILY_VISION_PROVIDER").expect("MEETILY_VISION_PROVIDER");
        let model = std::env::var("MEETILY_VISION_MODEL").expect("MEETILY_VISION_MODEL");
        let key = std::env::var("MEETILY_VISION_KEY").unwrap_or_default();
        let image = std::env::var("MEETILY_VISION_IMAGE").expect("MEETILY_VISION_IMAGE");
        let endpoint = std::env::var("MEETILY_VISION_ENDPOINT").ok();

        let provider = LLMProvider::from_str(&provider_name).expect("provider");
        let bytes = std::fs::read(&image).expect("read test image");
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);

        let client = Client::new();
        let result = extract_image_context(
            &client,
            &provider,
            &model,
            &key,
            &encoded,
            "image/png",
            endpoint.as_deref(),
            None,
        )
        .await;

        match result {
            Ok(extracted) => {
                println!("--- extracted by {} ---\n{}\n---", extracted.model, extracted.text);
                assert!(!extracted.text.is_empty());
            }
            Err(e) => panic!("live extraction failed: {}", e),
        }
    }

    #[test]
    fn the_local_model_is_rejected_with_an_actionable_message() {
        let reason = rejects_images(&LLMProvider::BuiltInAI).expect("should reject");
        assert!(reason.contains("vision-capable"), "got {}", reason);
    }

    #[test]
    fn cloud_and_ollama_providers_are_allowed_through() {
        for provider in [
            LLMProvider::Claude,
            LLMProvider::OpenAI,
            LLMProvider::Groq,
            LLMProvider::OpenRouter,
            LLMProvider::Ollama,
            LLMProvider::CustomOpenAI,
        ] {
            assert!(rejects_images(&provider).is_none(), "{:?} should be allowed", provider);
        }
    }

    #[test]
    fn detects_the_rename_this_parameter_complaint() {
        let real = r#"{"error":{"message":"Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead.","code":"unsupported_parameter"}}"#;
        assert!(wants_max_completion_tokens(real));
        // Must not retry on unrelated failures, or every error costs a second call
        assert!(!wants_max_completion_tokens(r#"{"error":{"message":"invalid api key"}}"#));
        assert!(!wants_max_completion_tokens(r#"{"error":{"message":"max_tokens too large"}}"#));
    }

    #[test]
    fn a_model_without_vision_is_named_in_the_error() {
        let err = provider_error(
            r#"{"error":{"message":"This model does not support image input"}}"#,
            reqwest::StatusCode::BAD_REQUEST,
            "gpt-4-turbo",
        );
        assert!(err.to_string().contains("gpt-4-turbo cannot read images"), "got {}", err);
    }

    #[test]
    fn the_extraction_prompt_forbids_speculation() {
        // This text ends up inside a summary, so invented detail would surface to the
        // user as if it were meeting content
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("ONLY what is actually visible"));
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("Do not speculate"));
        assert!(EXTRACTION_SYSTEM_PROMPT.contains("NO_READABLE_CONTENT"));
    }
}
