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
pub struct EnvConfig {
    pub personal_access_token: String,
}

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

// ── Environment ────────────────────────────────────────────────────

pub fn get_required_env() -> Result<EnvConfig> {
    let token = std::env::var("CONFLUENCE2MD_PERSONAL_ACCESS_TOKEN").map_err(|_| {
        anyhow!("Missing environment variable: CONFLUENCE2MD_PERSONAL_ACCESS_TOKEN.")
    })?;
    Ok(EnvConfig {
        personal_access_token: token,
    })
}

// ── HTTP helpers ───────────────────────────────────────────────────

fn auth_headers(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
        headers.insert(AUTHORIZATION, v);
    }
    headers
}

fn binary_auth_headers(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
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
        Lazy::new(|| Regex::new(r#"(?is)(<img\b[^>]*\bsrc=)(["'])(.*?)(["'])"#).unwrap());

    let result = REPLACE_RE.replace_all(html, |caps: &regex::Captures<'_>| {
        let prefix = &caps[1];
        let quote_open = &caps[2];
        let src = &caps[3];
        let quote_close = &caps[4];
        if quote_open != quote_close {
            return caps[0].to_owned();
        }
        match src_to_local.get(src) {
            Some(local) => format!("{prefix}{quote_open}{local}{quote_close}"),
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
}
