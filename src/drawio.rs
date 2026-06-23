//! Draw.io diagram extraction, embedding, and rewriting.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::Client;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use sha1::{Digest, Sha1};
use tracing::{debug, warn};
use url::Url;

use crate::confluence::{Attachment, attachment_download_url};
use crate::utils::{
    escape_html, extract_macro_blocks, extract_macro_param, resolve_url, sanitize_file_name,
    to_markdown_asset_path,
};

// ── Public types ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DrawioDiagramRef {
    pub diagram_name: String,
    pub aspect_hash: Option<String>,
}

pub struct ResolveDrawioOptions<'a> {
    pub page_id: &'a str,
    pub storage_html: Option<&'a str>,
    pub export_html: &'a str,
    pub attachments_by_title: &'a std::collections::HashMap<String, Attachment>,
    pub base_url: &'a str,
    pub token: &'a str,
    pub assets_abs_dir: &'a Path,
    pub dump_state_abs_dir: Option<&'a Path>,
    pub markdown_image_prefix: &'a str,
    pub used_names: &'a mut HashSet<String>,
}

#[derive(Debug, Default)]
pub struct RewrittenHtml(pub String);

// ── Extraction ─────────────────────────────────────────────────────

pub fn extract_drawio_diagrams(storage_html: Option<&str>) -> Vec<DrawioDiagramRef> {
    let html = storage_html.unwrap_or_default();
    let mut refs = Vec::new();
    for block in extract_macro_blocks(html, "drawio") {
        let Some(diagram_name) = extract_macro_param(&block, "diagramName") else {
            continue;
        };
        let aspect_hash = extract_macro_param(&block, "aspectHash").filter(|s| !s.is_empty());
        refs.push(DrawioDiagramRef {
            diagram_name,
            aspect_hash,
        });
    }
    refs
}

pub fn extract_drawio_diagram_names(storage_html: Option<&str>) -> Vec<String> {
    extract_drawio_diagrams(storage_html)
        .into_iter()
        .map(|d| d.diagram_name)
        .collect()
}

// ── HTML rewriting ─────────────────────────────────────────────────

pub fn replace_drawio_script_blocks_with_imgs(
    html: Option<&str>,
    local_image_paths: &[String],
) -> String {
    let html = html.unwrap_or_default();
    if local_image_paths.is_empty() {
        return html.to_owned();
    }
    static RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r#"(?is)<span\b[^>]*id="drawio-macro-content-[^"]+"[^>]*></span>\s*<script\b.*?</script>"#,
        )
        .unwrap()
    });
    let mut index = 0usize;
    RE.replace_all(html, |_caps: &regex::Captures<'_>| {
        let Some(local) = local_image_paths.get(index) else {
            return String::new();
        };
        index += 1;
        format!(r#"<p><img src="{local}" alt="draw.io diagram" /></p>"#)
    })
    .into_owned()
}

pub fn replace_drawio_img_srcs(html: &str, local_image_paths: &[String]) -> String {
    if html.is_empty() || local_image_paths.is_empty() {
        return html.to_owned();
    }
    static RE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r#"(?is)(<img\b[^>]*\bclass="drawio-diagram-image"[^>]*\bsrc=)(["'])([^"']*)(["'])([^>]*>)"#,
        )
        .unwrap()
    });
    let mut index = 0usize;
    RE.replace_all(html, |caps: &regex::Captures<'_>| {
        let Some(local) = local_image_paths.get(index) else {
            return caps[0].to_owned();
        };
        index += 1;
        let prefix = &caps[1];
        let q_open = &caps[2];
        let q_close = &caps[4];
        let suffix = &caps[5];
        if q_open != q_close {
            return caps[0].to_owned();
        }
        format!("{prefix}{q_open}{local}{q_close}{suffix}")
    })
    .into_owned()
}

pub struct FallbackDiagram {
    pub local_path: String,
    pub label: String,
    pub alt: String,
}

pub fn append_fallback_diagrams_section(html: &str, diagrams: &[FallbackDiagram]) -> String {
    if diagrams.is_empty() {
        return html.to_owned();
    }
    let mut blocks = Vec::with_capacity(diagrams.len());
    for d in diagrams {
        let caption = if d.label.is_empty() {
            String::new()
        } else {
            format!("<p>{}</p>", escape_html(&d.label))
        };
        let alt_text = if !d.alt.is_empty() {
            d.alt.as_str()
        } else if !d.label.is_empty() {
            d.label.as_str()
        } else {
            "diagram"
        };
        blocks.push(format!(
            "<div class=\"confluence2md-fallback-diagram\">\n{caption}\n<p><img src=\"{}\" alt=\"{}\" /></p>\n</div>",
            d.local_path,
            escape_html(alt_text),
        ));
    }
    format!(
        "{html}\n<hr />\n<h2>Embedded diagrams</h2>\n{}\n",
        blocks.join("\n")
    )
}

// ── PNG tEXt chunk ─────────────────────────────────────────────────

static CRC32_TABLE: Lazy<[u32; 256]> = Lazy::new(|| {
    let mut table = [0u32; 256];
    for (n, slot) in table.iter_mut().enumerate() {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *slot = c;
    }
    table
});

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc = CRC32_TABLE[((crc ^ u32::from(b)) & 0xff) as usize] ^ (crc >> 8);
    }
    crc ^ 0xffff_ffff
}

fn build_png_text_chunk(keyword: &str, text: &str) -> Vec<u8> {
    let keyword_bytes = keyword.as_bytes();
    let text_bytes = text.as_bytes();
    let data_len = keyword_bytes.len() + 1 + text_bytes.len();

    let mut chunk = Vec::with_capacity(4 + 4 + data_len + 4);
    chunk.extend_from_slice(&(data_len as u32).to_be_bytes());
    chunk.extend_from_slice(b"tEXt");
    chunk.extend_from_slice(keyword_bytes);
    chunk.push(0);
    chunk.extend_from_slice(text_bytes);
    let crc = crc32(&chunk[4..4 + 4 + data_len]);
    chunk.extend_from_slice(&crc.to_be_bytes());
    chunk
}

const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/// `encodeURIComponent`-equivalent encoder.
fn encode_uri_component(s: &str) -> String {
    use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
    const SET: AsciiSet = NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'!')
        .remove(b'~')
        .remove(b'*')
        .remove(b'\'')
        .remove(b'(')
        .remove(b')');
    utf8_percent_encode(s, &SET).to_string()
}

/// Return the lowercase SHA-1 hex digest of `input`.
fn sha1_hex(input: &[u8]) -> String {
    use std::fmt::Write as _;
    // SHA-1 digest is 20 bytes; each byte encodes to 2 hex characters.
    let sha1_hex_len: usize = Sha1::output_size() * 2;
    let hash = Sha1::digest(input);
    let mut out = String::with_capacity(sha1_hex_len);
    for b in &hash {
        write!(out, "{b:02x}").unwrap();
    }
    out
}

/// Parsed representation of an mxfile XML document.
struct ParsedMxfile {
    /// The `<mxfile>` opening tag, rebuilt with `pages="1"`.
    open_tag: String,
    /// Each entry: (diagram `id` attribute value, byte range of the full
    /// `<diagram>...</diagram>` element in the source XML string).
    diagrams: Vec<(String, std::ops::Range<usize>)>,
}

/// Parse `xml` as an mxfile document and return the opening `<mxfile>` tag
/// (with `pages` normalised to `"1"`) together with the byte ranges of each
/// `<diagram>` child element.
///
/// Returns `None` if the document cannot be parsed or contains no `<mxfile>`
/// root element.
fn parse_mxfile(xml: &str) -> Option<ParsedMxfile> {
    use quick_xml::{
        Reader, Writer,
        events::{BytesStart, Event},
        name::QName,
    };
    use std::io::Cursor;

    // Categorise each XML event before releasing the borrow on `buf`, so that
    // `reader` and `buf` are free for subsequent calls (e.g. `read_to_end_into`).
    enum Parsed {
        /// The `<mxfile>` opening tag, rebuilt with `pages="1"`.
        MxfileOpen(String),
        /// A `<diagram>` start tag: element name bytes (for `read_to_end_into`)
        /// and the value of its `id` attribute.
        DiagramStart {
            name_bytes: Vec<u8>,
            id: String,
        },
        Eof,
        Other,
    }

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut mxfile_open: Option<String> = None;
    let mut diagrams: Vec<(String, std::ops::Range<usize>)> = Vec::new();
    let mut buf = Vec::new();

    loop {
        // Record the byte offset of the *start* of the next event before consuming it.
        let pos_before = reader.buffer_position() as usize;

        // Parse the next event inside a block so the event's borrow of `buf`
        // ends here, freeing `reader` and `buf` for use below.
        let parsed = {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) if e.name().as_ref() == b"mxfile" => {
                    // Rebuild the opening tag, changing `pages="N"` to `pages="1"`.
                    let mut tag = BytesStart::new("mxfile");
                    for attr in e.attributes().filter_map(|a| a.ok()) {
                        if attr.key.as_ref() == b"pages" {
                            tag.push_attribute(("pages", "1"));
                        } else {
                            tag.push_attribute(attr);
                        }
                    }
                    let mut w = Writer::new(Cursor::new(Vec::<u8>::new()));
                    let Ok(()) = w.write_event(Event::Start(tag)) else {
                        return None;
                    };
                    let Ok(s) = String::from_utf8(w.into_inner().into_inner()) else {
                        return None;
                    };
                    Parsed::MxfileOpen(s)
                }
                Ok(Event::Start(ref e)) if e.name().as_ref() == b"diagram" => {
                    // Clone the name bytes so we can release the borrow on `buf`.
                    let name_bytes = e.name().as_ref().to_vec();
                    // Extract the `id` attribute value as an owned String.
                    let id = e
                        .attributes()
                        .filter_map(|a| a.ok())
                        .find(|a| a.key.as_ref() == b"id")
                        .and_then(|a| a.unescape_value().ok())
                        .map(|s| s.into_owned())
                        .unwrap_or_default();
                    Parsed::DiagramStart { name_bytes, id }
                }
                Ok(Event::Eof) => Parsed::Eof,
                Err(_) => return None,
                _ => Parsed::Other,
            }
        };

        match parsed {
            Parsed::MxfileOpen(s) => mxfile_open = Some(s),
            Parsed::DiagramStart { name_bytes, id } => {
                // Advance past the matching `</diagram>` end tag.
                let mut end_buf = Vec::new();
                if reader
                    .read_to_end_into(QName(&name_bytes), &mut end_buf)
                    .is_err()
                {
                    return None;
                }
                // `pos_before` is the start of `<diagram`, and `buffer_position()`
                // is now just after `</diagram>`, so this slice is the full element.
                diagrams.push((id, pos_before..reader.buffer_position() as usize));
            }
            Parsed::Eof => break,
            Parsed::Other => {}
        }

        buf.clear();
    }

    Some(ParsedMxfile {
        open_tag: mxfile_open?,
        diagrams,
    })
}

/// Return a new mxfile XML containing only the `<diagram>` element selected by
/// `aspect_hash`:
/// - `None`  → the first `<diagram>` element
/// - `Some(hash)` → the `<diagram>` whose `id` SHA-1 hashes to `hash`
///
/// Returns `None` when no matching diagram is found.
pub fn filter_mxfile_to_aspect_hash(xml: &str, aspect_hash: Option<&str>) -> Option<String> {
    let ParsedMxfile { open_tag, diagrams } = parse_mxfile(xml)?;

    // Select the diagram to keep: the first one, or the one whose `id`
    // SHA-1-hashes to `aspect_hash`.
    let (_, range) = match aspect_hash {
        None => diagrams.first()?,
        Some(hash) => {
            let target = hash.to_ascii_lowercase();
            diagrams
                .iter()
                .find(|(id, _)| sha1_hex(id.as_bytes()) == target)?
        }
    };

    let selected_xml = xml.get(range.clone())?;
    // Preserve the `</mxfile>` closing tag and any trailing content.
    let suffix = xml
        .rfind("</mxfile>")
        .map_or("</mxfile>", |pos| &xml[pos..]);

    Some(format!("{open_tag}{selected_xml}{suffix}"))
}

pub fn embed_drawio_xml_in_png(png_bytes: &[u8], drawio_xml: &str) -> Result<Vec<u8>> {
    if png_bytes.len() < 8 || png_bytes[..8] != PNG_SIGNATURE {
        bail!("Not a valid PNG file");
    }
    if png_bytes.len() < 8 + 8 {
        bail!("Not a valid PNG file");
    }
    let ihdr_length = u32::from_be_bytes(png_bytes[8..12].try_into().unwrap()) as usize;
    let ihdr_end = 8 + 4 + 4 + ihdr_length + 4;
    if png_bytes.len() < ihdr_end {
        bail!("Not a valid PNG file");
    }

    let encoded_xml = encode_uri_component(drawio_xml);
    let text_chunk = build_png_text_chunk("mxfile", &encoded_xml);

    let mut result = Vec::with_capacity(png_bytes.len() + text_chunk.len());
    result.extend_from_slice(&png_bytes[..ihdr_end]);
    result.extend_from_slice(&text_chunk);
    result.extend_from_slice(&png_bytes[ihdr_end..]);
    Ok(result)
}

// ── High-level resolution ──────────────────────────────────────────

fn binary_auth_headers(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {token}")) {
        headers.insert(AUTHORIZATION, v);
    }
    headers
}

fn pick_unique(used: &mut HashSet<String>, candidate: String) -> String {
    if !used.contains(&candidate) {
        used.insert(candidate.clone());
        return candidate;
    }
    let path = std::path::Path::new(&candidate);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    let mut i = 1u32;
    loop {
        let c = format!("{stem}_{i}{ext}");
        if !used.contains(&c) {
            used.insert(c.clone());
            return c;
        }
        i += 1;
    }
}

#[derive(Debug, Clone)]
struct DrawioAssetSource {
    page_id: String,
    diagram_name: String,
    aspect_hash: Option<String>,
    png_title: String,
    png_url: String,
    xml_url: Option<String>,
    xml_file_name: Option<String>,
    output_base_name: String,
}

fn extract_rendered_drawio_sources(html: &str, base_url: &str) -> Vec<DrawioAssetSource> {
    static IMG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?is)<img\b[^>]*>"#).unwrap());
    static CLASS_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?is)\bclass\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap());
    static SRC_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?is)\bsrc\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap());

    IMG_RE
        .find_iter(html)
        .filter_map(|m| {
            let tag = m.as_str();
            let has_drawio_class = CLASS_RE
                .captures(tag)
                .and_then(|caps| caps.get(1).or_else(|| caps.get(2)))
                .is_some_and(|class| {
                    class
                        .as_str()
                        .split_whitespace()
                        .any(|name| name.eq_ignore_ascii_case("drawio-diagram-image"))
                });
            if !has_drawio_class {
                return None;
            }
            let src = SRC_RE
                .captures(tag)
                .and_then(|caps| caps.get(1).or_else(|| caps.get(2)))
                .map(|m| m.as_str())?;
            source_from_rendered_drawio_src(src, base_url)
        })
        .collect()
}

fn source_from_rendered_drawio_src(src: &str, base_url: &str) -> Option<DrawioAssetSource> {
    let absolute = resolve_url(src, base_url);
    let url = Url::parse(&absolute).ok()?;
    let segments: Vec<String> = url
        .path_segments()?
        .map(|segment| {
            percent_encoding::percent_decode_str(segment)
                .decode_utf8_lossy()
                .into_owned()
        })
        .collect();

    let mut page_id = None;
    let mut png_title = None;
    for window in segments.windows(4) {
        if window[0] == "download" && window[1] == "attachments" {
            page_id = Some(window[2].clone());
            png_title = Some(window[3].clone());
            break;
        }
    }
    let page_id = page_id?;
    let png_title = png_title?;
    if !png_title.to_ascii_lowercase().ends_with(".png") {
        return None;
    }

    static HASHED_PNG_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?i)^(.+)-([0-9a-f]{40})\.png$"#).unwrap());

    let (diagram_name, aspect_hash, xml_title, output_base_name) =
        if let Some(name) = png_title.strip_suffix(".drawio.png") {
            let xml_title = format!("{name}.drawio");
            (
                name.to_owned(),
                None,
                xml_title,
                format!("{name}.drawio.png"),
            )
        } else if let Some(caps) = HASHED_PNG_RE.captures(&png_title) {
            let name = caps[1].to_owned();
            let hash = caps[2].to_owned();
            (
                name.clone(),
                Some(hash.clone()),
                name.clone(),
                format!("{name}-{hash}.drawio.png"),
            )
        } else {
            let name = png_title.trim_end_matches(".png").to_owned();
            (
                name.clone(),
                None,
                name.clone(),
                format!("{name}.drawio.png"),
            )
        };

    let xml_attachment = Attachment {
        id: String::new(),
        title: xml_title.clone(),
        media_type: None,
        download_path: None,
        webui_path: None,
    };

    Some(DrawioAssetSource {
        page_id: page_id.clone(),
        diagram_name,
        aspect_hash,
        png_title,
        png_url: absolute,
        xml_url: Some(attachment_download_url(base_url, &page_id, &xml_attachment)),
        xml_file_name: Some(xml_title),
        output_base_name,
    })
}

fn storage_drawio_source(
    dref: &DrawioDiagramRef,
    opts: &ResolveDrawioOptions<'_>,
) -> Option<DrawioAssetSource> {
    let page_attachment_title = dref
        .aspect_hash
        .as_deref()
        .map(|h| format!("{}-{}.png", dref.diagram_name, h));
    let png_attachment = page_attachment_title
        .as_deref()
        .and_then(|t| opts.attachments_by_title.get(t))
        .or_else(|| {
            opts.attachments_by_title
                .get(&format!("{}.png", dref.diagram_name))
        })?;
    let drawio_attachment = opts.attachments_by_title.get(&dref.diagram_name);
    let output_base_name = match &dref.aspect_hash {
        Some(h) => format!("{}-{}.drawio.png", dref.diagram_name, h),
        None => format!("{}.drawio.png", dref.diagram_name),
    };

    Some(DrawioAssetSource {
        page_id: opts.page_id.to_owned(),
        diagram_name: dref.diagram_name.clone(),
        aspect_hash: dref.aspect_hash.clone(),
        png_title: png_attachment.title.clone(),
        png_url: attachment_download_url(opts.base_url, opts.page_id, png_attachment),
        xml_url: drawio_attachment
            .map(|att| attachment_download_url(opts.base_url, opts.page_id, att)),
        xml_file_name: Some(format!("{}.drawio", dref.diagram_name)),
        output_base_name,
    })
}

fn order_drawio_sources(
    rendered_sources: Vec<DrawioAssetSource>,
    storage_sources: Vec<DrawioAssetSource>,
) -> Vec<DrawioAssetSource> {
    let mut sources = Vec::with_capacity(rendered_sources.len() + storage_sources.len());
    let mut used_storage_sources = vec![false; storage_sources.len()];

    for rendered in rendered_sources {
        let storage_index = storage_sources
            .iter()
            .enumerate()
            .position(|(index, source)| {
                !used_storage_sources[index]
                    && rendered.page_id == source.page_id
                    && rendered.png_title == source.png_title
            });
        match storage_index {
            Some(index) => {
                used_storage_sources[index] = true;
                sources.push(storage_sources[index].clone());
            }
            None => sources.push(rendered),
        }
    }

    for (index, source) in storage_sources.into_iter().enumerate() {
        if !used_storage_sources[index] {
            sources.push(source);
        }
    }

    sources
}

async fn materialize_drawio_source(
    client: &Client,
    opts: &mut ResolveDrawioOptions<'_>,
    source: &DrawioAssetSource,
    xml_cache: &mut std::collections::HashMap<String, Option<String>>,
    dump_used_names: &mut HashSet<String>,
) -> Result<String> {
    let png_response = client
        .get(&source.png_url)
        .headers(binary_auth_headers(opts.token))
        .send()
        .await
        .context("PNG fetch")?;
    if !png_response.status().is_success() {
        bail!(
            "{} {}",
            png_response.status().as_u16(),
            png_response.status().canonical_reason().unwrap_or("")
        );
    }
    let mut png_bytes: Vec<u8> = png_response.bytes().await?.to_vec();

    if let Some(xml_url) = &source.xml_url {
        if !xml_cache.contains_key(xml_url) {
            debug!(
                "Downloading draw.io XML: name: {}, url: {}",
                source.diagram_name, xml_url
            );
            let xml_response = client
                .get(xml_url)
                .headers(binary_auth_headers(opts.token))
                .send()
                .await;
            let drawio_xml: Option<String> = match xml_response {
                Ok(r) if r.status().is_success() => match r.text().await {
                    Ok(t) => {
                        debug!(
                            "Downloaded draw.io XML: length: {}, starts with: {}",
                            t.len(),
                            &t.chars().take(80).collect::<String>()
                        );
                        if let (Some(dump_dir), Some(xml_file_name)) =
                            (opts.dump_state_abs_dir, &source.xml_file_name)
                        {
                            let unique =
                                pick_unique(dump_used_names, sanitize_file_name(xml_file_name));
                            tokio::fs::write(dump_dir.join(&unique), &t)
                                .await
                                .with_context(|| format!("write {unique}"))?;
                            debug!("Saved draw.io XML dump: file: {unique}");
                        }
                        Some(t)
                    }
                    Err(_) => None,
                },
                Ok(r) => {
                    warn!(
                        "draw.io XML download failed: {} {}",
                        r.status().as_u16(),
                        r.status().canonical_reason().unwrap_or("")
                    );
                    None
                }
                Err(e) => {
                    warn!("draw.io XML download failed: {e}");
                    None
                }
            };
            xml_cache.insert(xml_url.clone(), drawio_xml);
        }
        if let Some(Some(xml)) = xml_cache.get(xml_url) {
            let xml_to_embed: std::borrow::Cow<'_, str> = match filter_mxfile_to_aspect_hash(
                xml,
                source.aspect_hash.as_deref(),
            ) {
                Some(filtered) => std::borrow::Cow::Owned(filtered),
                None => {
                    warn!(
                        "draw.io diagram not found in {} (aspectHash: {:?}); embedding full mxfile",
                        source.diagram_name, source.aspect_hash
                    );
                    std::borrow::Cow::Borrowed(xml.as_str())
                }
            };
            match embed_drawio_xml_in_png(&png_bytes, &xml_to_embed) {
                Ok(b) => png_bytes = b,
                Err(e) => {
                    warn!(
                        "Failed to embed draw.io XML for {}: {e}",
                        source.diagram_name
                    );
                }
            }
        }
    } else {
        warn!(
            "draw.io attachment not found for: \"{}\"",
            source.diagram_name
        );
    }

    let file_name = sanitize_file_name(&source.output_base_name);
    let unique = pick_unique(opts.used_names, file_name);
    tokio::fs::write(opts.assets_abs_dir.join(&unique), &png_bytes)
        .await
        .with_context(|| format!("write {unique}"))?;
    Ok(to_markdown_asset_path(opts.markdown_image_prefix, &unique))
}

pub async fn resolve_drawio_diagrams(
    client: &Client,
    mut opts: ResolveDrawioOptions<'_>,
) -> Result<RewrittenHtml> {
    let rendered_sources = extract_rendered_drawio_sources(opts.export_html, opts.base_url);
    let storage_sources: Vec<DrawioAssetSource> = extract_drawio_diagrams(opts.storage_html)
        .iter()
        .filter_map(|dref| storage_drawio_source(dref, &opts))
        .collect();
    if rendered_sources.is_empty() && storage_sources.is_empty() {
        return Ok(RewrittenHtml(opts.export_html.to_owned()));
    }

    let sources = order_drawio_sources(rendered_sources, storage_sources);
    let mut xml_cache = std::collections::HashMap::new();
    let mut local_paths: Vec<String> = Vec::new();
    let mut dump_used_names: HashSet<String> = HashSet::new();

    for source in sources {
        match materialize_drawio_source(
            client,
            &mut opts,
            &source,
            &mut xml_cache,
            &mut dump_used_names,
        )
        .await
        {
            Ok(local_path) => local_paths.push(local_path),
            Err(_) => warn!(
                "Failed to fetch draw.io preview attachment: {}",
                source.png_title
            ),
        }
    }

    let mut rewritten =
        replace_drawio_script_blocks_with_imgs(Some(opts.export_html), &local_paths);
    rewritten = replace_drawio_img_srcs(&rewritten, &local_paths);
    Ok(RewrittenHtml(rewritten))
}

// Suppress unused-warning on imports we keep for the public API surface.
#[allow(dead_code)]
fn _suppress(_: anyhow::Error) {}
#[allow(dead_code)]
fn _suppress2() -> anyhow::Error {
    anyhow!("never used")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::time::{SystemTime, UNIX_EPOCH};

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn create_minimal_png() -> Vec<u8> {
        let signature: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        let ihdr: [u8; 25] = [
            0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
            0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xde,
        ];
        let idat: [u8; 24] = [
            0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8, 0xcf, 0xc0,
            0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0x02, 0x6f, 0x43, 0xb6,
        ];
        let iend: [u8; 12] = [
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        let mut out = Vec::new();
        out.extend_from_slice(&signature);
        out.extend_from_slice(&ihdr);
        out.extend_from_slice(&idat);
        out.extend_from_slice(&iend);
        out
    }

    #[test]
    fn embed_drawio_xml_in_png_inserts_mxfile_text_chunk() {
        let png = create_minimal_png();
        let xml = r#"<mxfile><diagram name="test"><mxGraphModel><root><mxCell id="0"/></root></mxGraphModel></diagram></mxfile>"#;
        let result = embed_drawio_xml_in_png(&png, xml).unwrap();
        assert!(result.len() > png.len());
        assert_eq!(&result[..8], &PNG_SIGNATURE);

        // Locate the tEXt chunk.
        let pos = result
            .windows(4)
            .position(|w| w == b"tEXt")
            .expect("tEXt chunk found");
        let len_idx = pos - 4;
        let chunk_len =
            u32::from_be_bytes(result[len_idx..len_idx + 4].try_into().unwrap()) as usize;
        let chunk_data = &result[pos + 4..pos + 4 + chunk_len];
        let null_idx = chunk_data.iter().position(|&b| b == 0).unwrap();
        let keyword = std::str::from_utf8(&chunk_data[..null_idx]).unwrap();
        let text_value = std::str::from_utf8(&chunk_data[null_idx + 1..]).unwrap();
        assert_eq!(keyword, "mxfile");
        assert!(text_value.starts_with("%3Cmxfile"));
        let decoded = percent_encoding::percent_decode_str(text_value)
            .decode_utf8()
            .unwrap();
        assert_eq!(decoded, xml);

        let last_four = &result[result.len() - 8..result.len() - 4];
        assert_eq!(last_four, b"IEND");
    }

    #[test]
    fn embed_drawio_xml_handles_non_ascii_safely() {
        let png = create_minimal_png();
        let xml = "<mxfile><diagram name=\"\u{30da}\u{30fc}\u{30b8}1\"><mxGraphModel><root><mxCell id=\"0\"/></root></mxGraphModel></diagram></mxfile>";
        let result = embed_drawio_xml_in_png(&png, xml).unwrap();
        let pos = result.windows(4).position(|w| w == b"tEXt").unwrap();
        let len_idx = pos - 4;
        let chunk_len =
            u32::from_be_bytes(result[len_idx..len_idx + 4].try_into().unwrap()) as usize;
        let chunk_data = &result[pos + 4..pos + 4 + chunk_len];
        for &b in chunk_data {
            assert!(b <= 0x7f);
        }
        let chunk_str = std::str::from_utf8(chunk_data).unwrap();
        let null_idx = chunk_str.find('\0').unwrap();
        let text_value = &chunk_str[null_idx + 1..];
        assert!(text_value.starts_with("%3Cmxfile"));
        let decoded = percent_encoding::percent_decode_str(text_value)
            .decode_utf8()
            .unwrap();
        assert!(decoded.contains("\u{30da}\u{30fc}\u{30b8}1"));
        assert!(decoded.contains("<mxGraphModel>"));
    }

    #[test]
    fn embed_drawio_xml_preserves_remaining_data() {
        let png = create_minimal_png();
        let xml = "<mxfile>test</mxfile>";
        let result = embed_drawio_xml_in_png(&png, xml).unwrap();
        let ihdr_length = u32::from_be_bytes(png[8..12].try_into().unwrap()) as usize;
        let ihdr_end = 8 + 4 + 4 + ihdr_length + 4;
        assert_eq!(&result[..ihdr_end], &png[..ihdr_end]);

        let text_type = &result[ihdr_end + 4..ihdr_end + 8];
        assert_eq!(text_type, b"tEXt");

        let text_chunk_len =
            u32::from_be_bytes(result[ihdr_end..ihdr_end + 4].try_into().unwrap()) as usize;
        let after_text = ihdr_end + 4 + 4 + text_chunk_len + 4;
        assert_eq!(&result[after_text..], &png[ihdr_end..]);
    }

    #[test]
    fn embed_drawio_xml_rejects_invalid_png() {
        let not_png: [u8; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
        let err = embed_drawio_xml_in_png(&not_png, "<mxfile/>").unwrap_err();
        assert!(err.to_string().contains("Not a valid PNG"));
    }

    #[test]
    fn replace_drawio_img_srcs_rewrites_drawio_only() {
        let html = r#"<p><img class="drawio-diagram-image" src="assets%2Fdiagram1.png" /></p><p>middle</p><img class="drawio-diagram-image" src="assets%2Fdiagram2.png" />"#;
        let result = replace_drawio_img_srcs(
            html,
            &[
                "assets%2Fdiagram1.drawio.png".to_owned(),
                "assets%2Fdiagram2.drawio.png".to_owned(),
            ],
        );
        assert!(result.contains(r#"src="assets%2Fdiagram1.drawio.png""#));
        assert!(result.contains(r#"src="assets%2Fdiagram2.drawio.png""#));
        assert!(!result.contains(r#"src="assets%2Fdiagram1.png""#));
    }

    #[test]
    fn replace_drawio_img_srcs_leaves_non_drawio_alone() {
        let html = r#"<img src="assets%2Fother.png" /><img class="drawio-diagram-image" src="assets%2Fdrawio.png" />"#;
        let result = replace_drawio_img_srcs(html, &["assets%2Fdrawio.drawio.png".to_owned()]);
        assert!(result.contains(r#"src="assets%2Fother.png""#));
        assert!(result.contains(r#"src="assets%2Fdrawio.drawio.png""#));
    }

    #[test]
    fn replace_drawio_img_srcs_no_paths_unchanged() {
        let html = r#"<img class="drawio-diagram-image" src="assets%2Fdrawio.png" />"#;
        assert_eq!(replace_drawio_img_srcs(html, &[]), html);
    }

    #[test]
    fn extract_drawio_diagram_names_empty_for_no_macros() {
        assert!(extract_drawio_diagram_names(Some("<p>no macros</p>")).is_empty());
        assert!(extract_drawio_diagram_names(None).is_empty());
    }

    #[test]
    fn extract_rendered_drawio_images_derives_cross_page_drawio_source() {
        let html = r#"<p><img class="drawio-diagram-image" src="/download/attachments/1320525625/test.drawio.png?version=1&amp;api=v2" /></p>"#;
        let refs = extract_rendered_drawio_sources(html, "https://confluence.example.com");

        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].page_id, "1320525625");
        assert_eq!(refs[0].png_title, "test.drawio.png");
        assert_eq!(refs[0].xml_file_name.as_deref(), Some("test.drawio"));
        assert_eq!(refs[0].output_base_name, "test.drawio.png");
    }

    #[tokio::test]
    async fn resolve_drawio_fallbacks_saves_rendered_cross_page_drawio_in_html_order() {
        let server = MockServer::start().await;
        let png = create_minimal_png();
        let external_xml =
            r#"<mxfile><diagram id="external" name="external"><a/></diagram></mxfile>"#;
        let own_xml = r#"<mxfile><diagram id="own" name="own"><b/></diagram></mxfile>"#;

        Mock::given(method("GET"))
            .and(path("/download/attachments/other/External.drawio.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(png.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/download/attachments/other/External.drawio"))
            .respond_with(ResponseTemplate::new(200).set_body_string(external_xml))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/download/attachments/current/Own-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.png",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(png.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/download/attachments/current/Own"))
            .respond_with(ResponseTemplate::new(200).set_body_string(own_xml))
            .mount(&server)
            .await;

        let html = r#"<p><img class="drawio-diagram-image" src="/download/attachments/other/External.drawio.png?version=1" /></p><p><img class="drawio-diagram-image" src="/download/attachments/current/Own-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.png?version=3" /></p>"#;
        let storage = r#"<ac:structured-macro ac:name="drawio"><ac:parameter ac:name="diagramName">Own</ac:parameter><ac:parameter ac:name="aspectHash">aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</ac:parameter></ac:structured-macro>"#;
        let attachments = HashMap::from([
            (
                "Own-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.png".to_owned(),
                Attachment {
                    id: "1".to_owned(),
                    title: "Own-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.png".to_owned(),
                    media_type: Some("image/png".to_owned()),
                    download_path: None,
                    webui_path: None,
                },
            ),
            (
                "Own".to_owned(),
                Attachment {
                    id: "2".to_owned(),
                    title: "Own".to_owned(),
                    media_type: None,
                    download_path: None,
                    webui_path: None,
                },
            ),
        ]);
        let temp_root = std::env::temp_dir().join(format!(
            "confluence2md-drawio-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let assets_dir = temp_root.join("assets");
        tokio::fs::create_dir_all(&assets_dir).await.unwrap();
        let mut used_names = HashSet::new();

        let result = resolve_drawio_diagrams(
            &Client::new(),
            ResolveDrawioOptions {
                page_id: "current",
                storage_html: Some(storage),
                export_html: html,
                attachments_by_title: &attachments,
                base_url: &server.uri(),
                token: "token",
                assets_abs_dir: &assets_dir,
                dump_state_abs_dir: None,
                markdown_image_prefix: "assets",
                used_names: &mut used_names,
            },
        )
        .await
        .unwrap();

        assert!(result.0.contains(r#"src="assets%2FExternal.drawio.png""#));
        assert!(
            result.0.contains(
                r#"src="assets%2FOwn-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.drawio.png""#
            )
        );
        assert!(assets_dir.join("External.drawio.png").exists());
        assert!(!assets_dir.join("External.drawio").exists());
        assert!(!assets_dir.join("Own.drawio").exists());
        assert!(
            assets_dir
                .join("Own-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.drawio.png")
                .exists()
        );

        let _ = tokio::fs::remove_dir_all(&temp_root).await;
    }

    #[tokio::test]
    async fn resolve_drawio_fallbacks_writes_drawio_xml_only_to_dump_state_dir() {
        let server = MockServer::start().await;
        let png = create_minimal_png();
        let xml = r#"<mxfile><diagram id="dump-test" name="dump-test"><a/></diagram></mxfile>"#;

        Mock::given(method("GET"))
            .and(path("/download/attachments/current/diagram.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(png))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/download/attachments/current/diagram"))
            .respond_with(ResponseTemplate::new(200).set_body_string(xml))
            .mount(&server)
            .await;

        let storage = r#"<ac:structured-macro ac:name="drawio"><ac:parameter ac:name="diagramName">diagram</ac:parameter></ac:structured-macro>"#;
        let attachments = HashMap::from([
            (
                "diagram.png".to_owned(),
                Attachment {
                    id: "1".to_owned(),
                    title: "diagram.png".to_owned(),
                    media_type: Some("image/png".to_owned()),
                    download_path: None,
                    webui_path: None,
                },
            ),
            (
                "diagram".to_owned(),
                Attachment {
                    id: "2".to_owned(),
                    title: "diagram".to_owned(),
                    media_type: None,
                    download_path: None,
                    webui_path: None,
                },
            ),
        ]);

        let temp_root = std::env::temp_dir().join(format!(
            "confluence2md-drawio-dump-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let assets_dir = temp_root.join("assets");
        let dump_dir = temp_root.join("dump");
        tokio::fs::create_dir_all(&assets_dir).await.unwrap();
        tokio::fs::create_dir_all(&dump_dir).await.unwrap();
        let mut used_names = HashSet::new();

        let _ = resolve_drawio_diagrams(
            &Client::new(),
            ResolveDrawioOptions {
                page_id: "current",
                storage_html: Some(storage),
                export_html: "<p>drawio</p>",
                attachments_by_title: &attachments,
                base_url: &server.uri(),
                token: "token",
                assets_abs_dir: &assets_dir,
                dump_state_abs_dir: Some(&dump_dir),
                markdown_image_prefix: "assets",
                used_names: &mut used_names,
            },
        )
        .await
        .unwrap();

        assert!(assets_dir.join("diagram.drawio.png").exists());
        assert!(!assets_dir.join("diagram.drawio").exists());
        assert!(dump_dir.join("diagram.drawio").exists());

        let _ = tokio::fs::remove_dir_all(&temp_root).await;
    }

    #[test]
    fn sha1_hex_matches_known_vectors() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_hex(b"dhv8lTtAVSGf6rndoTdm"),
            "90a471c9f6e67592b1a4925f41581bedd9e05408"
        );
        assert_eq!(
            sha1_hex(b"uDV5wrpQSX34VL1bIryd"),
            "8617a86e51e7441130c2329cc97cafe9c7032e90"
        );
    }

    #[test]
    fn filter_mxfile_none_keeps_only_first_and_normalizes_pages() {
        let xml = r#"<mxfile host="x" pages="3"><diagram id="aaa" name="ページ1"><a/></diagram><diagram id="bbb" name="ページ2"><b/></diagram><diagram id="ccc" name="ページ3"><c/></diagram></mxfile>"#;
        let out = filter_mxfile_to_aspect_hash(xml, None).unwrap();
        assert!(out.contains(r#"name="ページ1""#));
        assert!(!out.contains(r#"name="ページ2""#));
        assert!(!out.contains(r#"name="ページ3""#));
        assert!(out.contains(r#"pages="1""#));
        assert!(out.contains("<a/>"));
        assert!(!out.contains("<b/>"));
        assert!(!out.contains("<c/>"));
    }

    #[test]
    fn filter_mxfile_none_returns_none_when_no_diagram() {
        assert!(filter_mxfile_to_aspect_hash("<mxfile></mxfile>", None).is_none());
    }

    #[test]
    fn filter_mxfile_keeps_only_matching_diagram_and_normalizes_pages() {
        let xml = r#"<mxfile host="x" pages="3"><diagram id="dhv8lTtAVSGf6rndoTdm" name="ページ1"><a/></diagram><diagram id="uDV5wrpQSX34VL1bIryd" name="ページ2"><b/></diagram><diagram id="qiFhaAI4dCEqhqX3Nilp" name="ページ3"><c/></diagram></mxfile>"#;
        let out =
            filter_mxfile_to_aspect_hash(xml, Some("8617a86e51e7441130c2329cc97cafe9c7032e90"))
                .unwrap();
        assert!(out.contains(r#"name="ページ2""#));
        assert!(!out.contains(r#"name="ページ1""#));
        assert!(!out.contains(r#"name="ページ3""#));
        assert!(out.contains(r#"pages="1""#));
        assert!(out.contains("<b/>"));
        assert!(!out.contains("<a/>"));
        assert!(!out.contains("<c/>"));
    }

    #[test]
    fn filter_mxfile_returns_none_when_no_match() {
        let xml = r#"<mxfile><diagram id="only" name="x"></diagram></mxfile>"#;
        assert!(filter_mxfile_to_aspect_hash(xml, Some("deadbeef")).is_none());
    }
}
