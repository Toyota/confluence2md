//! PlantUML extraction and fallback resolution.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use tracing::{debug, warn};

use crate::confluence::{
    Attachment, DownloadAttachmentOptions, attachment_download_url, download_attachment_to_asset,
};
use crate::drawio::{FallbackDiagram, RewrittenHtml, append_fallback_diagrams_section};
use crate::utils::{escape_html, extract_macro_blocks, extract_macro_param};

pub struct ResolvePlantUmlOptions<'a> {
    pub page_id: &'a str,
    pub storage_html: Option<&'a str>,
    pub html: &'a str,
    pub attachments_by_title: &'a std::collections::HashMap<String, Attachment>,
    pub base_url: &'a str,
    pub token: &'a str,
    pub assets_abs_dir: &'a Path,
    pub markdown_image_prefix: &'a str,
    pub used_names: &'a mut HashSet<String>,
}

pub fn extract_plantuml_export_files(storage_html: Option<&str>) -> Vec<String> {
    let html = storage_html.unwrap_or_default();
    let mut files = Vec::new();
    static EXT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\.[A-Za-z0-9]+$").unwrap());
    for block in extract_macro_blocks(html, "plantuml") {
        let Some(export_name) = extract_macro_param(&block, "exportName") else {
            continue;
        };
        let format = extract_macro_param(&block, "format")
            .map(|f| f.to_ascii_lowercase())
            .unwrap_or_else(|| "png".to_owned());
        if EXT_RE.is_match(&export_name) {
            files.push(export_name);
        } else {
            let ext = if format == "svg" { "svg" } else { "png" };
            files.push(format!("{export_name}.{ext}"));
        }
    }
    files
}

pub fn extract_plantuml_sources(storage_html: Option<&str>) -> Vec<String> {
    let html = storage_html.unwrap_or_default();
    let mut sources = Vec::new();
    static CDATA_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?is)<ac:plain-text-body>\s*<!\[CDATA\[(.*?)\]\]>\s*</ac:plain-text-body>"#)
            .unwrap()
    });
    for block in extract_macro_blocks(html, "plantuml") {
        if let Some(c) = CDATA_RE.captures(&block)
            && let Some(m) = c.get(1)
        {
            sources.push(m.as_str().to_owned());
        }
    }
    sources
}

pub fn replace_plantuml_imgs_with_code(html: &str, sources: &[String]) -> String {
    if sources.is_empty() {
        return html.to_owned();
    }
    static RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?is)<img\b[^>]*\bsrc=['"][^'"]*/rest/plantuml/[^'"]*['"][^>]*/?>"#).unwrap()
    });
    let mut index = 0usize;
    RE.replace_all(html, |_caps: &regex::Captures<'_>| {
        let Some(src) = sources.get(index) else {
            return String::new();
        };
        index += 1;
        format!(
            r#"<pre><code class="language-plantuml">{}</code></pre>"#,
            escape_html(src)
        )
    })
    .into_owned()
}

// ── PlantUML !include attachment downloads ─────────────────────────

pub struct DownloadIncludesOptions<'a> {
    pub page_id: &'a str,
    pub attachments_by_title: &'a std::collections::HashMap<String, Attachment>,
    pub base_url: &'a str,
    pub token: &'a str,
    pub assets_abs_dir: &'a Path,
    pub markdown_image_prefix: &'a str,
}

fn binary_auth_headers(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
        headers.insert(AUTHORIZATION, v);
    }
    headers
}

pub async fn download_plantuml_includes(
    client: &Client,
    sources: &[String],
    opts: &DownloadIncludesOptions<'_>,
) -> Result<Vec<String>> {
    static INCLUDE_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?m)^(\s*!include\s+)\^(.+)$").unwrap());

    let mut files_to_download: HashSet<String> = HashSet::new();
    for src in sources {
        for cap in INCLUDE_RE.captures_iter(src) {
            files_to_download.insert(cap[2].trim().to_owned());
        }
    }

    for file_name in &files_to_download {
        let Some(attachment) = opts.attachments_by_title.get(file_name) else {
            warn!("PlantUML !include attachment not found: {file_name}");
            continue;
        };
        let url = attachment_download_url(opts.base_url, opts.page_id, attachment);
        let result: Result<()> = async {
            debug!("Downloading PlantUML include: name: {file_name}");
            let response = client
                .get(&url)
                .headers(binary_auth_headers(opts.token))
                .send()
                .await
                .context("HTTP")?;
            if !response.status().is_success() {
                bail!(
                    "{} {}",
                    response.status().as_u16(),
                    response.status().canonical_reason().unwrap_or("")
                );
            }
            debug!("Downloaded PlantUML include: name: {file_name}");
            let bytes = response.bytes().await?;
            tokio::fs::write(opts.assets_abs_dir.join(file_name), &bytes)
                .await
                .with_context(|| format!("write {file_name}"))?;
            debug!("Saved PlantUML include: file: {file_name}");
            Ok(())
        }
        .await;
        if let Err(e) = result {
            warn!("Failed to download PlantUML include {file_name}: {e}");
        }
    }

    Ok(sources
        .iter()
        .map(|src| {
            INCLUDE_RE
                .replace_all(src, |caps: &regex::Captures<'_>| {
                    let prefix = &caps[1];
                    let file_name = caps[2].trim();
                    format!("{prefix}{}/{file_name}", opts.markdown_image_prefix)
                })
                .into_owned()
        })
        .collect())
}

// ── High-level resolution ──────────────────────────────────────────

/// Resolves PlantUML by the primary path (fenced code blocks) when source is
/// available, otherwise delegates to the fallback path (exported image files).
pub async fn resolve_plantuml_diagrams(
    client: &Client,
    opts: ResolvePlantUmlOptions<'_>,
) -> Result<RewrittenHtml> {
    let sources = extract_plantuml_sources(opts.storage_html);

    if !sources.is_empty() {
        let include_opts = DownloadIncludesOptions {
            page_id: opts.page_id,
            attachments_by_title: opts.attachments_by_title,
            base_url: opts.base_url,
            token: opts.token,
            assets_abs_dir: opts.assets_abs_dir,
            markdown_image_prefix: opts.markdown_image_prefix,
        };
        let rewritten_sources = download_plantuml_includes(client, &sources, &include_opts).await?;
        let rewritten_html = replace_plantuml_imgs_with_code(opts.html, &rewritten_sources);
        return Ok(RewrittenHtml(rewritten_html));
    }

    resolve_plantuml_fallbacks(client, opts).await
}

/// Fallback path only: PlantUML source is unavailable; downloads exported image attachments and
/// appends them as an image section at the bottom of the HTML.
async fn resolve_plantuml_fallbacks(
    client: &Client,
    opts: ResolvePlantUmlOptions<'_>,
) -> Result<RewrittenHtml> {
    let export_files = extract_plantuml_export_files(opts.storage_html);
    if export_files.is_empty() {
        return Ok(RewrittenHtml(opts.html.to_owned()));
    }

    let mut fallback_diagrams: Vec<FallbackDiagram> = Vec::new();
    let mut existing_refs: HashSet<String> = HashSet::new();

    for file_name in &export_files {
        let Some(attachment) = opts.attachments_by_title.get(file_name) else {
            continue;
        };
        let download_opts = DownloadAttachmentOptions {
            page_id: opts.page_id,
            attachment,
            base_url: opts.base_url,
            token: opts.token,
            assets_abs_dir: opts.assets_abs_dir,
            markdown_image_prefix: opts.markdown_image_prefix,
            used_names: opts.used_names,
        };
        match download_attachment_to_asset(client, download_opts).await {
            Ok(local_path) => {
                if existing_refs.insert(local_path.clone()) {
                    fallback_diagrams.push(FallbackDiagram {
                        local_path,
                        label: format!("PlantUML: {file_name}"),
                        alt: file_name.clone(),
                    });
                }
            }
            Err(_) => {
                warn!("Failed to fetch PlantUML attachment: {file_name}");
            }
        }
    }

    let rewritten_html = append_fallback_diagrams_section(opts.html, &fallback_diagrams);
    Ok(RewrittenHtml(rewritten_html))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_plantuml_sources_empty_for_no_macros() {
        assert!(extract_plantuml_sources(Some("<p>no macros</p>")).is_empty());
        assert!(extract_plantuml_sources(None).is_empty());
    }

    #[test]
    fn replace_plantuml_imgs_replaces_img_tags() {
        let html = r#"<p>Before</p><img class="confluence-embedded-image" src='https://example.com/rest/plantuml/1.0/abc?type=image%2Fpng' /><p>After</p>"#;
        let sources = vec!["@startuml\nAlice -> Bob\n@enduml".to_owned()];
        let result = replace_plantuml_imgs_with_code(html, &sources);
        assert!(!result.contains("/rest/plantuml/"));
        assert!(result.contains("language-plantuml"));
        assert!(result.contains("@startuml"));
    }

    #[test]
    fn replace_plantuml_imgs_leaves_other_images() {
        let html = r#"<img src="https://example.com/image.png" /><img src='https://example.com/rest/plantuml/1.0/abc' />"#;
        let sources = vec!["@startuml\ntest\n@enduml".to_owned()];
        let result = replace_plantuml_imgs_with_code(html, &sources);
        assert!(result.contains(r#"src="https://example.com/image.png""#));
    }

    #[test]
    fn extract_plantuml_sources_does_not_pick_up_preceding_code_macro() {
        let html = r#"<ac:structured-macro ac:name="code"><ac:plain-text-body><![CDATA[int x = 1;]]></ac:plain-text-body></ac:structured-macro>
<ac:structured-macro ac:name="plantuml"><ac:plain-text-body><![CDATA[@startuml
A -> B
@enduml]]></ac:plain-text-body></ac:structured-macro>"#;
        let sources = extract_plantuml_sources(Some(html));
        assert_eq!(sources.len(), 1);
        assert!(sources[0].contains("@startuml"));
        assert!(!sources[0].contains("int x"));
    }
}
