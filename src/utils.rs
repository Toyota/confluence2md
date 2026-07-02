//! Pure helper functions: filename sanitisation, HTML-entity decoding, URL
//! resolution, and Confluence-storage-format macro preprocessing.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::jira::replace_jira_macros;

use once_cell::sync::Lazy;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use regex::Regex;
use url::Url;

// ── Filename sanitisation ──────────────────────────────────────────

static FORBIDDEN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"[\\/:*?"<>|]"#).unwrap());
static WHITESPACE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());
static UNDERSCORE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"_+").unwrap());

/// Make a string safe to use as a filename on common filesystems.
///
/// * forbidden chars `\/:*?"<>|` are replaced with `_`
/// * runs of whitespace and underscores collapse to a single `_`
/// * leading/trailing `_` are stripped
/// * the result is truncated to 180 bytes — wait, the original truncates to
///   180 *code units*. We replicate the same logic on characters so that
///   non-ASCII titles keep the same behaviour.
/// * if the result is empty, `"file"` is returned
pub fn sanitize_file_name(name: &str) -> String {
    let s = FORBIDDEN_RE.replace_all(name, "_").into_owned();
    let s = WHITESPACE_RE.replace_all(&s, "_").into_owned();
    let s = UNDERSCORE_RE.replace_all(&s, "_").into_owned();
    let s = s.trim_matches('_').to_string();
    // The TS implementation uses `.slice(0, 180)`, which counts UTF-16 code
    // units. For our purposes — and to remain memory-safe — truncate to at
    // most 180 Unicode scalar values.
    let truncated: String = s.chars().take(180).collect();
    if truncated.is_empty() {
        "file".to_owned()
    } else {
        truncated
    }
}

// ── HTML entity decoding ───────────────────────────────────────────

/// Decode the small set of entities used in HTML attributes:
/// `&amp; &quot; &#39; &lt; &gt;`.
pub fn decode_html_attribute(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

static NUMERIC_HEX_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)&#x([0-9a-f]+);").unwrap());
static NUMERIC_DEC_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"&#([0-9]+);").unwrap());

/// Decode the same entities as [`decode_html_attribute`], plus numeric
/// character references (`&#x41;`, `&#65;`).
pub fn decode_html_text(value: &str) -> String {
    let s = decode_html_attribute(value);
    let s = NUMERIC_HEX_RE.replace_all(&s, |c: &regex::Captures<'_>| {
        u32::from_str_radix(&c[1], 16)
            .ok()
            .and_then(char::from_u32)
            .map(|ch| ch.to_string())
            .unwrap_or_default()
    });
    let s = NUMERIC_DEC_RE.replace_all(&s, |c: &regex::Captures<'_>| {
        c[1].parse::<u32>()
            .ok()
            .and_then(char::from_u32)
            .map(|ch| ch.to_string())
            .unwrap_or_default()
    });
    s.into_owned()
}

static TAG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]+>").unwrap());

/// Remove HTML tags and decode entities, then trim.
pub fn strip_tags(html: &str) -> String {
    decode_html_text(&TAG_RE.replace_all(html, ""))
        .trim()
        .to_owned()
}

// ── URL helpers ────────────────────────────────────────────────────

/// Strip trailing slashes from a base URL.
pub fn normalize_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    trimmed.to_owned()
}

/// Resolve a possibly-relative URL against the configured Confluence base URL,
/// taking the base's context path into account.
pub fn resolve_url(src: &str, base_url: &str) -> String {
    let decoded = decode_html_attribute(src);
    if let Ok(u) = Url::parse(&decoded) {
        return u.to_string();
    }

    let base = match Url::parse(base_url) {
        Ok(u) => u,
        Err(_) => return decoded,
    };
    let origin = format!(
        "{}://{}",
        base.scheme(),
        base.host_str()
            .map(|h| {
                if let Some(port) = base.port() {
                    format!("{h}:{port}")
                } else {
                    h.to_owned()
                }
            })
            .unwrap_or_default()
    );
    let context_path = base.path().trim_end_matches('/');

    if let Some(rest) = decoded.strip_prefix('/') {
        let path = format!("/{rest}");
        if !context_path.is_empty() && !path.starts_with(&format!("{context_path}/")) {
            return format!("{origin}{context_path}{path}");
        }
        return format!("{origin}{path}");
    }

    // Relative URL: join against `base_url + "/"`.
    let trailing = if base_url.ends_with('/') {
        base_url.to_owned()
    } else {
        format!("{base_url}/")
    };
    match Url::parse(&trailing).and_then(|b| b.join(&decoded)) {
        Ok(u) => u.to_string(),
        Err(_) => decoded,
    }
}

/// Map a `Content-Type` header to a file extension (or empty string).
pub fn ext_from_content_type(content_type: &str) -> &'static str {
    let primary = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match primary.as_str() {
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/svg+xml" => ".svg",
        "image/bmp" => ".bmp",
        "image/x-icon" | "image/vnd.microsoft.icon" => ".ico",
        "image/tiff" => ".tif",
        _ => "",
    }
}

/// Escape `& < > "` for inclusion in HTML attributes / text.
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

// ── Filename derivation ────────────────────────────────────────────

static CD_FILENAME_STAR_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)filename\*=UTF-8''([^;]+)").unwrap());
static CD_FILENAME_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?i)filename="?([^"]+)"?"#).unwrap());

/// Headers needed to derive a filename for a downloaded asset.
#[derive(Debug, Default, Clone)]
pub struct HeaderHints<'a> {
    pub content_disposition: Option<&'a str>,
    pub content_type: Option<&'a str>,
}

/// Extract filename from URL or HTTP headers.
pub fn get_file_name_from_url_or_headers(
    url: &str,
    headers: &HeaderHints<'_>,
    fallback_base_name: &str,
    used_names: &mut HashSet<String>,
) -> String {
    let mut file_name: Option<String> = None;

    if let Some(cd) = headers.content_disposition {
        if let Some(cap) = CD_FILENAME_STAR_RE.captures(cd) {
            file_name = Some(
                percent_encoding::percent_decode_str(&cap[1])
                    .decode_utf8_lossy()
                    .into_owned(),
            );
        } else if let Some(cap) = CD_FILENAME_RE.captures(cd) {
            file_name = Some(cap[1].to_owned());
        }
    }

    if file_name.is_none()
        && let Ok(u) = Url::parse(url)
    {
        let base = u
            .path_segments()
            .and_then(|mut s| s.next_back())
            .map(str::to_owned)
            .unwrap_or_default();
        if !base.is_empty() && base != "/" && base != "." {
            file_name = Some(
                percent_encoding::percent_decode_str(&base)
                    .decode_utf8_lossy()
                    .into_owned(),
            );
        }
    }

    let mut name = file_name.unwrap_or_else(|| fallback_base_name.to_owned());

    if Path::new(&name).extension().is_none()
        && let Some(ct) = headers.content_type
    {
        let inferred = ext_from_content_type(ct);
        if !inferred.is_empty() {
            name.push_str(inferred);
        }
    }

    let sanitised = sanitize_file_name(&name);
    if !used_names.contains(&sanitised) {
        used_names.insert(sanitised.clone());
        return sanitised;
    }

    let path = Path::new(&sanitised);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    let mut index = 1u32;
    loop {
        let candidate = format!("{stem}_{index}{ext}");
        if !used_names.contains(&candidate) {
            used_names.insert(candidate.clone());
            return candidate;
        }
        index += 1;
    }
}

// ── Assets layout ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AssetsInfo {
    pub assets_dir_name: String,
    pub assets_abs_dir: PathBuf,
    pub markdown_image_prefix: String,
}

pub fn make_assets_info(page_id: &str, title: &str, output_path: &Path) -> AssetsInfo {
    let title_owned;
    let title_ref: &str = if title.is_empty() {
        title_owned = format!("page-{page_id}");
        &title_owned
    } else {
        title
    };
    let safe_base = sanitize_file_name(title_ref);
    let assets_dir_name = format!("{safe_base}_assets");

    let output_abs_path = if output_path.is_absolute() {
        output_path.to_owned()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(output_path))
            .unwrap_or_else(|_| output_path.to_owned())
    };
    let output_dir = output_abs_path
        .parent()
        .map(Path::to_owned)
        .unwrap_or_else(|| PathBuf::from("."));
    let assets_abs_dir = output_dir.join(&assets_dir_name);

    // Forward slashes — the prefix is used inside markdown image paths.
    let markdown_image_prefix = assets_dir_name.replace('\\', "/");

    AssetsInfo {
        assets_dir_name,
        assets_abs_dir,
        markdown_image_prefix,
    }
}

// `encodeURIComponent` percent-encodes everything that is not in the unreserved
// set `A-Z a-z 0-9 - _ . ! ~ * ' ( )`. Construct the inverse `AsciiSet`.
pub const URI_COMPONENT: AsciiSet = NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

/// `encodeURIComponent(`prefix/fileName`)`-style escaped path used inside
/// Markdown image references.
pub fn to_markdown_asset_path(markdown_image_prefix: &str, file_name: &str) -> String {
    let rel_path = format!("{}/{}", markdown_image_prefix, file_name).replace('\\', "/");
    utf8_percent_encode(&rel_path, &URI_COMPONENT).to_string()
}

pub async fn ensure_dir(dir: impl AsRef<Path>) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dir).await
}

// ── Macro extraction ───────────────────────────────────────────────

/// Extract every `<ac:structured-macro ac:name="$name">…</ac:structured-macro>`
/// block from the given storage HTML.
///
/// `[^>]*` (instead of `.*?`) is used for the attribute portion so a malformed
/// opening tag never spans across a preceding macro's closing tag.
pub fn extract_macro_blocks(storage_html: &str, macro_name: &str) -> Vec<String> {
    let pattern = format!(
        r#"(?is)<ac:structured-macro\b[^>]*\bac:name="{name}"[^>]*>.*?</ac:structured-macro>"#,
        name = regex::escape(macro_name),
    );
    let re = match Regex::new(&pattern) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    re.find_iter(storage_html)
        .map(|m| m.as_str().to_owned())
        .collect()
}

pub fn extract_macro_param(macro_block: &str, param_name: &str) -> Option<String> {
    let pattern = format!(
        r#"(?is)<ac:parameter\b[^>]*ac:name="{name}"[^>]*>(.*?)</ac:parameter>"#,
        name = regex::escape(param_name),
    );
    let re = Regex::new(&pattern).ok()?;
    let caps = re.captures(macro_block)?;
    Some(strip_tags(caps.get(1)?.as_str()))
}

// ── Confluence-storage-format macro preprocessing ──────────────────

fn replace_code_macros(html: &str) -> String {
    static RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?is)<ac:structured-macro\b[^>]*\bac:name="code".*?</ac:structured-macro>"#)
            .unwrap()
    });
    static LANG_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?is)<ac:parameter\b[^>]*\bac:name="language"[^>]*>(.*?)</ac:parameter>"#)
            .unwrap()
    });
    static BODY_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?is)<ac:plain-text-body>\s*<!\[CDATA\[(.*?)\]\]>\s*</ac:plain-text-body>"#)
            .unwrap()
    });

    RE.replace_all(html, |caps: &regex::Captures<'_>| {
        let m = &caps[0];
        let lang = LANG_RE
            .captures(m)
            .and_then(|c| c.get(1))
            .map(|g| strip_tags(g.as_str()))
            .unwrap_or_default();
        let code = BODY_RE
            .captures(m)
            .and_then(|c| c.get(1))
            .map(|g| g.as_str().trim().to_owned())
            .unwrap_or_default();

        let class_attr = if lang.is_empty() {
            String::new()
        } else {
            format!(r#" class="language-{}""#, escape_html(&lang))
        };
        format!("<pre><code{class_attr}>{}</code></pre>", escape_html(&code))
    })
    .into_owned()
}

/// Replace Confluence `expand` macros with `<details>`/`<summary>` HTML.
///
/// Implementation walks the input nesting-aware so that nested
/// `<ac:structured-macro>` tags inside the body (e.g. `jira`, `drawio`) are
/// matched correctly to their own closing tag.
fn replace_expand_macros(html: &str) -> String {
    static EXPAND_NAME_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?i)\bac:name="expand""#).unwrap());
    static TITLE_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?is)<ac:parameter\b[^>]*ac:name="title"[^>]*>(.*?)</ac:parameter>"#).unwrap()
    });
    const OPEN: &str = "<ac:structured-macro";
    const CLOSE: &str = "</ac:structured-macro>";
    const BODY_OPEN: &str = "<ac:rich-text-body>";
    const BODY_CLOSE: &str = "</ac:rich-text-body>";

    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut pos = 0usize;

    while pos < html.len() {
        let macro_start = match html[pos..].find(OPEN) {
            Some(offset) => pos + offset,
            None => {
                out.push_str(&html[pos..]);
                break;
            }
        };

        // Find end of opening tag, respecting quoted attribute values.
        let mut tag_end = macro_start + OPEN.len();
        let mut in_str = false;
        while tag_end < bytes.len() && (in_str || bytes[tag_end] != b'>') {
            if bytes[tag_end] == b'"' {
                in_str = !in_str;
            }
            tag_end += 1;
        }
        if tag_end >= bytes.len() {
            out.push_str(&html[pos..]);
            break;
        }
        let open_text = &html[macro_start..=tag_end];
        let self_closing = bytes[tag_end - 1] == b'/';

        if !EXPAND_NAME_RE.is_match(open_text) {
            out.push_str(&html[pos..=tag_end]);
            pos = tag_end + 1;
            continue;
        }

        out.push_str(&html[pos..macro_start]);

        if self_closing {
            out.push_str("<details><summary></summary></details>");
            pos = tag_end + 1;
            continue;
        }

        // Walk forward counting nesting depth.
        let mut depth = 1u32;
        let mut search_pos = tag_end + 1;
        let mut macro_end: Option<usize> = None;
        while depth > 0 {
            let next_open = html[search_pos..].find(OPEN).map(|o| search_pos + o);
            let next_close = match html[search_pos..].find(CLOSE) {
                Some(o) => search_pos + o,
                None => break,
            };
            if let Some(no) = next_open
                && no < next_close
            {
                let mut inner_end = no + OPEN.len();
                let mut inner_in_str = false;
                while inner_end < bytes.len() && (inner_in_str || bytes[inner_end] != b'>') {
                    if bytes[inner_end] == b'"' {
                        inner_in_str = !inner_in_str;
                    }
                    inner_end += 1;
                }
                if inner_end < bytes.len() && bytes[inner_end - 1] != b'/' {
                    depth += 1;
                }
                search_pos = inner_end + 1;
                continue;
            }
            depth -= 1;
            if depth == 0 {
                macro_end = Some(next_close + CLOSE.len());
            }
            search_pos = next_close + CLOSE.len();
        }

        let macro_end = match macro_end {
            Some(e) => e,
            None => {
                out.push_str(open_text);
                pos = tag_end + 1;
                continue;
            }
        };

        let full_macro = &html[macro_start..macro_end];
        let title = TITLE_RE
            .captures(full_macro)
            .and_then(|c| c.get(1))
            .map(|g| strip_tags(g.as_str()))
            .unwrap_or_default();

        let body = match (full_macro.find(BODY_OPEN), full_macro.rfind(BODY_CLOSE)) {
            (Some(bs), Some(be)) if be > bs => &full_macro[bs + BODY_OPEN.len()..be],
            _ => "",
        };

        out.push_str(&format!(
            "<details><summary>{title}</summary>{body}</details>"
        ));
        pos = macro_end;
    }

    out
}

const LREF_GDRIVE_LINK_TEXT: &str = "Google Drive Link";

fn is_inside_anchor(full: &str, offset: usize) -> bool {
    let before = &full[..offset];
    let last_open = before.rfind("<a ");
    let last_close = before.rfind("</a>");
    matches!((last_open, last_close), (Some(o), c) if c.is_none_or(|cc| o > cc))
}

fn replace_lref_gdrive_macros(html: &str) -> String {
    static STORAGE_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r#"(?is)<ac:structured-macro\b[^>]*\bac:name="lref-gdrive-file".*?</ac:structured-macro>"#,
        )
        .unwrap()
    });
    static RENDERED_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r#"(?is)(?:<span\b[^>]*\bclass="[^"]*\bap-container\b[^"]*"[^>]*>\s*)?<span\b[^>]*\bdata-module-key="lref-gdrive-file"[^>]*>\s*</span>(?:\s*</span>)?"#,
        )
        .unwrap()
    });
    static DATA_CTX_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?i)data-context="([^"]*)""#).unwrap());

    let after_storage = {
        let mut out = String::with_capacity(html.len());
        let mut last = 0usize;
        for m in STORAGE_RE.find_iter(html) {
            out.push_str(&html[last..m.start()]);
            let url = extract_macro_param(m.as_str(), "url").unwrap_or_default();
            if url.is_empty() {
                // skip — emit nothing
            } else if is_inside_anchor(html, m.start()) {
                out.push_str(LREF_GDRIVE_LINK_TEXT);
            } else {
                out.push_str(&format!(
                    r#"<a href="{}">{}</a>"#,
                    escape_html(&url),
                    LREF_GDRIVE_LINK_TEXT
                ));
            }
            last = m.end();
        }
        out.push_str(&html[last..]);
        out
    };

    let mut out = String::with_capacity(after_storage.len());
    let mut last = 0usize;
    for m in RENDERED_RE.find_iter(&after_storage) {
        out.push_str(&after_storage[last..m.start()]);
        let chunk = m.as_str();
        let mut url = String::new();
        if let Some(cap) = DATA_CTX_RE.captures(chunk) {
            let decoded = decode_html_attribute(&cap[1]);
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&decoded)
                && let Some(u) = v.get("url").and_then(|x| x.as_str())
            {
                url = u.to_owned();
            }
        }
        if is_inside_anchor(&after_storage, m.start()) || url.is_empty() {
            out.push_str(LREF_GDRIVE_LINK_TEXT);
        } else {
            out.push_str(&format!(
                r#"<a href="{}">{}</a>"#,
                escape_html(&url),
                LREF_GDRIVE_LINK_TEXT
            ));
        }
        last = m.end();
    }
    out.push_str(&after_storage[last..]);
    out
}

/// Pre-process Confluence storage-format macros (`code`, `expand`,
/// `jira`, `lref-gdrive-file`, alert macros) into plain HTML elements that the
/// HTML-to-Markdown converter knows how to handle.
pub fn preprocess_confluence_macros(html: &str) -> String {
    let html = replace_code_macros(html);
    let html = replace_expand_macros(&html);
    let html = replace_jira_macros(&html);
    let html = replace_lref_gdrive_macros(&html);
    let html = replace_task_list_items(&html);

    static ALERT_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r#"(?is)<ac:structured-macro\b[^>]*\bac:name="(info|panel|tip|note|warning)".*?</ac:structured-macro>"#,
        )
        .unwrap()
    });
    static BODY_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?is)<ac:rich-text-body>(.*?)</ac:rich-text-body>"#).unwrap());

    ALERT_RE
        .replace_all(&html, |caps: &regex::Captures<'_>| {
            let macro_name = caps[1].to_ascii_lowercase();
            let cls = match macro_name.as_str() {
                "info" => "confluence-information-macro-information",
                "panel" => "confluence-information-macro-panel",
                "tip" => "confluence-information-macro-tip",
                "note" => "confluence-information-macro-note",
                "warning" => "confluence-information-macro-warning",
                _ => "confluence-information-macro-information",
            };
            let body = BODY_RE
                .captures(&caps[0])
                .and_then(|c| c.get(1))
                .map(|g| g.as_str())
                .unwrap_or("");
            format!(
                r#"<div class="confluence-information-macro {cls}"><div class="confluence-information-macro-body">{body}</div></div>"#
            )
        })
        .into_owned()
}

fn extract_task_statuses(storage_html: &str) -> HashMap<String, String> {
    static TASK_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?is)<ac:task>.*?</ac:task>").unwrap());
    static TASK_ID_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?is)<ac:task-id>\s*(.*?)\s*</ac:task-id>").unwrap());
    static TASK_STATUS_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?is)<ac:task-status>\s*(complete|incomplete)\s*</ac:task-status>").unwrap()
    });

    let mut statuses = HashMap::new();
    for task in TASK_RE.find_iter(storage_html) {
        let task_html = task.as_str();
        let Some(id) = TASK_ID_RE
            .captures(task_html)
            .and_then(|caps| caps.get(1))
            .map(|m| strip_tags(m.as_str()))
        else {
            continue;
        };
        let Some(status) = TASK_STATUS_RE
            .captures(task_html)
            .and_then(|caps| caps.get(1))
            .map(|m| m.as_str().to_ascii_lowercase())
        else {
            continue;
        };
        statuses.insert(id, status);
    }
    statuses
}

/// Copy task completion state from Confluence storage HTML onto rendered
/// inline task list items in export-view HTML.
pub fn apply_task_list_statuses(rendered_html: &str, storage_html: &str) -> String {
    let statuses = extract_task_statuses(storage_html);
    if statuses.is_empty() {
        return rendered_html.to_string();
    }

    static LI_OPEN_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?is)<li\b([^>]*)\bdata-inline-task-id="([^"]+)"([^>]*)>"#).unwrap()
    });

    LI_OPEN_RE
        .replace_all(rendered_html, |caps: &regex::Captures<'_>| {
            let original = &caps[0];
            if original.contains("data-task-status=") {
                return original.to_string();
            }
            let id = &caps[2];
            let Some(status) = statuses.get(id) else {
                return original.to_string();
            };
            format!(
                r#"<li{} data-inline-task-id="{}"{} data-task-status="{}">"#,
                &caps[1], id, &caps[3], status
            )
        })
        .into_owned()
}

fn replace_task_list_items(html: &str) -> String {
    static TASK_LI_RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?is)<li\b([^>]*)\bdata-inline-task-id="([^"]+)"([^>]*)>(.*?)</li>"#).unwrap()
    });

    TASK_LI_RE
        .replace_all(html, |caps: &regex::Captures<'_>| {
            let attrs = format!(
                "{} data-inline-task-id=\"{}\"{}",
                caps[1].trim_end(),
                &caps[2],
                &caps[3]
            );
            let marker = if task_item_is_checked(&attrs) {
                "[x]"
            } else {
                "[ ]"
            };
            let body = caps[4].trim();
            let body_is_empty = strip_tags(body).replace("&nbsp;", "").trim().is_empty();
            let content = if body_is_empty {
                marker.to_string()
            } else {
                format!("{marker} {body}")
            };
            format!("<li{attrs}>{content}</li>")
        })
        .into_owned()
}

fn task_item_is_checked(attrs: &str) -> bool {
    static STATUS_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?i)data-task-status\s*=\s*"complete""#).unwrap());
    static CLASS_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?i)class\s*=\s*"([^"]*)""#).unwrap());
    static ARIA_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?i)aria-checked\s*=\s*"true""#).unwrap());

    STATUS_RE.is_match(attrs)
        || ARIA_RE.is_match(attrs)
        || CLASS_RE
            .captures(attrs)
            .and_then(|caps| caps.get(1))
            .is_some_and(|m| {
                m.as_str().split_whitespace().any(|class| {
                    matches!(class.to_ascii_lowercase().as_str(), "checked" | "complete")
                })
            })
}

// `once_cell` for static regex caches.
use once_cell as _;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_file_name_removes_forbidden_chars() {
        assert_eq!(sanitize_file_name(r#"foo/bar\baz:*?"<>|"#), "foo_bar_baz");
    }

    #[test]
    fn sanitize_file_name_collapses_whitespace_and_underscores() {
        assert_eq!(sanitize_file_name("hello   world"), "hello_world");
        assert_eq!(sanitize_file_name("a___b"), "a_b");
    }

    #[test]
    fn sanitize_file_name_strips_outer_underscores() {
        assert_eq!(sanitize_file_name("_foo_"), "foo");
    }

    #[test]
    fn sanitize_file_name_truncates_to_180_chars() {
        let long = "a".repeat(200);
        assert_eq!(sanitize_file_name(&long).chars().count(), 180);
    }

    #[test]
    fn sanitize_file_name_fallback_for_empty_or_symbols_only() {
        assert_eq!(sanitize_file_name(""), "file");
        assert_eq!(sanitize_file_name("???"), "file");
    }

    #[test]
    fn decode_html_attribute_basic_entities() {
        assert_eq!(decode_html_attribute("a &amp; b"), "a & b");
        assert_eq!(decode_html_attribute("&quot;hello&quot;"), "\"hello\"");
        assert_eq!(decode_html_attribute("a &lt; b &gt; c"), "a < b > c");
        assert_eq!(decode_html_attribute("it&#39;s"), "it's");
    }

    #[test]
    fn decode_html_text_numeric_refs() {
        assert_eq!(decode_html_text("&#x41;"), "A");
        assert_eq!(decode_html_text("&#65;"), "A");
        assert_eq!(decode_html_text("&amp;&#x26;"), "&&");
    }

    #[test]
    fn strip_tags_removes_tags_and_decodes() {
        assert_eq!(strip_tags("<p>Hello &amp; World</p>"), "Hello & World");
        assert_eq!(strip_tags("<b><i>text</i></b>"), "text");
        assert_eq!(strip_tags("no tags"), "no tags");
    }

    #[test]
    fn normalize_base_url_strips_trailing_slashes() {
        assert_eq!(
            normalize_base_url("http://example.com/confluence///"),
            "http://example.com/confluence"
        );
        assert_eq!(
            normalize_base_url("http://example.com"),
            "http://example.com"
        );
    }

    #[test]
    fn ext_from_content_type_maps_known_types() {
        assert_eq!(ext_from_content_type("image/png"), ".png");
        assert_eq!(ext_from_content_type("image/jpeg"), ".jpg");
        assert_eq!(ext_from_content_type("image/gif"), ".gif");
        assert_eq!(ext_from_content_type("image/webp"), ".webp");
        assert_eq!(ext_from_content_type("image/svg+xml"), ".svg");
        assert_eq!(ext_from_content_type("application/pdf"), "");
        assert_eq!(ext_from_content_type(""), "");
        assert_eq!(ext_from_content_type("image/png; charset=utf-8"), ".png");
    }

    const BASE: &str = "http://localhost:8080/confluence";

    #[test]
    fn resolve_url_absolute_unchanged() {
        assert_eq!(
            resolve_url("https://example.com/image.png", BASE),
            "https://example.com/image.png"
        );
    }

    #[test]
    fn resolve_url_root_relative_prepends_context_path() {
        assert_eq!(
            resolve_url("/download/attachments/123/foo.png", BASE),
            "http://localhost:8080/confluence/download/attachments/123/foo.png"
        );
    }

    #[test]
    fn resolve_url_relative_joins_against_base() {
        let result = resolve_url("images/foo.png", BASE);
        assert!(
            result.starts_with("http://localhost:8080/confluence/"),
            "got {result}"
        );
    }

    #[test]
    fn resolve_url_decodes_html_encoded_src() {
        let result = resolve_url("/path?a=1&amp;b=2", BASE);
        assert!(result.contains("a=1&b=2"), "got {result}");
    }

    #[test]
    fn preprocess_code_macro_with_language() {
        let html = r#"<ac:structured-macro ac:name="code" ac:schema-version="1" ac:macro-id="abc">
  <ac:parameter ac:name="language">python</ac:parameter><ac:plain-text-body>
    <![CDATA[print("hello")]]>
  </ac:plain-text-body>
</ac:structured-macro>"#;
        let result = preprocess_confluence_macros(html);
        assert!(result.contains(r#"<pre><code class="language-python">"#));
        assert!(result.contains("print(&quot;hello&quot;)"));
        assert!(result.contains("</code></pre>"));
    }

    #[test]
    fn preprocess_code_macro_without_language() {
        let html = r#"<ac:structured-macro ac:name="code"><ac:plain-text-body><![CDATA[echo hi]]></ac:plain-text-body></ac:structured-macro>"#;
        let result = preprocess_confluence_macros(html);
        assert!(result.contains("<pre><code>echo hi</code></pre>"));
    }

    #[test]
    fn preprocess_code_macro_escapes_html_specials() {
        let html = r#"<ac:structured-macro ac:name="code"><ac:plain-text-body><![CDATA[a < b && b > 0]]></ac:plain-text-body></ac:structured-macro>"#;
        let result = preprocess_confluence_macros(html);
        assert!(
            result.contains("a &lt; b &amp;&amp; b &gt; 0"),
            "got {result}"
        );
    }

    #[test]
    fn preprocess_info_macro_to_information_div() {
        let html = r#"<ac:structured-macro ac:name="info" ac:schema-version="1" ac:macro-id="abc">
  <ac:parameter ac:name="title">Title</ac:parameter>
  <ac:rich-text-body><p>Info content.</p></ac:rich-text-body>
</ac:structured-macro>"#;
        let processed = preprocess_confluence_macros(html);
        assert!(processed.contains("confluence-information-macro-information"));
        assert!(processed.contains("Info content."));
        assert!(!processed.contains("ac:structured-macro"));
    }

    #[test]
    fn preprocess_note_macro_to_note_div() {
        let html = r#"<ac:structured-macro ac:name="note"><ac:rich-text-body><p>Note body.</p></ac:rich-text-body></ac:structured-macro>"#;
        let processed = preprocess_confluence_macros(html);
        assert!(processed.contains("confluence-information-macro-note"));
    }

    #[test]
    fn preprocess_tip_macro_to_tip_div() {
        let html = r#"<ac:structured-macro ac:name="tip"><ac:rich-text-body><p>Tip body.</p></ac:rich-text-body></ac:structured-macro>"#;
        let processed = preprocess_confluence_macros(html);
        assert!(processed.contains("confluence-information-macro-tip"));
    }

    #[test]
    fn preprocess_warning_macro_to_warning_div() {
        let html = r#"<ac:structured-macro ac:name="warning"><ac:rich-text-body><p>Warning body.</p></ac:rich-text-body></ac:structured-macro>"#;
        let processed = preprocess_confluence_macros(html);
        assert!(processed.contains("confluence-information-macro-warning"));
    }

    #[test]
    fn preprocess_panel_macro_to_panel_div() {
        let html = r#"<ac:structured-macro ac:name="panel"><ac:rich-text-body><p>Panel body.</p></ac:rich-text-body></ac:structured-macro>"#;
        let processed = preprocess_confluence_macros(html);
        assert!(processed.contains("confluence-information-macro-panel"));
    }

    #[test]
    fn preprocess_does_not_touch_non_alert_macros() {
        let html = r#"<ac:structured-macro ac:name="drawio"><ac:parameter ac:name="diagramName">test</ac:parameter></ac:structured-macro>"#;
        assert_eq!(preprocess_confluence_macros(html), html);
    }

    #[test]
    fn preprocess_lref_gdrive_inside_anchor_yields_text() {
        let url = "https://docs.google.com/spreadsheets/d/abc/edit?gid=0#gid=0";
        let html = format!(
            r#"<a href="{url}"><ac:structured-macro ac:name="lref-gdrive-file" ac:schema-version="1" ac:macro-id="m1"><ac:parameter ac:name="url">{url}</ac:parameter></ac:structured-macro></a>"#
        );
        let processed = preprocess_confluence_macros(&html);
        assert_eq!(
            processed,
            format!(r#"<a href="{url}">Google Drive Link</a>"#)
        );
    }

    #[test]
    fn preprocess_lref_gdrive_standalone_yields_anchor() {
        let url = "https://docs.google.com/spreadsheets/d/xyz/edit";
        let html = format!(
            r#"<p><ac:structured-macro ac:name="lref-gdrive-file" ac:schema-version="1" ac:macro-id="m2"><ac:parameter ac:name="url">{url}</ac:parameter></ac:structured-macro></p>"#
        );
        let processed = preprocess_confluence_macros(&html);
        assert!(processed.contains(&format!(r#"<a href="{url}">Google Drive Link</a>"#)));
        assert!(!processed.contains("ac:structured-macro"));
    }

    #[test]
    fn preprocess_lref_gdrive_no_url_param_removed() {
        let html = r#"<p><ac:structured-macro ac:name="lref-gdrive-file" ac:schema-version="1" ac:macro-id="m3"></ac:structured-macro></p>"#;
        let processed = preprocess_confluence_macros(html);
        assert!(!processed.contains("ac:structured-macro"));
    }

    #[test]
    fn apply_task_list_statuses_copies_storage_status_to_rendered_tasks() {
        let storage = r#"<ac:task-list>
<ac:task><ac:task-id>1</ac:task-id><ac:task-status>complete</ac:task-status><ac:task-body>done</ac:task-body></ac:task>
<ac:task><ac:task-id>2</ac:task-id><ac:task-status>incomplete</ac:task-status><ac:task-body>todo</ac:task-body></ac:task>
</ac:task-list>"#;
        let rendered = r#"<ul class="inline-task-list"><li data-inline-task-id="1">done</li><li data-inline-task-id="2">todo</li></ul>"#;

        let annotated = apply_task_list_statuses(rendered, storage);

        assert!(annotated.contains(r#"data-task-status="complete""#));
        assert!(annotated.contains(r#"data-task-status="incomplete""#));
    }

    #[test]
    fn preprocess_task_list_items_to_markdown_checkbox_text() {
        let html = r#"<ul class="inline-task-list"><li data-inline-task-id="1" data-task-status="complete">done</li><li data-inline-task-id="2" data-task-status="incomplete">todo</li><li data-inline-task-id="3">&nbsp;</li></ul>"#;

        let processed = preprocess_confluence_macros(html);

        assert!(processed.contains(">[x] done</li>"), "{processed}");
        assert!(processed.contains(">[ ] todo</li>"), "{processed}");
        assert!(processed.contains(">[ ]</li>"), "{processed}");
    }

    #[test]
    fn preprocess_rendered_lref_gdrive_inside_anchor() {
        let url = "https://docs.google.com/spreadsheets/d/abc/edit?gid=0#gid=0";
        let ctx = format!(r#"{{&quot;url&quot;:&quot;{url}&quot;}}"#);
        let html = format!(
            r#"<p>状態遷移検討資料：<a class="external-link" href="{url}" rel="nofollow"><span class="ap-container ap-inline"><span class="uninitialized_lref_module ap-content ap-inline" data-context="{ctx}" data-addon-key="com.bilith.lref.confluence-gdrive" data-module-key="lref-gdrive-file"></span></span></a></p>"#
        );
        let processed = preprocess_confluence_macros(&html);
        assert!(processed.contains(&format!(
            r#"<a class="external-link" href="{url}" rel="nofollow">Google Drive Link</a>"#
        )));
        assert!(!processed.contains("data-module-key"));
        assert!(!processed.contains("ap-container"));
    }

    #[test]
    fn preprocess_rendered_lref_gdrive_standalone() {
        let url = "https://docs.google.com/spreadsheets/d/xyz/edit";
        let ctx = format!(r#"{{&quot;url&quot;:&quot;{url}&quot;}}"#);
        let html = format!(
            r#"<p><span class="ap-container ap-inline"><span class="uninitialized_lref_module ap-content ap-inline" data-context="{ctx}" data-addon-key="com.bilith.lref.confluence-gdrive" data-module-key="lref-gdrive-file"></span></span></p>"#
        );
        let processed = preprocess_confluence_macros(&html);
        assert!(processed.contains(&format!(r#"<a href="{url}">Google Drive Link</a>"#)));
        assert!(!processed.contains("data-module-key"));
    }

    #[test]
    fn preprocess_expand_macro_to_details_summary() {
        let html = r#"<ac:structured-macro ac:name="expand" ac:schema-version="1" ac:macro-id="abc">
  <ac:parameter ac:name="title">Click to expand</ac:parameter>
  <ac:rich-text-body><p>Hidden body content.</p></ac:rich-text-body>
</ac:structured-macro>"#;
        let processed = preprocess_confluence_macros(html);
        assert!(processed.contains("<details>"));
        assert!(processed.contains("<summary>Click to expand</summary>"));
        assert!(processed.contains("Hidden body content."));
        assert!(processed.contains("</details>"));
        assert!(!processed.contains("ac:structured-macro"));
    }

    #[test]
    fn preprocess_expand_macro_with_nested_structured_macro() {
        let html = r#"<ac:structured-macro ac:name="expand" ac:schema-version="1" ac:macro-id="outer">
  <ac:parameter ac:name="title">変更履歴/Change history</ac:parameter>
  <ac:rich-text-body>
    <p>Before nested</p>
    <ac:structured-macro ac:name="jira" ac:schema-version="1" ac:macro-id="inner">
      <ac:parameter ac:name="key">DEMO-1234</ac:parameter>
    </ac:structured-macro>
    <p>After nested</p>
  </ac:rich-text-body>
</ac:structured-macro>"#;
        let processed = preprocess_confluence_macros(html);
        assert_eq!(processed.matches("<details>").count(), 1);
        assert!(processed.contains("<summary>変更履歴/Change history</summary>"));
        assert!(processed.contains("Before nested"));
        assert!(processed.contains("After nested"));
        assert!(!processed.contains(r#"ac:name="expand""#));
        assert!(processed.contains("DEMO-1234"));
    }

    #[test]
    fn preprocess_expand_macro_without_title() {
        let html = r#"<ac:structured-macro ac:name="expand"><ac:rich-text-body><p>Body only.</p></ac:rich-text-body></ac:structured-macro>"#;
        let processed = preprocess_confluence_macros(html);
        assert!(processed.contains("<summary></summary>"));
        assert!(processed.contains("Body only."));
    }
}
