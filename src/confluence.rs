//! Confluence REST API client and page-ID resolver.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::Client;
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_DISPOSITION, CONTENT_TYPE, HeaderMap, HeaderValue,
};
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, warn};
use url::Url;

use crate::utils::{
    HeaderHints, URI_COMPONENT, decode_html_attribute, ensure_dir,
    get_file_name_from_url_or_headers, resolve_url, to_markdown_asset_path,
};

// ── Public types ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PageResult {
    pub title: String,
    pub content_json: String,
    pub storage_html: Option<String>,
    pub export_html: String,
    pub webui: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Attachment {
    pub id: String,
    pub title: String,
    pub media_type: Option<String>,
    pub download_path: Option<String>,
    #[allow(dead_code)]
    pub webui_path: Option<String>,
}

pub struct AttachmentMaps {
    pub by_title: HashMap<String, Attachment>,
}

// ── Authentication ─────────────────────────────────────────────────

/// Reads authentication environment variables and returns the full
/// Authorization header value string.
///
/// Priority:
/// 1. If CONFLUENCE2MD_PERSONAL_ACCESS_TOKEN is set (non-empty, non-whitespace) → "Bearer <token>"
/// 2. If both CONFLUENCE2MD_USERNAME and CONFLUENCE2MD_API_TOKEN are set → "Basic <base64>"
/// 3. Otherwise → error
pub fn resolve_auth() -> Result<String> {
    let pat = std::env::var("CONFLUENCE2MD_PERSONAL_ACCESS_TOKEN").ok();
    let username = std::env::var("CONFLUENCE2MD_USERNAME").ok();
    let api_token = std::env::var("CONFLUENCE2MD_API_TOKEN").ok();

    // 1. Check PAT first
    if let Some(ref pat_val) = pat
        && !pat_val.is_empty()
    {
        // PAT is set (non-empty) — validate it's not whitespace-only
        if pat_val.trim().is_empty() {
            bail!("CONFLUENCE2MD_PERSONAL_ACCESS_TOKEN is set but empty or whitespace-only.");
        }
        return Ok(format!("Bearer {pat_val}"));
    }

    // 2. PAT is unset or empty — check Cloud pair
    let has_username = matches!(&username, Some(u) if !u.is_empty());
    let has_api_token = matches!(&api_token, Some(t) if !t.is_empty());

    if has_username && has_api_token {
        // 3. Both Cloud vars set — validate neither is whitespace-only
        let u = username.unwrap();
        let t = api_token.unwrap();
        if u.trim().is_empty() {
            bail!("CONFLUENCE2MD_USERNAME must not be empty or whitespace-only.");
        }
        if t.trim().is_empty() {
            bail!("CONFLUENCE2MD_API_TOKEN must not be empty or whitespace-only.");
        }
        let credentials = format!("{u}:{t}");
        let encoded = base64_encode(credentials.as_bytes());
        return Ok(format!("Basic {encoded}"));
    }

    // 4. Exactly one Cloud var set — error naming the missing one
    if has_username && !has_api_token {
        bail!(
            "CONFLUENCE2MD_USERNAME is set but CONFLUENCE2MD_API_TOKEN is missing. \
             Both are required for Cloud authentication."
        );
    }
    if has_api_token && !has_username {
        bail!(
            "CONFLUENCE2MD_API_TOKEN is set but CONFLUENCE2MD_USERNAME is missing. \
             Both are required for Cloud authentication."
        );
    }

    // 5. Nothing set at all
    bail!(
        "No authentication configured. Set CONFLUENCE2MD_PERSONAL_ACCESS_TOKEN for Bearer auth, \
         or both CONFLUENCE2MD_USERNAME and CONFLUENCE2MD_API_TOKEN for Basic auth."
    );
}

// ── Base64 encoding ────────────────────────────────────────────────

/// RFC 4648 standard Base64 encoding (no line breaks, standard alphabet with padding).
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let chunks = input.chunks(3);

    for chunk in chunks {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };

        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);

        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }

        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }

    out
}

// ── HTTP helpers ───────────────────────────────────────────────────

fn auth_headers(auth_value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    if let Ok(v) = HeaderValue::from_str(auth_value) {
        headers.insert(AUTHORIZATION, v);
    }
    headers
}

fn binary_auth_headers(auth_value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(auth_value) {
        headers.insert(AUTHORIZATION, v);
    }
    headers
}

pub async fn fetch_json(client: &Client, url: &str, token: &str) -> Result<Value> {
    let text = fetch_json_text(client, url, token).await?;
    parse_json_text(&text)
}

async fn fetch_json_text(client: &Client, url: &str, token: &str) -> Result<String> {
    debug!("Downloading JSON: url: {url}");
    let response = client
        .get(url)
        .headers(auth_headers(token))
        .send()
        .await
        .with_context(|| format!("HTTP request failed: {url}"))?;
    let status = response.status();
    let text = response.text().await.context("read body")?;
    if !status.is_success() {
        bail!(
            "Confluence API error: {} {}\n{}",
            status.as_u16(),
            status.canonical_reason().unwrap_or(""),
            text
        );
    }
    debug!(
        "Downloaded JSON: length: {}, starts with: {}",
        text.len(),
        &text.chars().take(80).collect::<String>()
    );
    Ok(text)
}

fn parse_json_text(text: &str) -> Result<Value> {
    serde_json::from_str(text).map_err(|_| anyhow!("Failed to parse JSON response:\n{text}"))
}

// ── Page fetching ──────────────────────────────────────────────────

const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

fn encode_path_segment(s: &str) -> String {
    utf8_percent_encode(s, PATH_SEGMENT).to_string()
}

pub async fn fetch_confluence_page(
    client: &Client,
    page_id: &str,
    base_url: &str,
    token: &str,
) -> Result<PageResult> {
    let url = format!(
        "{base}/rest/api/content/{id}?expand=body.storage,body.export_view",
        base = base_url,
        id = encode_path_segment(page_id),
    );
    let content_json = fetch_json_text(client, &url, token).await?;
    let data = parse_json_text(&content_json)?;
    let title = data
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("page-{page_id}"));

    let body = data.get("body");
    let storage_html = body
        .and_then(|b| b.get("storage"))
        .and_then(|v| v.get("value"))
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let export_html = body
        .and_then(|b| b.get("export_view"))
        .and_then(|v| v.get("value"))
        .and_then(|v| v.as_str())
        .map(str::to_owned);

    let export_html = export_html.ok_or_else(|| {
        let keys: Vec<String> = data
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        anyhow!(
            "export_view body not available. Response keys: {}",
            keys.join(", ")
        )
    })?;

    let webui = data
        .get("_links")
        .and_then(|v| v.get("webui"))
        .and_then(|v| v.as_str())
        .map(|s| format!("{base_url}{s}"));

    Ok(PageResult {
        title,
        content_json,
        storage_html,
        export_html,
        webui,
    })
}

// ── Attachments ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AttachmentRaw {
    id: String,
    title: String,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default)]
    extensions: Option<Value>,
    #[serde(default, rename = "_links")]
    links: Option<Value>,
}

pub async fn list_attachments(
    client: &Client,
    page_id: &str,
    base_url: &str,
    token: &str,
) -> Result<Vec<Attachment>> {
    let url = format!(
        "{base}/rest/api/content/{id}/child/attachment?limit=1000",
        base = base_url,
        id = encode_path_segment(page_id),
    );
    let data = fetch_json(client, &url, token).await?;
    let results = data
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut out = Vec::with_capacity(results.len());
    for raw in results {
        let parsed: AttachmentRaw = match serde_json::from_value(raw) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let media_type = parsed
            .metadata
            .as_ref()
            .and_then(|m| m.get("mediaType"))
            .and_then(|v| v.as_str())
            .or_else(|| {
                parsed
                    .extensions
                    .as_ref()
                    .and_then(|e| e.get("mediaType"))
                    .and_then(|v| v.as_str())
            })
            .map(str::to_owned);
        let download_path = parsed
            .links
            .as_ref()
            .and_then(|l| l.get("download"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let webui_path = parsed
            .links
            .as_ref()
            .and_then(|l| l.get("webui"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        out.push(Attachment {
            id: parsed.id,
            title: parsed.title,
            media_type,
            download_path,
            webui_path,
        });
    }
    Ok(out)
}

pub fn attachment_download_url(base_url: &str, page_id: &str, attachment: &Attachment) -> String {
    if let Some(p) = &attachment.download_path {
        return resolve_url(p, base_url);
    }
    format!(
        "{base}/download/attachments/{page}/{title}",
        base = base_url,
        page = encode_path_segment(page_id),
        title = encode_path_segment(&attachment.title),
    )
}

pub fn build_attachment_maps(attachments: &[Attachment]) -> AttachmentMaps {
    let mut by_title = HashMap::with_capacity(attachments.len());
    for a in attachments {
        by_title.insert(a.title.clone(), a.clone());
    }
    AttachmentMaps { by_title }
}

// ── Binary downloads ───────────────────────────────────────────────

pub struct DownloadBinaryOptions<'a> {
    pub url: &'a str,
    pub token: &'a str,
    pub assets_abs_dir: &'a Path,
    pub markdown_image_prefix: &'a str,
    pub fallback_base_name: &'a str,
    pub used_names: &'a mut HashSet<String>,
}

pub async fn download_binary_to_asset(
    client: &Client,
    opts: DownloadBinaryOptions<'_>,
) -> Result<String> {
    let response = client
        .get(opts.url)
        .headers(binary_auth_headers(opts.token))
        .send()
        .await
        .with_context(|| format!("HTTP request failed: {}", opts.url))?;
    if !response.status().is_success() {
        bail!(
            "Failed to fetch binary: {} {} {}",
            response.status().as_u16(),
            response.status().canonical_reason().unwrap_or(""),
            opts.url
        );
    }

    let content_disposition = response
        .headers()
        .get(CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let hints = HeaderHints {
        content_disposition: content_disposition.as_deref(),
        content_type: content_type.as_deref(),
    };

    let file_name = get_file_name_from_url_or_headers(
        opts.url,
        &hints,
        opts.fallback_base_name,
        opts.used_names,
    );

    let bytes = response.bytes().await.context("read response body")?;
    let file_path = opts.assets_abs_dir.join(&file_name);
    tokio::fs::write(&file_path, &bytes)
        .await
        .with_context(|| format!("writing {}", file_path.display()))?;

    Ok(to_markdown_asset_path(
        opts.markdown_image_prefix,
        &file_name,
    ))
}

pub struct DownloadAttachmentOptions<'a> {
    pub page_id: &'a str,
    pub attachment: &'a Attachment,
    pub base_url: &'a str,
    pub token: &'a str,
    pub assets_abs_dir: &'a Path,
    pub markdown_image_prefix: &'a str,
    pub used_names: &'a mut HashSet<String>,
}

pub async fn download_attachment_to_asset(
    client: &Client,
    opts: DownloadAttachmentOptions<'_>,
) -> Result<String> {
    let url = attachment_download_url(opts.base_url, opts.page_id, opts.attachment);
    let fallback = if opts.attachment.title.is_empty() {
        "attachment".to_owned()
    } else {
        opts.attachment.title.clone()
    };
    download_binary_to_asset(
        client,
        DownloadBinaryOptions {
            url: &url,
            token: opts.token,
            assets_abs_dir: opts.assets_abs_dir,
            markdown_image_prefix: opts.markdown_image_prefix,
            fallback_base_name: &fallback,
            used_names: opts.used_names,
        },
    )
    .await
}

// ── Image rewriting ────────────────────────────────────────────────

pub struct DownloadImagesOptions<'a> {
    pub base_url: &'a str,
    pub personal_access_token: &'a str,
    pub assets_abs_dir: &'a Path,
    pub markdown_image_prefix: &'a str,
    pub used_names: &'a mut HashSet<String>,
}

pub async fn download_images_and_rewrite_html(
    client: &Client,
    html: &str,
    opts: DownloadImagesOptions<'_>,
) -> Result<String> {
    use once_cell::sync::Lazy;
    use regex::Regex;
    static IMG_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?is)<img\b[^>]*\bsrc=(?:"([^"]*)"|'([^']*)')[^>]*>"#).unwrap());
    static PLANTUML_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)/rest/plantuml/").unwrap());

    let matches: Vec<String> = IMG_RE
        .captures_iter(html)
        .filter_map(|c| c.get(1).or_else(|| c.get(2)).map(|g| g.as_str().to_owned()))
        .collect();

    if matches.is_empty() {
        return Ok(html.to_owned());
    }

    ensure_dir(opts.assets_abs_dir).await?;
    let mut src_to_local: HashMap<String, String> = HashMap::new();

    for (i, original_src) in matches.iter().enumerate() {
        if src_to_local.contains_key(original_src) {
            continue;
        }
        if PLANTUML_RE.is_match(original_src) {
            continue;
        }
        if is_local_markdown_asset(original_src, opts.markdown_image_prefix) {
            continue;
        }
        let absolute = resolve_url(original_src, opts.base_url);
        let fallback = format!("image_{}", i + 1);
        let result = download_binary_to_asset(
            client,
            DownloadBinaryOptions {
                url: &absolute,
                token: opts.personal_access_token,
                assets_abs_dir: opts.assets_abs_dir,
                markdown_image_prefix: opts.markdown_image_prefix,
                fallback_base_name: &fallback,
                used_names: opts.used_names,
            },
        )
        .await;
        match result {
            Ok(local_path) => {
                src_to_local.insert(original_src.clone(), local_path);
            }
            Err(_) => {
                warn!("Failed to fetch image: {absolute}");
            }
        }
    }

    static REPLACE_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?is)(<img\b[^>]*\bsrc=)(?:"([^"]*)"|'([^']*)')"#).unwrap());

    let result = REPLACE_RE.replace_all(html, |caps: &regex::Captures<'_>| {
        let prefix = &caps[1];
        let (quote, src) = if let Some(m) = caps.get(2) {
            ('"', m.as_str())
        } else if let Some(m) = caps.get(3) {
            ('\'', m.as_str())
        } else {
            return caps[0].to_owned();
        };
        match src_to_local.get(src) {
            Some(local) => format!("{prefix}{quote}{local}{quote}"),
            None => caps[0].to_owned(),
        }
    });

    Ok(result.into_owned())
}

fn is_local_markdown_asset(src: &str, markdown_image_prefix: &str) -> bool {
    let encoded_prefix = utf8_percent_encode(markdown_image_prefix, &URI_COMPONENT).to_string();
    src.starts_with(&format!("{markdown_image_prefix}/"))
        || src.starts_with(&format!("{encoded_prefix}%2F"))
}

// ── Page ID resolution ─────────────────────────────────────────────

const QUERY_VAL: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

fn encode_query_value(s: &str) -> String {
    utf8_percent_encode(s, QUERY_VAL).to_string()
}

pub async fn lookup_page_id_by_space_and_title(
    client: &Client,
    space_key: &str,
    title: &str,
    base_url: &str,
    token: &str,
) -> Result<String> {
    let url = format!(
        "{base}/rest/api/content?spaceKey={space}&title={title}&type=page",
        base = base_url,
        space = encode_query_value(space_key),
        title = encode_query_value(title),
    );
    let data = fetch_json(client, &url, token).await?;
    let results = data
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if results.is_empty() {
        bail!("Page not found for spaceKey=\"{space_key}\" title=\"{title}\"");
    }
    let id = results[0]
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Unexpected page id type in response"))?;
    Ok(id.to_owned())
}

pub async fn resolve_page_id_from_url(
    client: &Client,
    page_url: &str,
    base_url: &str,
    token: &str,
) -> Result<String> {
    let parsed = Url::parse(page_url).context("invalid URL")?;

    // 1. pageId query param.
    for (k, v) in parsed.query_pairs() {
        if k == "pageId" {
            return Ok(v.into_owned());
        }
    }

    // 2. /spaces/SPACE/pages/{pageId}.
    use once_cell::sync::Lazy;
    use regex::Regex;
    static WIKI_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"/spaces/[^/]+/pages/(\d+)").unwrap());
    if let Some(c) = WIKI_RE.captures(parsed.path()) {
        return Ok(c[1].to_owned());
    }

    // 3. spaceKey + title query params.
    let mut space_key: Option<String> = None;
    let mut title_param: Option<String> = None;
    for (k, v) in parsed.query_pairs() {
        match k.as_ref() {
            "spaceKey" => space_key = Some(v.into_owned()),
            "title" => title_param = Some(v.into_owned()),
            _ => {}
        }
    }
    if let (Some(space), Some(title)) = (&space_key, &title_param) {
        return lookup_page_id_by_space_and_title(client, space, title, base_url, token).await;
    }

    // 4. /display/SPACEKEY/Page+Title.
    static DISPLAY_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"/display/([^/]+)/(.+)").unwrap());
    if let Some(c) = DISPLAY_RE.captures(parsed.path()) {
        let space = decode_path_segment(&c[1]);
        let title = decode_path_segment(&c[2]).replace('+', " ");
        return lookup_page_id_by_space_and_title(client, &space, &title, base_url, token).await;
    }

    bail!("Cannot determine page ID from URL: {page_url}")
}

fn decode_path_segment(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

// Helper for callers that need a configured HTTP client.
pub fn build_http_client() -> Result<Client> {
    Client::builder()
        .user_agent("confluence2md/1.2.0")
        .build()
        .context("build HTTP client")
}

// Hold a writeable path buffer for the assets dir to keep clippy happy.
fn _unused(_p: PathBuf) {}

#[allow(dead_code)]
pub(crate) fn _decode_attribute(s: &str) -> String {
    decode_html_attribute(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn resolve_page_id_extracts_pageid_query_param() {
        let client = Client::new();
        let url =
            "https://confluence.example.com/pages/viewpage.action?pageId=1082335934&spaceKey=DEMO";
        let id = resolve_page_id_from_url(&client, url, "https://confluence.example.com", "token")
            .await
            .unwrap();
        assert_eq!(id, "1082335934");
    }

    #[tokio::test]
    async fn resolve_page_id_extracts_pageid_from_spaces_path() {
        let client = Client::new();
        let url = "https://confluence.example.com/wiki/spaces/DEMO/pages/9876543/My+Page";
        let id = resolve_page_id_from_url(&client, url, "https://confluence.example.com", "token")
            .await
            .unwrap();
        assert_eq!(id, "9876543");
    }

    #[tokio::test]
    async fn resolve_page_id_extracts_pageid_from_spaces_path_no_title() {
        let client = Client::new();
        let url = "https://confluence.example.com/wiki/spaces/DEMO/pages/1111111";
        let id = resolve_page_id_from_url(&client, url, "https://confluence.example.com", "token")
            .await
            .unwrap();
        assert_eq!(id, "1111111");
    }

    #[tokio::test]
    async fn resolve_page_id_priority_pageid_over_space_title() {
        let client = Client::new();
        let url = "https://confluence.example.com/pages/viewpage.action?pageId=1082335934&spaceKey=DEMO&title=foo";
        let id = resolve_page_id_from_url(&client, url, "https://confluence.example.com", "token")
            .await
            .unwrap();
        assert_eq!(id, "1082335934");
    }

    #[tokio::test]
    async fn resolve_page_id_errors_for_unknown_url() {
        let client = Client::new();
        let url = "https://confluence.example.com/unknown/path";
        let err = resolve_page_id_from_url(&client, url, "https://confluence.example.com", "token")
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("Cannot determine page ID from URL")
        );
    }

    #[tokio::test]
    async fn resolve_page_id_looks_up_via_api_for_space_and_title() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/content"))
            .and(query_param("spaceKey", "DEMO"))
            .and(query_param("type", "page"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"results":[{"id":"555444"}]}"#),
            )
            .mount(&server)
            .await;

        let client = Client::new();
        let url = format!(
            "{}/pages/viewpage.action?spaceKey=DEMO&title=My+Page",
            server.uri()
        );
        let id = resolve_page_id_from_url(&client, &url, &server.uri(), "token")
            .await
            .unwrap();
        assert_eq!(id, "555444");
    }

    #[tokio::test]
    async fn resolve_page_id_looks_up_via_api_for_classic_display_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/content"))
            .and(query_param("spaceKey", "DEMO"))
            .and(query_param("title", "My Page Title"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"results":[{"id":"333222"}]}"#),
            )
            .mount(&server)
            .await;

        let client = Client::new();
        let url = format!("{}/display/DEMO/My+Page+Title", server.uri());
        let id = resolve_page_id_from_url(&client, &url, &server.uri(), "token")
            .await
            .unwrap();
        assert_eq!(id, "333222");
    }

    #[tokio::test]
    async fn resolve_page_id_handles_percent_encoded_title() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/content"))
            .and(query_param("spaceKey", "SAMPLE"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"results":[{"id":"777888"}]}"#),
            )
            .mount(&server)
            .await;

        let client = Client::new();
        let url = format!(
            "{}/pages/viewpage.action?spaceKey=SAMPLE&title=SampleManager_%E3%82%B7%E3%82%B9%E3%83%86%E3%83%A0%E8%A8%AD%E8%A8%88%E6%9B%B8_V1.00",
            server.uri()
        );
        let id = resolve_page_id_from_url(&client, &url, &server.uri(), "token")
            .await
            .unwrap();
        assert_eq!(id, "777888");
    }

    #[tokio::test]
    async fn rewrite_html_rewrites_image_src_when_url_contains_apostrophe() {
        let server = MockServer::start().await;
        // Confluence embeds the raw page title (with apostrophe) in the URL.
        let img_path = "/download/attachments/123/My%20team's%20page/image.png";
        Mock::given(method("GET"))
            .and(path(img_path))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"\x89PNG\r\n\x1a\n"))
            .mount(&server)
            .await;

        let html = format!(r#"<img src="{}{}" />"#, server.uri(), img_path);

        let tmp_dir = std::env::temp_dir().join(format!(
            "confluence2md_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let assets_dir = tmp_dir.join("assets");
        let client = Client::new();
        let mut used = HashSet::new();
        let result = download_images_and_rewrite_html(
            &client,
            &html,
            DownloadImagesOptions {
                base_url: &server.uri(),
                personal_access_token: "token",
                assets_abs_dir: &assets_dir,
                markdown_image_prefix: "assets",
                used_names: &mut used,
            },
        )
        .await
        .unwrap();

        assert!(
            !result.contains(&server.uri()),
            "src should be rewritten to local path, got: {result}"
        );
        assert!(
            result.contains("assets"),
            "src should point into assets dir, got: {result}"
        );
    }

    #[tokio::test]
    async fn fetch_confluence_page_preserves_content_json_response() {
        let server = MockServer::start().await;
        let body = r#"{"title":"Saved Page","body":{"storage":{"value":"<p>storage</p>"},"export_view":{"value":"<p>export</p>"}},"_links":{"webui":"/pages/123"}}"#;
        Mock::given(method("GET"))
            .and(path("/rest/api/content/123"))
            .and(query_param("expand", "body.storage,body.export_view"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let client = Client::new();
        let page = fetch_confluence_page(&client, "123", &server.uri(), "token")
            .await
            .unwrap();

        assert_eq!(page.title, "Saved Page");
        assert_eq!(page.content_json, body);
        assert_eq!(page.export_html, "<p>export</p>");
        assert_eq!(page.storage_html.as_deref(), Some("<p>storage</p>"));
    }

    // Regression test for: is_local_markdown_asset fails when the markdown_image_prefix
    // contains an apostrophe because the old code used PATH_SEGMENT encoding (which
    // encodes `'` → `%27`) while to_markdown_asset_path uses URI_COMPONENT encoding
    // (which keeps `'` as a literal). The mismatch caused local draw.io / image assets
    // to be treated as remote URLs and re-downloaded.
    #[test]
    fn is_local_markdown_asset_recognizes_apostrophe_in_prefix() {
        let prefix = "confluence2md's_test_assets";
        let src = to_markdown_asset_path(prefix, "single.drawio.png");
        // With the old PATH_SEGMENT encoding, encoded_prefix would contain %27 instead
        // of the literal apostrophe used by to_markdown_asset_path, so starts_with
        // would return false and this assertion would fail.
        assert!(
            is_local_markdown_asset(&src, prefix),
            "local asset not recognized (apostrophe in prefix): {src}"
        );
    }

    #[test]
    fn is_local_markdown_asset_recognizes_plain_prefix() {
        let prefix = "my_page_assets";
        let src = to_markdown_asset_path(prefix, "image.png");
        assert!(is_local_markdown_asset(&src, prefix));
    }

    #[test]
    fn is_local_markdown_asset_rejects_remote_url() {
        assert!(!is_local_markdown_asset(
            "https://example.com/image.png",
            "my_page_assets"
        ));
    }

    // Feature: cloud-api-token-auth, Property 1: base64 encoding correctness

    #[test]
    fn base64_encode_empty_input() {
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn base64_encode_rfc4648_test_vectors() {
        // RFC 4648 §10 test vectors
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_encode_confluence_credential_example() {
        // Typical Cloud auth: email:api-token
        let input = b"user@example.com:my-api-token";
        assert_eq!(
            base64_encode(input),
            "dXNlckBleGFtcGxlLmNvbTpteS1hcGktdG9rZW4="
        );
    }

    #[test]
    fn base64_encode_non_ascii_utf8() {
        // Non-ASCII UTF-8 bytes
        let input = "ü:tökën".as_bytes();
        assert_eq!(base64_encode(input), "w7w6dMO2a8Orbg==");
    }

    #[test]
    fn base64_encode_all_zero_bytes() {
        assert_eq!(base64_encode(&[0, 0, 0]), "AAAA");
        assert_eq!(base64_encode(&[0]), "AA==");
        assert_eq!(base64_encode(&[0, 0]), "AAA=");
    }

    #[test]
    fn base64_encode_all_ff_bytes() {
        assert_eq!(base64_encode(&[0xFF, 0xFF, 0xFF]), "////");
    }

    /// Validates: Requirements 3.1, 3.4, 6.1
    /// Explicitly verifies all padding cases based on input length mod 3:
    /// - len % 3 == 0 → no padding
    /// - len % 3 == 1 → two `=` padding chars
    /// - len % 3 == 2 → one `=` padding char
    #[test]
    fn base64_encode_padding_cases_by_length_mod_3() {
        // len % 3 == 0: no padding (lengths 3, 6)
        let no_pad_3 = base64_encode(b"abc");
        assert!(
            !no_pad_3.ends_with('='),
            "len=3 should have no padding, got: {no_pad_3}"
        );
        assert_eq!(no_pad_3, "YWJj");

        let no_pad_6 = base64_encode(b"abcdef");
        assert!(
            !no_pad_6.ends_with('='),
            "len=6 should have no padding, got: {no_pad_6}"
        );
        assert_eq!(no_pad_6, "YWJjZGVm");

        // len % 3 == 1: two padding chars (lengths 1, 4)
        let two_pad_1 = base64_encode(b"a");
        assert!(
            two_pad_1.ends_with("=="),
            "len=1 should have == padding, got: {two_pad_1}"
        );
        assert_eq!(two_pad_1, "YQ==");

        let two_pad_4 = base64_encode(b"abcd");
        assert!(
            two_pad_4.ends_with("=="),
            "len=4 should have == padding, got: {two_pad_4}"
        );
        assert_eq!(two_pad_4, "YWJjZA==");

        // len % 3 == 2: one padding char (lengths 2, 5)
        let one_pad_2 = base64_encode(b"ab");
        assert!(
            one_pad_2.ends_with('=') && !one_pad_2.ends_with("=="),
            "len=2 should have single = padding, got: {one_pad_2}"
        );
        assert_eq!(one_pad_2, "YWI=");

        let one_pad_5 = base64_encode(b"abcde");
        assert!(
            one_pad_5.ends_with('=') && !one_pad_5.ends_with("=="),
            "len=5 should have single = padding, got: {one_pad_5}"
        );
        assert_eq!(one_pad_5, "YWJjZGU=");
    }

    // Feature: cloud-api-token-auth, Property 1: base64 encoding correctness
    // Validates: Requirements 3.1, 3.4, 6.1

    /// Programmatic property coverage: verify all single-byte values (0x00..=0xFF)
    /// produce output containing only valid base64 characters and the expected length.
    #[test]
    fn base64_encode_property_all_single_bytes() {
        let valid_chars = |s: &str| -> bool {
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
        };

        for byte in 0x00u8..=0xFF {
            let input = [byte];
            let output = base64_encode(&input);

            // Single-byte input → output length must be 4 (4 * ceil(1/3) = 4)
            assert_eq!(
                output.len(),
                4,
                "byte 0x{byte:02X}: expected output length 4, got {}",
                output.len()
            );

            // Output must only contain valid base64 characters
            assert!(
                valid_chars(&output),
                "byte 0x{byte:02X}: output contains invalid base64 chars: {output}"
            );
        }
    }

    /// Programmatic property coverage: verify inputs of length 0..=4 produce correct
    /// output length and only valid base64 characters, covering all padding cases.
    #[test]
    fn base64_encode_property_lengths_0_through_4() {
        let valid_chars = |s: &str| -> bool {
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
        };

        // Expected output lengths: 4 * ceil(input_len / 3)
        // len 0 → 0, len 1 → 4, len 2 → 4, len 3 → 4, len 4 → 8
        let expected_output_lengths: [usize; 5] = [0, 4, 4, 4, 8];

        // Use representative byte patterns for each length
        let test_patterns: &[&[u8]] = &[
            &[],                       // length 0
            &[0x00],                   // length 1 (min byte)
            &[0x7F, 0x80],             // length 2 (boundary bytes)
            &[0xFF, 0x00, 0xAB],       // length 3 (mixed)
            &[0x01, 0x02, 0x03, 0x04], // length 4
        ];

        for pattern in test_patterns {
            let output = base64_encode(pattern);
            let expected_len = expected_output_lengths[pattern.len()];

            assert_eq!(
                output.len(),
                expected_len,
                "input len {}: expected output length {expected_len}, got {}. input: {pattern:?}",
                pattern.len(),
                output.len()
            );

            assert!(
                valid_chars(&output),
                "input len {}: output contains invalid base64 chars: {output}. input: {pattern:?}",
                pattern.len()
            );
        }

        // Additional patterns per length to cover more byte diversity
        let additional_patterns: &[&[u8]] = &[
            // More length 1 examples
            &[0xFF],
            &[0x41], // 'A'
            &[0x80],
            // More length 2 examples
            &[0x00, 0x00],
            &[0xFF, 0xFF],
            &[0x41, 0x42],
            // More length 3 examples
            &[0x00, 0x00, 0x00],
            &[0xFF, 0xFF, 0xFF],
            &[0x41, 0x42, 0x43],
            // More length 4 examples
            &[0x00, 0x00, 0x00, 0x00],
            &[0xFF, 0xFF, 0xFF, 0xFF],
            &[0xDE, 0xAD, 0xBE, 0xEF],
        ];

        for pattern in additional_patterns {
            let output = base64_encode(pattern);
            let expected_len = expected_output_lengths[pattern.len()];

            assert_eq!(
                output.len(),
                expected_len,
                "input len {}: expected output length {expected_len}, got {}. input: {pattern:?}",
                pattern.len(),
                output.len()
            );

            assert!(
                valid_chars(&output),
                "input len {}: output contains invalid base64 chars: {output}. input: {pattern:?}",
                pattern.len()
            );
        }
    }

    // Feature: cloud-api-token-auth — auth_headers / binary_auth_headers tests
    // Validates: Requirements 3.2, 4.2

    #[test]
    fn auth_headers_bearer_sets_authorization_and_accept() {
        let headers = auth_headers("Bearer my-token");
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            "Bearer my-token"
        );
        assert_eq!(
            headers.get(ACCEPT).unwrap().to_str().unwrap(),
            "application/json"
        );
    }

    #[test]
    fn auth_headers_basic_sets_authorization_and_accept() {
        let headers = auth_headers("Basic dXNlcjpwYXNz");
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            "Basic dXNlcjpwYXNz"
        );
        assert_eq!(
            headers.get(ACCEPT).unwrap().to_str().unwrap(),
            "application/json"
        );
    }

    #[test]
    fn binary_auth_headers_bearer_sets_authorization_without_accept() {
        let headers = binary_auth_headers("Bearer my-token");
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            "Bearer my-token"
        );
        assert!(
            headers.get(ACCEPT).is_none(),
            "binary_auth_headers must not set Accept header"
        );
    }

    #[test]
    fn binary_auth_headers_basic_sets_authorization_without_accept() {
        let headers = binary_auth_headers("Basic dXNlcjpwYXNz");
        assert_eq!(
            headers.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            "Basic dXNlcjpwYXNz"
        );
        assert!(
            headers.get(ACCEPT).is_none(),
            "binary_auth_headers must not set Accept header"
        );
    }

    // ── resolve_auth() unit tests ──────────────────────────────────────
    //
    // Feature: cloud-api-token-auth
    // Validates: Requirements 1.3, 1.4, 2.1, 2.2, 2.3, 2.4, 2.5, 4.1, 4.3, 7.2, 7.3
    //
    // Environment variables are process-global, so these tests must not run
    // concurrently.  We use a static Mutex to serialize access.

    use std::sync::Mutex;

    static AUTH_ENV_LOCK: Mutex<()> = Mutex::new(());

    const PAT_VAR: &str = "CONFLUENCE2MD_PERSONAL_ACCESS_TOKEN";
    const USER_VAR: &str = "CONFLUENCE2MD_USERNAME";
    const TOKEN_VAR: &str = "CONFLUENCE2MD_API_TOKEN";

    /// Helper: clear all auth env vars, returning the lock guard.
    ///
    /// # Safety
    /// Env var manipulation is unsafe in Rust 2024 edition because it is not
    /// thread-safe. We serialize access via AUTH_ENV_LOCK so concurrent
    /// modification cannot occur within our test suite.
    fn clear_auth_env() -> std::sync::MutexGuard<'static, ()> {
        let guard = AUTH_ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by AUTH_ENV_LOCK — no concurrent env mutation.
        unsafe {
            std::env::remove_var(PAT_VAR);
            std::env::remove_var(USER_VAR);
            std::env::remove_var(TOKEN_VAR);
        }
        guard
    }

    #[test]
    fn resolve_auth_pat_set_alone_returns_bearer() {
        let _guard = clear_auth_env();
        // SAFETY: guarded by AUTH_ENV_LOCK.
        unsafe { std::env::set_var(PAT_VAR, "my-pat-value") };

        let result = resolve_auth().unwrap();
        assert_eq!(result, "Bearer my-pat-value");
    }

    #[test]
    fn resolve_auth_cloud_vars_set_no_pat_returns_basic() {
        let _guard = clear_auth_env();
        // SAFETY: guarded by AUTH_ENV_LOCK.
        unsafe {
            std::env::set_var(USER_VAR, "user@example.com");
            std::env::set_var(TOKEN_VAR, "my-api-token");
        }

        let result = resolve_auth().unwrap();
        // base64("user@example.com:my-api-token") = "dXNlckBleGFtcGxlLmNvbTpteS1hcGktdG9rZW4="
        assert_eq!(result, "Basic dXNlckBleGFtcGxlLmNvbTpteS1hcGktdG9rZW4=");
    }

    #[test]
    fn resolve_auth_both_cloud_and_pat_set_returns_bearer_priority() {
        let _guard = clear_auth_env();
        // SAFETY: guarded by AUTH_ENV_LOCK.
        unsafe {
            std::env::set_var(PAT_VAR, "my-pat");
            std::env::set_var(USER_VAR, "user@example.com");
            std::env::set_var(TOKEN_VAR, "my-api-token");
        }

        let result = resolve_auth().unwrap();
        assert_eq!(result, "Bearer my-pat");
    }

    #[test]
    fn resolve_auth_no_vars_set_returns_error_listing_both_options() {
        let _guard = clear_auth_env();

        let err = resolve_auth().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("CONFLUENCE2MD_PERSONAL_ACCESS_TOKEN"),
            "error should mention PAT var: {msg}"
        );
        assert!(
            msg.contains("CONFLUENCE2MD_USERNAME") || msg.contains("CONFLUENCE2MD_API_TOKEN"),
            "error should mention Cloud vars: {msg}"
        );
    }

    #[test]
    fn resolve_auth_only_username_set_errors_naming_missing_api_token() {
        let _guard = clear_auth_env();
        // SAFETY: guarded by AUTH_ENV_LOCK.
        unsafe { std::env::set_var(USER_VAR, "user@example.com") };

        let err = resolve_auth().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("CONFLUENCE2MD_API_TOKEN"),
            "error should name missing API token var: {msg}"
        );
    }

    #[test]
    fn resolve_auth_only_api_token_set_errors_naming_missing_username() {
        let _guard = clear_auth_env();
        // SAFETY: guarded by AUTH_ENV_LOCK.
        unsafe { std::env::set_var(TOKEN_VAR, "my-api-token") };

        let err = resolve_auth().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("CONFLUENCE2MD_USERNAME"),
            "error should name missing username var: {msg}"
        );
    }

    #[test]
    fn resolve_auth_pat_whitespace_only_returns_error() {
        let _guard = clear_auth_env();
        // SAFETY: guarded by AUTH_ENV_LOCK.
        unsafe { std::env::set_var(PAT_VAR, "   \t  ") };

        let err = resolve_auth().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("whitespace") || msg.contains("empty"),
            "error should mention whitespace/empty PAT: {msg}"
        );
    }

    #[test]
    fn resolve_auth_username_whitespace_only_with_valid_api_token_returns_error() {
        let _guard = clear_auth_env();
        // SAFETY: guarded by AUTH_ENV_LOCK.
        unsafe {
            std::env::set_var(USER_VAR, "   ");
            std::env::set_var(TOKEN_VAR, "valid-token");
        }

        let err = resolve_auth().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("USERNAME") || msg.contains("username"),
            "error should mention username issue: {msg}"
        );
    }

    #[test]
    fn resolve_auth_api_token_whitespace_only_with_valid_username_returns_error() {
        let _guard = clear_auth_env();
        // SAFETY: guarded by AUTH_ENV_LOCK.
        unsafe {
            std::env::set_var(USER_VAR, "user@example.com");
            std::env::set_var(TOKEN_VAR, "   \t");
        }

        let err = resolve_auth().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("API_TOKEN") || msg.contains("api_token") || msg.contains("API token"),
            "error should mention API token issue: {msg}"
        );
    }

    // Feature: cloud-api-token-auth, Property 2: PAT priority and Bearer header format
    // **Validates: Requirements 2.1, 2.4, 4.1, 4.2**
    #[test]
    fn resolve_auth_pat_priority_and_bearer_format_programmatic() {
        let long_pat = "a".repeat(100);
        let pat_values_owned: Vec<&str> = vec![
            "x",
            &long_pat,
            "abc!@#$%^&*()_+-=[]{}",
            "tökën-pät",
            "token with spaces",
        ];

        for pat_val in &pat_values_owned {
            // Case A: PAT set alone (no cloud vars)
            {
                let _guard = clear_auth_env();
                // SAFETY: guarded by AUTH_ENV_LOCK.
                unsafe {
                    std::env::set_var(PAT_VAR, pat_val);
                }

                let result = resolve_auth();
                assert!(
                    result.is_ok(),
                    "PAT alone should succeed for pat={pat_val:?}, got err: {:?}",
                    result.unwrap_err()
                );
                let header = result.unwrap();
                assert!(
                    header.starts_with("Bearer "),
                    "Header must start with 'Bearer ' for pat={pat_val:?}, got: {header:?}"
                );
                let remainder = &header["Bearer ".len()..];
                assert_eq!(
                    remainder, *pat_val,
                    "Remainder after 'Bearer ' must equal exact PAT value for pat={pat_val:?}"
                );
            }

            // Case B: PAT set WITH cloud vars also set (PAT takes priority)
            {
                let _guard = clear_auth_env();
                // SAFETY: guarded by AUTH_ENV_LOCK.
                unsafe {
                    std::env::set_var(PAT_VAR, pat_val);
                    std::env::set_var(USER_VAR, "user@example.com");
                    std::env::set_var(TOKEN_VAR, "token123");
                }

                let result = resolve_auth();
                assert!(
                    result.is_ok(),
                    "PAT with cloud vars should succeed for pat={pat_val:?}, got err: {:?}",
                    result.unwrap_err()
                );
                let header = result.unwrap();
                assert!(
                    header.starts_with("Bearer "),
                    "Header must start with 'Bearer ' when PAT has priority for pat={pat_val:?}, got: {header:?}"
                );
                let remainder = &header["Bearer ".len()..];
                assert_eq!(
                    remainder, *pat_val,
                    "Remainder after 'Bearer ' must equal exact PAT value (priority over cloud) for pat={pat_val:?}"
                );
            }
        }
    }

    // Feature: cloud-api-token-auth, Property 3: Cloud Basic auth header format
    // **Validates: Requirements 1.5, 2.2, 3.1, 3.4**
    //
    // For any pair of non-empty, non-whitespace-only values for username and
    // api_token, when PAT is not set, resolve_auth() returns "Basic <encoded>"
    // where <encoded> == base64_encode("{username}:{api_token}".as_bytes()).
    //
    // This test loops over ASCII and non-ASCII username/token pairs, including
    // colons in the token (edge case for the separator), and verifies:
    // - Result is Ok
    // - Result starts with "Basic "
    // - The base64 portion only contains valid base64 chars [A-Za-z0-9+/=]
    // - The base64 portion matches base64_encode("{username}:{api_token}".as_bytes())
    #[test]
    fn resolve_auth_basic_header_format_property_coverage() {
        let long_user = "a".repeat(50) + "@example.com";
        let long_token = "b".repeat(100);

        let pairs: Vec<(&str, &str)> = vec![
            // Normal ASCII
            ("user@example.com", "simple-token"),
            // Unicode username
            ("üser@example.com", "token123"),
            // Unicode token
            ("user@example.com", "tökën-123"),
            // Both unicode
            ("ü@ëx.com", "tök:ën"),
            // Colon in token (edge case for the separator)
            ("user@example.com", "token:with:colons"),
            // Special chars
            ("user+tag@example.com", "abc!@#$%^&*()"),
        ];

        // Also include long values (owned strings)
        let owned_pairs: Vec<(String, String)> = vec![(long_user.clone(), long_token.clone())];

        // Combine into a single iteration
        let all_pairs: Vec<(&str, &str)> = pairs
            .iter()
            .copied()
            .chain(owned_pairs.iter().map(|(u, t)| (u.as_str(), t.as_str())))
            .collect();

        for (username, api_token) in &all_pairs {
            let _guard = clear_auth_env();
            // SAFETY: guarded by AUTH_ENV_LOCK.
            unsafe {
                std::env::set_var(USER_VAR, username);
                std::env::set_var(TOKEN_VAR, api_token);
            }

            let result = resolve_auth();
            assert!(
                result.is_ok(),
                "resolve_auth() should succeed for ({username:?}, {api_token:?}), got: {:?}",
                result.err()
            );

            let header_value = result.unwrap();

            // Must start with "Basic "
            assert!(
                header_value.starts_with("Basic "),
                "Header for ({username:?}, {api_token:?}) should start with 'Basic ', got: {header_value}"
            );

            // Extract the base64 portion
            let b64_portion = &header_value["Basic ".len()..];

            // Verify base64 alphabet: only [A-Za-z0-9+/=]
            assert!(
                b64_portion
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='),
                "Base64 portion for ({username:?}, {api_token:?}) contains invalid chars: {b64_portion}"
            );

            // Verify correctness: re-encode expected input and compare
            let expected_input = format!("{username}:{api_token}");
            let expected_b64 = base64_encode(expected_input.as_bytes());
            assert_eq!(
                b64_portion, expected_b64,
                "Base64 mismatch for ({username:?}, {api_token:?}): got {b64_portion}, expected {expected_b64}"
            );
        }
    }
}
