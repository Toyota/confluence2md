//! HTML-to-Markdown converter for Confluence export-view HTML.
//!
//! The heavy lifting is delegated to the [`htmd`](https://crates.io/crates/htmd)
//! crate. This module adds Confluence-specific behaviour on top of htmd's
//! built-in handlers:
//!
//! * `<div class="confluence-information-macro …">` is rendered as a GitHub
//!   alert blockquote (`> [!IMPORTANT]`, `> [!WARNING]`, etc.).
//! * `<div class="expand-container">` / `expand-control-text` /
//!   `expand-content` mirrors are recognised in case they survive the
//!   storage-format preprocess pass. Preprocessed expand macros that are
//!   already `<details>` / `<summary>` are emitted back as raw HTML so
//!   GitHub-flavoured Markdown can collapse them.
//! * `pre.syntaxhighlighter-pre` becomes a fenced code block using the
//!   brush name from `data-syntaxhighlighter-params`.
//! * Markdown-compatible tables without an explicit `<thead>` have their first
//!   `<tr>` promoted to a header row so htmd emits a GFM table rather than
//!   flattening the cells as plain text.
//! * 1x1 tables are unwrapped: the single cell's content is emitted as normal
//!   Markdown outside of a table.
//! * In `TableConversion::Default` mode, tables with merged cells
//!   (colspan/rowspan) are preserved as raw HTML since Markdown cannot
//!   represent cell spans.
//! * In `TableConversion::Default` mode, tables that contain nested tables are
//!   also preserved as raw HTML (pretty-printed) since nested tables are not
//!   representable in GFM tables.
//! * In `TableConversion::Always` mode, tables with merged cells are expanded
//!   into a flat grid: the top-left cell of each merged region keeps its
//!   content and the remaining spanned positions become empty cells.
//! * In `TableConversion::Always` mode, nested tables are extracted and placed
//!   after the outer table as standalone markdown tables. The outer table cell
//!   is replaced with a unique marker such as `(*1)` for traceability.

use std::collections::HashMap;

use htmd::{
    Element, HtmlToMarkdown,
    element_handler::{HandlerResult, Handlers},
    options::{BulletListMarker, Options},
};
use lightningcss::{
    properties::{
        Property::{self},
        text::TextDecorationLine,
    },
    stylesheet::{ParserOptions, StyleAttribute},
};
use markup5ever_rcdom::NodeData;
use once_cell::sync::Lazy;
use regex::Regex;

use crate::utils::{decode_html_attribute, escape_html, strip_tags};

// ── Public API ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TableConversion {
    #[default]
    Default,
    Always,
}

#[derive(Debug, Clone, Default)]
pub struct ConvertOptions {
    pub table_conversion: TableConversion,
}

/// Convert `html` to GitHub-flavoured Markdown with Confluence-specific
/// extensions.
pub fn convert_to_md(html: &str, options: ConvertOptions) -> String {
    let heading_links_rewritten = rewrite_internal_heading_links(html);
    match options.table_conversion {
        TableConversion::Default => {
            let promoted = promote_markdown_compatible_tables_to_thead(&heading_links_rewritten);
            let converter = build_converter(true);
            let md = converter.convert(&promoted).unwrap_or_default();
            post_process(&md)
        }
        TableConversion::Always => {
            let (nested_processed, nested_replacements) =
                preprocess_nested_tables_always(&heading_links_rewritten);
            let expanded = expand_merged_cells(&nested_processed);
            let promoted = promote_first_row_to_thead(&expanded);
            let converter = build_converter(false);
            let mut md = post_process(&converter.convert(&promoted).unwrap_or_default());
            for (token, replacement) in nested_replacements {
                md = md.replace(&token, &replacement);
            }
            post_process(&md)
        }
    }
}

// ── Converter setup ────────────────────────────────────────────────

fn build_converter(preserve_merged_tables: bool) -> HtmlToMarkdown {
    let mut builder = HtmlToMarkdown::builder()
        .options(Options {
            bullet_list_marker: BulletListMarker::Dash,
            ul_bullet_spacing: 1,
            ..Options::default()
        })
        .add_handler(vec!["div"], div_handler)
        .add_handler(vec!["style", "script"], skip_handler)
        .add_handler(vec!["pre"], pre_handler)
        .add_handler(vec!["details"], details_handler)
        .add_handler(vec!["summary"], summary_handler)
        .add_handler(vec!["span"], span_handler);

    if preserve_merged_tables {
        builder = builder.add_handler(vec!["table"], table_handler_preserve_merged);
    } else {
        builder = builder.add_handler(vec!["table"], table_handler_unwrap_single_cell);
    }
    builder.build()
}

// ── Handlers ───────────────────────────────────────────────────────

fn span_handler(handlers: &dyn Handlers, element: Element) -> Option<HandlerResult> {
    let content = handlers.walk_children(element.node).content;

    if span_has_style_text_decoration_line_through(element) {
        return Some(format!("~~{content}~~").into());
    }

    // Default for plain <span>: walk children transparently.
    Some(content.into())
}

fn div_handler(handlers: &dyn Handlers, element: Element) -> Option<HandlerResult> {
    let class = class_of(&element);

    // Confluence's rendered expand UI: surrounding container becomes
    // <details>, its expand-control wrapper supplies the summary, and the
    // expand-content holds the body. The storage-format preprocess pass also
    // emits ready-made <details>/<summary>, which is handled by the dedicated
    // handlers below.
    if class.contains("expand-container") {
        let title = find_text_in_class(&element, "expand-control-text").unwrap_or_default();
        let body = render_class_children(handlers, &element, "expand-content")
            .unwrap_or_else(|| handlers.walk_children(element.node).content);
        return Some(
            format!(
                "\n\n<details>\n<summary>{}</summary>\n\n{}\n</details>\n\n",
                title.trim(),
                body.trim()
            )
            .into(),
        );
    }
    if class.contains("expand-control") {
        // Suppress: text content is consumed by the parent expand-container.
        return Some(String::new().into());
    }

    if class.contains("confluence-information-macro") {
        let alert = alert_type_from_class(&class).unwrap_or("NOTE");
        let body = render_class_children(handlers, &element, "confluence-information-macro-body")
            .unwrap_or_else(|| handlers.walk_children(element.node).content);
        return Some(render_alert(alert, body.trim()).into());
    }
    if class.contains("confluence-information-macro-body") {
        // Inner body div is handled by its parent. If it ever appears at the
        // top level, fall through to a transparent walk.
        return Some(handlers.walk_children(element.node).content.into());
    }

    // Default for plain <div>: walk children transparently.
    Some(handlers.walk_children(element.node).content.into())
}

fn skip_handler(_handlers: &dyn Handlers, _element: Element) -> Option<HandlerResult> {
    Some(String::new().into())
}

fn pre_handler(handlers: &dyn Handlers, element: Element) -> Option<HandlerResult> {
    let class = class_of(&element);
    if class.contains("syntaxhighlighter-pre") {
        let params = attr_of(&element, "data-syntaxhighlighter-params").unwrap_or_default();
        let brush = parse_brush(&params).unwrap_or_default();
        let code = raw_text_of(element.node);
        let code = code.trim_end_matches('\n');
        return Some(format!("\n\n```{brush}\n{code}\n```\n\n").into());
    }
    handlers.fallback(element)
}

fn details_handler(handlers: &dyn Handlers, element: Element) -> Option<HandlerResult> {
    let inner = handlers.walk_children(element.node).content;
    Some(format!("\n\n<details>\n{}\n</details>\n\n", inner.trim()).into())
}

fn summary_handler(handlers: &dyn Handlers, element: Element) -> Option<HandlerResult> {
    let inner = handlers.walk_children(element.node).content;
    Some(format!("<summary>{}</summary>\n\n", inner.trim()).into())
}

fn table_handler_unwrap_single_cell(
    handlers: &dyn Handlers,
    element: Element,
) -> Option<HandlerResult> {
    if let Some(cell) = table_single_cell(element.node) {
        let content = handlers.walk_children(&cell).content;
        Some(format!("\n\n{}\n\n", content.trim()).into())
    } else {
        handlers.fallback(element)
    }
}

/// In `Default` mode, tables with colspan/rowspan are preserved as raw HTML.
fn table_handler_preserve_merged(
    handlers: &dyn Handlers,
    element: Element,
) -> Option<HandlerResult> {
    if let Some(cell) = table_single_cell(element.node) {
        let content = handlers.walk_children(&cell).content;
        Some(format!("\n\n{}\n\n", content.trim()).into())
    } else if node_has_merged_cells(element.node) || node_has_nested_table(element.node) {
        let html = serialize_node_to_html(element.node);
        Some(format!("\n\n{html}\n\n").into())
    } else {
        handlers.fallback(element)
    }
}

fn table_single_cell(node: &std::rc::Rc<htmd::Node>) -> Option<std::rc::Rc<htmd::Node>> {
    let rows = direct_table_rows(node);
    if rows.len() != 1 {
        return None;
    }

    let cells = direct_row_cells(&rows[0]);
    if cells.len() == 1 {
        Some(cells[0].clone())
    } else {
        None
    }
}

fn direct_table_rows(node: &std::rc::Rc<htmd::Node>) -> Vec<std::rc::Rc<htmd::Node>> {
    let mut rows = Vec::new();
    for child in node.children.borrow().iter() {
        if element_name_is(child, "tr") {
            rows.push(child.clone());
        } else if element_name_is(child, "thead")
            || element_name_is(child, "tbody")
            || element_name_is(child, "tfoot")
        {
            for section_child in child.children.borrow().iter() {
                if element_name_is(section_child, "tr") {
                    rows.push(section_child.clone());
                }
            }
        }
    }
    rows
}

fn direct_row_cells(node: &std::rc::Rc<htmd::Node>) -> Vec<std::rc::Rc<htmd::Node>> {
    node.children
        .borrow()
        .iter()
        .filter(|child| element_name_is(child, "td") || element_name_is(child, "th"))
        .cloned()
        .collect()
}

fn element_name_is(node: &std::rc::Rc<htmd::Node>, expected: &str) -> bool {
    matches!(&node.data, NodeData::Element { name, .. } if &*name.local == expected)
}

fn span_has_style_text_decoration_line_through(element: Element) -> bool {
    let attrs = element.attrs.iter();

    for attr in attrs {
        if attr.name.local.as_ref() == "style" {
            let style_str = attr.value.as_ref();
            let style_result = StyleAttribute::parse(style_str, ParserOptions::default());

            if let Ok(style) = style_result {
                return style.declarations.declarations.iter().any(|decl| {
                    matches!(
                        decl,
                        Property::TextDecoration(text_decoration, _)
                            if text_decoration
                                .line
                                .contains(TextDecorationLine::LineThrough)
                    )
                });
            }
        }
    }

    false
}

/// Check whether a DOM subtree contains any element with colspan or rowspan > 1.
fn node_has_merged_cells(node: &std::rc::Rc<htmd::Node>) -> bool {
    if let NodeData::Element { attrs, .. } = &node.data {
        for attr in attrs.borrow().iter() {
            let name = &*attr.name.local;
            if (name == "colspan" || name == "rowspan") && attr.value.trim() != "1" {
                return true;
            }
        }
    }
    for child in node.children.borrow().iter() {
        if node_has_merged_cells(child) {
            return true;
        }
    }
    false
}

/// Check whether a table subtree contains another `<table>` element.
fn node_has_nested_table(node: &std::rc::Rc<htmd::Node>) -> bool {
    fn has_descendant_table(node: &std::rc::Rc<htmd::Node>) -> bool {
        for child in node.children.borrow().iter() {
            if let NodeData::Element { name, .. } = &child.data
                && &*name.local == "table"
            {
                return true;
            }
            if has_descendant_table(child) {
                return true;
            }
        }
        false
    }

    has_descendant_table(node)
}

/// Serialize an html5ever DOM node back to an HTML string.
fn serialize_node_to_html(node: &std::rc::Rc<htmd::Node>) -> String {
    // Serialize an HTML node tree with indentation for human readability.
    // Table structural elements each get their own line; cell content stays inline.
    fn write_node(node: &std::rc::Rc<htmd::Node>, buf: &mut String, depth: usize) {
        match &node.data {
            NodeData::Element { name, attrs, .. } => {
                let tag = name.local.as_ref();
                // Block-level table elements: placed on their own indented line.
                let is_block = matches!(
                    tag,
                    "table" | "colgroup" | "col" | "thead" | "tbody" | "tfoot" | "tr" | "th" | "td"
                );
                // Container elements whose children are all block-level:
                // closing tag gets its own indented line too.
                let is_container = matches!(
                    tag,
                    "table" | "colgroup" | "thead" | "tbody" | "tfoot" | "tr"
                );
                let indent = "  ".repeat(depth);
                if is_block {
                    buf.push('\n');
                    buf.push_str(&indent);
                }
                buf.push('<');
                buf.push_str(tag);
                for attr in attrs.borrow().iter() {
                    buf.push(' ');
                    buf.push_str(&attr.name.local);
                    buf.push_str("=\"");
                    for ch in attr.value.chars() {
                        match ch {
                            '"' => buf.push_str("&quot;"),
                            '&' => buf.push_str("&amp;"),
                            '<' => buf.push_str("&lt;"),
                            _ => buf.push(ch),
                        }
                    }
                    buf.push('"');
                }
                buf.push('>');
                let child_depth = if is_block { depth + 1 } else { depth };
                for child in node.children.borrow().iter() {
                    write_node(child, buf, child_depth);
                }
                if is_container {
                    buf.push('\n');
                    buf.push_str(&indent);
                }
                buf.push_str("</");
                buf.push_str(tag);
                buf.push('>');
            }
            NodeData::Text { contents } => {
                for ch in contents.borrow().chars() {
                    match ch {
                        '&' => buf.push_str("&amp;"),
                        '<' => buf.push_str("&lt;"),
                        '>' => buf.push_str("&gt;"),
                        _ => buf.push(ch),
                    }
                }
            }
            _ => {
                for child in node.children.borrow().iter() {
                    write_node(child, buf, depth);
                }
            }
        }
    }
    let mut result = String::new();
    write_node(node, &mut result, 0);
    // The root table tag emits a leading newline as part of block formatting; strip it.
    result.trim_start_matches('\n').to_string()
}

// ── Helpers ────────────────────────────────────────────────────────

fn class_of(element: &Element<'_>) -> String {
    attr_of(element, "class").unwrap_or_default()
}

fn attr_of(element: &Element<'_>, name: &str) -> Option<String> {
    element
        .attrs
        .iter()
        .find(|a| &*a.name.local == name)
        .map(|a| a.value.to_string())
}

fn alert_type_from_class(class: &str) -> Option<&'static str> {
    // Order matters: `confluence-information-macro-information` must match
    // before the prefix-only `confluence-information-macro` check.
    if class.contains("confluence-information-macro-information") {
        Some("IMPORTANT")
    } else if class.contains("confluence-information-macro-note") {
        Some("WARNING")
    } else if class.contains("confluence-information-macro-tip") {
        Some("TIP")
    } else if class.contains("confluence-information-macro-warning") {
        Some("CAUTION")
    } else if class.contains("confluence-information-macro-panel") {
        Some("NOTE")
    } else {
        None
    }
}

fn render_alert(alert: &str, body: &str) -> String {
    let mut s = String::new();
    s.push_str("\n\n> [!");
    s.push_str(alert);
    s.push_str("]\n");
    for line in body.lines() {
        if line.is_empty() {
            s.push_str(">\n");
        } else {
            s.push_str("> ");
            s.push_str(line);
            s.push('\n');
        }
    }
    s.push('\n');
    s
}

fn parse_brush(params: &str) -> Option<String> {
    // `data-syntaxhighlighter-params` looks like `brush: rust; gutter: false`.
    for piece in params.split(';') {
        let mut kv = piece.splitn(2, ':');
        let key = kv.next()?.trim();
        let value = kv.next()?.trim();
        if key.eq_ignore_ascii_case("brush") {
            return Some(value.to_owned());
        }
    }
    None
}

/// Search descendants for the first element whose class attribute contains
/// `class_token`, then return its rendered children content.
fn render_class_children(
    handlers: &dyn Handlers,
    element: &Element<'_>,
    class_token: &str,
) -> Option<String> {
    let node = find_descendant_by_class(element.node, class_token)?;
    Some(handlers.walk_children(&node).content)
}

fn find_text_in_class(element: &Element<'_>, class_token: &str) -> Option<String> {
    let node = find_descendant_by_class(element.node, class_token)?;
    Some(raw_text_of(&node))
}

fn find_descendant_by_class(
    node: &std::rc::Rc<htmd::Node>,
    class_token: &str,
) -> Option<std::rc::Rc<htmd::Node>> {
    for child in node.children.borrow().iter() {
        if let NodeData::Element { attrs, .. } = &child.data {
            let has_class = attrs.borrow().iter().any(|a| {
                &*a.name.local == "class" && a.value.split_whitespace().any(|c| c == class_token)
            });
            if has_class {
                return Some(child.clone());
            }
        }
        if let Some(found) = find_descendant_by_class(child, class_token) {
            return Some(found);
        }
    }
    None
}

fn raw_text_of(node: &std::rc::Rc<htmd::Node>) -> String {
    let mut out = String::new();
    fn walk(node: &std::rc::Rc<htmd::Node>, out: &mut String) {
        match &node.data {
            NodeData::Text { contents } => out.push_str(&contents.borrow()),
            _ => {
                for c in node.children.borrow().iter() {
                    walk(c, out);
                }
            }
        }
    }
    walk(node, &mut out);
    out
}

// ── Heading-link preprocess ───────────────────────────────────────

static HEADING_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?is)<h[1-6]\b([^>]*)>(.*?)</h[1-6]>"#).unwrap());
static ID_ATTR_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?is)\bid\s*=\s*"([^"]*)"|\bid\s*=\s*'([^']*)'"#).unwrap());
static ANCHOR_HREF_DOUBLE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r##"(?is)<a\b([^>]*?)\bhref\s*=\s*"(#[^"]*)"([^>]*)>"##).unwrap());
static ANCHOR_HREF_SINGLE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r##"(?is)<a\b([^>]*?)\bhref\s*=\s*'(#[^']*)'([^>]*)>"##).unwrap());

fn rewrite_internal_heading_links(html: &str) -> String {
    let id_to_slug = heading_id_to_markdown_slug_map(html);
    if id_to_slug.is_empty() {
        return html.to_string();
    }

    let rewritten = ANCHOR_HREF_DOUBLE_RE.replace_all(html, |caps: &regex::Captures<'_>| {
        rewrite_anchor_href(&caps[0], &caps[1], &caps[2], &caps[3], '"', &id_to_slug)
    });
    ANCHOR_HREF_SINGLE_RE
        .replace_all(&rewritten, |caps: &regex::Captures<'_>| {
            rewrite_anchor_href(&caps[0], &caps[1], &caps[2], &caps[3], '\'', &id_to_slug)
        })
        .into_owned()
}

fn heading_id_to_markdown_slug_map(html: &str) -> HashMap<String, String> {
    let mut id_to_slug = HashMap::new();
    let mut slug_counts: HashMap<String, usize> = HashMap::new();

    for caps in HEADING_RE.captures_iter(html) {
        let Some(id) = attr_value(&caps[1], &ID_ATTR_RE) else {
            continue;
        };
        let text = strip_tags(&caps[2]);
        let slug = unique_markdown_heading_slug(&text, &mut slug_counts);
        id_to_slug.insert(id, slug);
    }

    id_to_slug
}

fn attr_value(attrs: &str, regex: &Regex) -> Option<String> {
    let caps = regex.captures(attrs)?;
    caps.get(1)
        .or_else(|| caps.get(2))
        .map(|m| decode_html_attribute(m.as_str()))
}

fn rewrite_anchor_href(
    original: &str,
    before_href: &str,
    href: &str,
    after_href: &str,
    quote: char,
    id_to_slug: &HashMap<String, String>,
) -> String {
    let target = decode_html_attribute(href.trim_start_matches('#'));
    let Some(slug) = id_to_slug.get(&target) else {
        return original.to_string();
    };

    format!(
        "<a{before_href}href={quote}#{href_value}{quote}{after_href}>",
        href_value = escape_html(slug)
    )
}

fn unique_markdown_heading_slug(text: &str, slug_counts: &mut HashMap<String, usize>) -> String {
    let base = markdown_heading_slug(text);
    let count = slug_counts.entry(base.clone()).or_insert(0);
    let slug = if *count == 0 {
        base
    } else {
        format!("{base}-{count}")
    };
    *count += 1;
    slug
}

fn markdown_heading_slug(text: &str) -> String {
    let mut slug = String::new();

    for ch in text.trim().to_lowercase().chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '-' {
            slug.push(ch);
        } else if ch.is_whitespace() {
            slug.push('-');
        }
    }

    if slug.is_empty() {
        "section".to_string()
    } else {
        slug
    }
}

// ── Table-conversion preprocess ────────────────────────────────────

/// Regex matching a `<table>…</table>` block (non-greedy, case-insensitive).
static TABLE_BLOCK_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?is)<table\b([^>]*)>(.*?)</table>").unwrap());
static TABLE_TAG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?is)</?table\b[^>]*>").unwrap());

/// Detect whether a table HTML fragment contains `colspan` or `rowspan` attrs.
fn html_has_merged_cells(table_html: &str) -> bool {
    static MERGE_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?i)(colspan|rowspan)\s*=\s*["']?\d"#).unwrap());
    MERGE_RE.is_match(table_html)
}

fn is_close_table_tag(tag: &str) -> bool {
    tag.trim_start().starts_with("</")
}

/// Find all complete top-level `<table>...</table>` byte ranges in `html`.
fn find_top_level_table_ranges(html: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut depth = 0usize;
    let mut current_start = None;

    for m in TABLE_TAG_RE.find_iter(html) {
        let tag = m.as_str();
        if is_close_table_tag(tag) {
            if depth == 0 {
                continue;
            }
            depth -= 1;
            if depth == 0 {
                if let Some(start) = current_start {
                    ranges.push((start, m.end()));
                }
                current_start = None;
            }
        } else {
            if depth == 0 {
                current_start = Some(m.start());
            }
            depth += 1;
        }
    }

    ranges
}

/// Find the first direct nested `<table>...</table>` range inside `table_html`.
/// The returned range is a byte range relative to `table_html`.
fn first_direct_nested_table_range(table_html: &str) -> Option<(usize, usize)> {
    let mut depth = 0usize;
    let mut direct_start = None;

    for m in TABLE_TAG_RE.find_iter(table_html) {
        let tag = m.as_str();
        if is_close_table_tag(tag) {
            if depth == 0 {
                continue;
            }
            if depth == 2 {
                let start = direct_start?;
                return Some((start, m.end()));
            }
            depth -= 1;
        } else {
            depth += 1;
            if depth == 2 {
                direct_start = Some(m.start());
            }
        }
    }

    None
}

/// Extract nested tables from a single table HTML block.
///
/// Returns:
/// - outer table html where nested tables are replaced by markers
/// - list of `(marker, table_html_without_nested_tables)` in extraction order
fn extract_nested_tables_from_table(
    table_html: &str,
    marker_counter: &mut usize,
) -> (String, Vec<(String, String)>) {
    let mut outer = table_html.to_string();
    let mut extracted: Vec<(String, String)> = Vec::new();

    while let Some((start, end)) = first_direct_nested_table_range(&outer) {
        let nested = outer[start..end].to_string();
        let (nested_outer, nested_children) =
            extract_nested_tables_from_table(&nested, marker_counter);

        *marker_counter += 1;
        let marker = format!("(*{})", *marker_counter);

        outer.replace_range(start..end, &marker);
        extracted.push((marker, nested_outer));
        extracted.extend(nested_children);
    }

    (outer, extracted)
}

fn convert_table_fragment_always(table_html: &str) -> String {
    let expanded = expand_merged_cells(table_html);
    let promoted = promote_first_row_to_thead(&expanded);
    let converter = build_converter(false);
    post_process(&converter.convert(&promoted).unwrap_or_default())
        .trim()
        .to_string()
}

/// In always mode, replace tables containing nested tables with tokens and
/// prepare markdown replacements:
///
/// 1. Outer table markdown (nested cells replaced by markers)
/// 2. Extracted nested table markdowns appended after the outer table
fn preprocess_nested_tables_always(html: &str) -> (String, Vec<(String, String)>) {
    let ranges = find_top_level_table_ranges(html);
    if ranges.is_empty() {
        return (html.to_string(), Vec::new());
    }

    let mut out = String::with_capacity(html.len());
    let mut replacements: Vec<(String, String)> = Vec::new();
    let mut cursor = 0usize;
    let mut marker_counter = 0usize;
    let mut token_counter = 0usize;

    for (start, end) in ranges {
        out.push_str(&html[cursor..start]);
        let table_html = &html[start..end];

        if first_direct_nested_table_range(table_html).is_none() {
            out.push_str(table_html);
        } else {
            let (outer, extracted) =
                extract_nested_tables_from_table(table_html, &mut marker_counter);

            let mut snippet = convert_table_fragment_always(&outer);
            for (marker, nested_table_html) in extracted {
                let nested_md = convert_table_fragment_always(&nested_table_html);
                snippet.push_str("\n\n");
                snippet.push_str(&marker);
                snippet.push_str("\n\n");
                snippet.push_str(&nested_md);
            }

            token_counter += 1;
            let token = format!("C2MDNESTEDTABLEBLOCK{token_counter}");
            out.push_str(&token);
            replacements.push((token, snippet));
        }

        cursor = end;
    }
    out.push_str(&html[cursor..]);

    (out, replacements)
}

/// In default mode, only promote tables that can safely become GFM tables.
/// Tables with merged cells or nested tables remain untouched so the table
/// handler can preserve them as readable raw HTML.
fn promote_markdown_compatible_tables_to_thead(html: &str) -> String {
    let ranges = find_top_level_table_ranges(html);
    if ranges.is_empty() {
        return html.to_string();
    }

    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;

    for (start, end) in ranges {
        out.push_str(&html[cursor..start]);
        let table_html = &html[start..end];

        if html_has_merged_cells(table_html)
            || first_direct_nested_table_range(table_html).is_some()
        {
            out.push_str(table_html);
        } else {
            out.push_str(&promote_first_row_to_thead(table_html));
        }

        cursor = end;
    }
    out.push_str(&html[cursor..]);

    out
}

// ── Always mode: expand merged cells into a flat grid ──────────────

/// For `TableConversion::Always`: expand colspan/rowspan into a flat grid of
/// cells. The top-left cell of each merged region keeps the original content;
/// the remaining spanned cells become empty.
fn expand_merged_cells(html: &str) -> String {
    TABLE_BLOCK_RE
        .replace_all(html, |caps: &regex::Captures<'_>| {
            let full = &caps[0];
            if !html_has_merged_cells(full) {
                return full.to_string();
            }
            expand_single_table(full)
        })
        .into_owned()
}

/// Expand a single `<table>…</table>` by resolving colspan/rowspan into a
/// flat grid.
fn expand_single_table(table_html: &str) -> String {
    static TR_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?is)<tr\b[^>]*>(.*?)</tr>").unwrap());
    static CELL_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?is)<(td|th)\b([^>]*)>(.*?)</(?:td|th)>").unwrap());
    static COLSPAN_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?i)colspan\s*=\s*["']?(\d+)["']?"#).unwrap());
    static ROWSPAN_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?i)rowspan\s*=\s*["']?(\d+)["']?"#).unwrap());
    static SPAN_ATTR_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?i)\s*(colspan|rowspan)\s*=\s*["']?\d+["']?"#).unwrap());

    let rows: Vec<&str> = TR_RE
        .captures_iter(table_html)
        .map(|c| c.get(1).unwrap().as_str())
        .collect();
    if rows.is_empty() {
        return table_html.to_string();
    }

    struct CellInfo {
        tag: String,
        attrs: String,
        content: String,
    }

    let num_rows = rows.len();
    let mut grid: Vec<Vec<Option<CellInfo>>> = (0..num_rows).map(|_| Vec::new()).collect();
    let mut occupied: Vec<Vec<bool>> = (0..num_rows).map(|_| Vec::new()).collect();
    let mut max_cols: usize = 0;

    for (row_idx, row_content) in rows.iter().enumerate() {
        let cells: Vec<_> = CELL_RE.captures_iter(row_content).collect();
        let mut col_idx = 0;

        if occupied[row_idx].len() < max_cols {
            occupied[row_idx].resize(max_cols, false);
        }

        for cell_caps in &cells {
            // Skip columns occupied by rowspan from above
            while col_idx < occupied[row_idx].len() && occupied[row_idx][col_idx] {
                col_idx += 1;
            }

            let tag = cell_caps[1].to_lowercase();
            let attrs_str = &cell_caps[2];
            let content = cell_caps[3].to_string();

            let colspan: usize = COLSPAN_RE
                .captures(attrs_str)
                .and_then(|c| c[1].parse().ok())
                .unwrap_or(1);
            let rowspan: usize = ROWSPAN_RE
                .captures(attrs_str)
                .and_then(|c| c[1].parse().ok())
                .unwrap_or(1);

            let clean_attrs = SPAN_ATTR_RE.replace_all(attrs_str, "").to_string();

            let end_col = col_idx + colspan;
            let end_row = row_idx + rowspan;
            if end_col > max_cols {
                max_cols = end_col;
                for occ in occupied.iter_mut() {
                    occ.resize(max_cols, false);
                }
            }

            // Mark occupied cells for merged region
            for (r, occ_row) in occupied
                .iter_mut()
                .enumerate()
                .take(end_row.min(num_rows))
                .skip(row_idx)
            {
                for c in col_idx..end_col {
                    if occ_row.len() <= c {
                        occ_row.resize(c + 1, false);
                    }
                    if !(r == row_idx && c == col_idx) {
                        occ_row[c] = true;
                    }
                }
            }

            // Place content in top-left cell
            while grid[row_idx].len() <= col_idx {
                grid[row_idx].push(None);
            }
            grid[row_idx][col_idx] = Some(CellInfo {
                tag,
                attrs: clean_attrs,
                content,
            });

            col_idx = end_col;
        }
    }

    // Fill empty cells for remaining positions
    for row in grid.iter_mut() {
        while row.len() < max_cols {
            row.push(None);
        }
    }

    // Reconstruct the table
    static TR_OPEN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?is)<tr\b[^>]*>").unwrap());
    let tr_opens: Vec<&str> = TR_OPEN_RE
        .find_iter(table_html)
        .map(|m| m.as_str())
        .collect();

    fn default_tag_for_row(row: &[Option<CellInfo>]) -> &str {
        row.iter()
            .find_map(|cell| cell.as_ref().map(|c| c.tag.as_str()))
            .unwrap_or("td")
    }

    let mut new_rows = String::new();
    for (row_idx, row) in grid.iter().enumerate() {
        let tr_open = tr_opens.get(row_idx).copied().unwrap_or("<tr>");
        let def_tag = default_tag_for_row(row);
        new_rows.push_str(tr_open);
        for cell in row.iter() {
            match cell {
                Some(info) => {
                    new_rows.push('<');
                    new_rows.push_str(&info.tag);
                    if !info.attrs.trim().is_empty() {
                        new_rows.push_str(&info.attrs);
                    }
                    new_rows.push('>');
                    new_rows.push_str(&info.content);
                    new_rows.push_str("</");
                    new_rows.push_str(&info.tag);
                    new_rows.push('>');
                }
                None => {
                    new_rows.push('<');
                    new_rows.push_str(def_tag);
                    new_rows.push_str("></");
                    new_rows.push_str(def_tag);
                    new_rows.push('>');
                }
            }
        }
        new_rows.push_str("</tr>");
    }

    // Find content before the first <tr> (table open tag, colgroup, tbody open, etc.)
    static BEFORE_FIRST_TR: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?is)^(<table\b[^>]*>)(.*?)(<tr\b)").unwrap());

    if let Some(cap) = BEFORE_FIRST_TR.captures(table_html) {
        let prefix = format!("{}{}", &cap[1], &cap[2]);
        let has_tbody = table_html.contains("</tbody>");
        let suffix = if has_tbody {
            "</tbody></table>"
        } else {
            "</table>"
        };
        format!("{prefix}{new_rows}{suffix}")
    } else {
        table_html.to_string()
    }
}

/// For `TableConversion::Always`: ensure every `<table>` has a `<thead>` by
/// promoting its first `<tr>` (converting `<td>`s to `<th>`s) if no header is
/// present. Without this, htmd falls back to raw HTML for headerless tables.
fn promote_first_row_to_thead(html: &str) -> String {
    static THEAD_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?is)<thead\b").unwrap());
    static FIRST_TR_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?is)<tr\b[^>]*>.*?</tr>").unwrap());

    TABLE_BLOCK_RE
        .replace_all(html, |caps: &regex::Captures<'_>| {
            let attrs = &caps[1];
            let inner = &caps[2];
            if THEAD_RE.is_match(inner) {
                return caps[0].to_string();
            }
            let Some(tr) = FIRST_TR_RE.find(inner) else {
                return caps[0].to_string();
            };
            let promoted = tr.as_str().replace("<td", "<th").replace("</td>", "</th>");
            let before = &inner[..tr.start()];
            let after = &inner[tr.end()..];
            format!("<table{attrs}>{before}<thead>{promoted}</thead>{after}</table>")
        })
        .into_owned()
}

// ── Post-processing ────────────────────────────────────────────────

fn post_process(s: &str) -> String {
    // Collapse 3+ consecutive blank lines into 2 and trim trailing whitespace.
    static BLANKS_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\n{3,}").unwrap());
    let collapsed = BLANKS_RE.replace_all(s, "\n\n");
    let task_markers = collapsed.replace(r"\[x\]", "[x]").replace(r"\[ \]", "[ ]");
    task_markers.trim().to_owned() + "\n"
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn td(html: &str) -> String {
        convert_to_md(html, ConvertOptions::default())
    }

    fn td_always(html: &str) -> String {
        convert_to_md(
            html,
            ConvertOptions {
                table_conversion: TableConversion::Always,
            },
        )
    }

    #[test]
    fn renders_headings() {
        let md = td("<h1>Title</h1><h2>Sub</h2>");
        assert!(md.contains("# Title"));
        assert!(md.contains("## Sub"));
    }

    #[test]
    fn rewrites_confluence_toc_links_to_markdown_heading_slugs() {
        let html = r##"<div class="toc-macro"><ul><li><a href="#SamplePage-背景">背景</a></li><li><a href="#SamplePage-4.2.サンプル章(テスト)/Samplesection(Test)">4.2.サンプル章(テスト)/Sample section (Test)</a></li></ul></div><h1 id="SamplePage-背景">背景</h1><h2 id="SamplePage-4.2.サンプル章(テスト)/Samplesection(Test)">4.2.サンプル章(テスト)/Sample section (Test)</h2>"##;
        let md = td(html);

        assert!(md.contains("[背景](#背景)"), "{md}");
        assert!(md.contains(
            "[4.2.サンプル章(テスト)/Sample section (Test)](#42サンプル章テストsample-section-test)"
        ), "{md}");
        assert!(!md.contains("<a id="), "{md}");
        assert!(!md.contains("#SamplePage-背景"), "{md}");
        assert!(md.contains("## 4.2.サンプル章"), "{md}");
    }

    #[test]
    fn rewrites_duplicate_heading_links_with_markdown_suffixes() {
        let html = r##"<ul><li><a href="#page-Repeat">first</a></li><li><a href="#page-Repeat.1">second</a></li></ul><h2 id="page-Repeat">Repeat</h2><h2 id="page-Repeat.1">Repeat</h2>"##;
        let md = td(html);

        assert!(md.contains("[first](#repeat)"), "{md}");
        assert!(md.contains("[second](#repeat-1)"), "{md}");
    }

    #[test]
    fn renders_paragraph_and_inline() {
        let md = td("<p>Hello <strong>bold</strong> and <em>italic</em>.</p>");
        assert!(md.contains("**bold**"));
        assert!(md.contains("_italic_") || md.contains("*italic*"));
    }

    #[test]
    fn renders_link() {
        let md = td(r#"<p><a href="https://example.com">Example</a></p>"#);
        assert!(md.contains("[Example](https://example.com)"));
    }

    #[test]
    fn renders_unordered_list() {
        let md = td("<ul><li>a</li><li>b</li></ul>");
        assert!(md.contains("- a"), "{md}");
        assert!(md.contains("- b"), "{md}");
    }

    #[test]
    fn skips_style_and_script_elements() {
        let html = r##"<p><style type="text/css">/*<![CDATA[*/ div.rbtoc1 {padding: 0px;} /*]]>*/</style><div class="toc-macro"><ul><li><a href="#one">One</a></li></ul></div><script>alert("x")</script></p>"##;
        let md = td(html);

        assert!(!md.contains("CDATA"), "{md}");
        assert!(!md.contains("rbtoc1"), "{md}");
        assert!(!md.contains("alert"), "{md}");
        assert!(md.contains("[One](#one)"), "{md}");
    }

    #[test]
    fn renders_code_block_with_language() {
        let md = td(r#"<pre><code class="language-rust">fn main() {}</code></pre>"#);
        assert!(md.contains("```rust"));
        assert!(md.contains("fn main()"));
    }

    #[test]
    fn syntax_highlighter_pre_extracts_brush() {
        let md = td(
            r#"<pre class="syntaxhighlighter-pre" data-syntaxhighlighter-params="brush: rust; gutter: false">fn main() {}</pre>"#,
        );
        assert!(md.contains("```rust"), "{md}");
        assert!(md.contains("fn main()"));
    }

    #[test]
    fn alert_info_to_important() {
        let md = td(
            r#"<div class="confluence-information-macro confluence-information-macro-information"><div class="confluence-information-macro-body"><p>Hi.</p></div></div>"#,
        );
        assert!(md.contains("> [!IMPORTANT]"), "{md}");
        assert!(md.contains("> Hi."), "{md}");
    }

    #[test]
    fn alert_warning_to_caution() {
        let md = td(
            r#"<div class="confluence-information-macro confluence-information-macro-warning"><div class="confluence-information-macro-body"><p>Beware.</p></div></div>"#,
        );
        assert!(md.contains("> [!CAUTION]"), "{md}");
    }

    #[test]
    fn alert_note_to_warning() {
        let md = td(
            r#"<div class="confluence-information-macro confluence-information-macro-note"><div class="confluence-information-macro-body"><p>Note.</p></div></div>"#,
        );
        assert!(md.contains("> [!WARNING]"), "{md}");
    }

    #[test]
    fn details_block_preserved_as_html() {
        let md = td("<details><summary>Click</summary><p>Hidden.</p></details>");
        assert!(md.contains("<details>"));
        assert!(md.contains("<summary>Click</summary>"));
        assert!(md.contains("Hidden."));
    }

    #[test]
    fn always_table_mode_unwraps_single_cell_table() {
        let md = td_always("<table><tbody><tr><td>Only cell</td></tr></tbody></table>");
        assert_eq!(md, "Only cell\n");
    }

    #[test]
    fn default_mode_unwraps_single_cell_table_with_block_content() {
        let html = r#"<table><tbody><tr><td><p>Diagram</p><pre><code class="language-plantuml">@startuml
A -> B
@enduml</code></pre></td></tr></tbody></table>"#;
        let md = td(html);

        assert!(md.contains("Diagram"), "{md}");
        assert!(md.contains("```plantuml"), "{md}");
        assert!(md.contains("@startuml"), "{md}");
        assert!(!md.contains("| Diagram"), "{md}");
        assert!(!md.contains("<table"), "{md}");
    }

    #[test]
    fn default_mode_preserves_merged_table_as_html() {
        let html = r#"<table><tbody><tr><th>A</th><th>B</th><th>C</th></tr><tr><td colspan="2">merged</td><td>c</td></tr></tbody></table>"#;
        let md = td(html);
        assert!(md.contains("<table"), "Expected raw HTML table, got:\n{md}");
        assert!(
            md.contains("colspan"),
            "Expected colspan preserved, got:\n{md}"
        );
    }

    #[test]
    fn default_mode_converts_non_merged_table_normally() {
        let html = r#"<table><thead><tr><th>A</th><th>B</th></tr></thead><tbody><tr><td>1</td><td>2</td></tr></tbody></table>"#;
        let md = td(html);
        assert!(
            md.contains("| A | B |"),
            "Expected markdown table, got:\n{md}"
        );
        assert!(
            md.contains("| 1 | 2 |"),
            "Expected markdown table, got:\n{md}"
        );
    }

    #[test]
    fn default_mode_promotes_confluence_td_header_table() {
        let html = r#"<table class="wrapped confluenceTable"><tbody><tr><td><p><strong>No.</strong></p></td><td><p><strong>用語/Term</strong></p></td><td><p><strong>説明/Explanation</strong></p></td></tr><tr><td><p>例/e.g.</p></td><td><p>sample-tool</p></td><td><p>サンプルデータを処理するツール。<br/>Tool for processing sample data.</p></td></tr><tr><td><p>1</p></td><td><p>サンプル管理機能</p></td><td><p>入力されたサンプル情報をもとに処理を実行する。</p></td></tr></tbody></table>"#;
        let md = td(html);

        let table_lines: Vec<&str> = md.lines().filter(|line| line.starts_with('|')).collect();
        assert_eq!(table_lines.len(), 4, "{md}");
        assert!(table_lines[0].contains("**No.**"), "{md}");
        assert!(table_lines[0].contains("**用語/Term**"), "{md}");
        assert!(table_lines[0].contains("**説明/Explanation**"), "{md}");
        assert!(table_lines[2].contains("例/e.g."), "{md}");
        assert!(table_lines[2].contains("sample-tool"), "{md}");
        assert!(table_lines[3].contains("サンプル管理機能"), "{md}");
        assert!(!md.contains("<table"), "{md}");
    }

    #[test]
    fn always_mode_expands_colspan() {
        let html = r#"<table><tbody><tr><th>A</th><th>B</th><th>C</th></tr><tr><td colspan="2">merged</td><td>c</td></tr></tbody></table>"#;
        let md = td_always(html);
        assert!(md.contains("| A"), "Expected header, got:\n{md}");
        assert!(
            md.contains("| merged |"),
            "Expected merged in cell, got:\n{md}"
        );
    }

    #[test]
    fn always_mode_expands_rowspan() {
        let html = r#"<table><tbody><tr><th>A</th><th>B</th></tr><tr><td rowspan="2">span</td><td>x</td></tr><tr><td>y</td></tr></tbody></table>"#;
        let md = td_always(html);
        assert!(md.contains("| span"), "Expected span in cell, got:\n{md}");
        assert!(md.contains("| y |"), "Expected y in cell, got:\n{md}");
    }

    #[test]
    fn always_mode_expands_confluence_merged_table() {
        let html = r#"<table class="confluenceTable"><tbody><tr><th>列１</th><th>列２</th><th>列３</th><th>列４</th><th>列５</th></tr><tr><td colspan="2">１</td><td>２</td><td colspan="2">３</td></tr><tr><td>４</td><td colspan="3">５</td><td>６</td></tr><tr><td>７</td><td rowspan="2">８</td><td>９</td><td colspan="2" rowspan="2">１０</td></tr><tr><td>１１</td><td>１２</td></tr></tbody></table>"#;
        let md = td_always(html);
        assert!(md.contains("| 列１"), "Expected header, got:\n{md}");
        let lines: Vec<&str> = md.lines().filter(|l| l.starts_with('|')).collect();
        assert_eq!(
            lines.len(),
            6,
            "Expected 6 table lines (header+sep+4 data), got:\n{md}"
        );
    }

    #[test]
    fn default_mode_preserves_merged_table_with_indented_html() {
        let html = r#"<table><tbody><tr><th>A</th><th>B</th></tr><tr><td colspan="2">merged</td></tr></tbody></table>"#;
        let md = td(html);
        // The table is rendered as indented HTML (leading whitespace may be trimmed by htmd).
        assert!(md.contains("<table>"), "Expected <table>, got:\n{md}");
        assert!(
            md.contains("\n  <tbody>"),
            "Expected <tbody> indented, got:\n{md}"
        );
        assert!(
            md.contains("\n    <tr>"),
            "Expected <tr> indented, got:\n{md}"
        );
        // Cell opening tag indented; content stays on the same line as the tag.
        assert!(
            md.contains("\n      <th>A</th>"),
            "Expected <th> cell on its own line, got:\n{md}"
        );
        assert!(
            md.contains("\n      <td colspan=\"2\">merged</td>"),
            "Expected <td> cell on its own line, got:\n{md}"
        );
    }

    #[test]
    fn default_mode_preserves_nested_table_as_html() {
        let html = r#"<table><tbody><tr><th>A</th><th>B</th></tr><tr><td>outer</td><td><table><tbody><tr><th>X</th></tr><tr><td>1</td></tr></tbody></table></td></tr></tbody></table>"#;
        let md = td(html);
        assert!(md.contains("<table"), "Expected raw HTML table, got:\n{md}");
        assert_eq!(
            md.matches("<table").count(),
            2,
            "Expected nested table HTML preserved, got:\n{md}"
        );
        assert!(
            md.contains("\n      <td>outer</td>"),
            "Expected pretty-printed outer cell, got:\n{md}"
        );
    }

    #[test]
    fn always_mode_extracts_nested_tables_with_unique_markers() {
        let html = r#"<table><tbody><tr><th>A</th><th>B</th></tr><tr><td><table><tbody><tr><th>X</th></tr><tr><td>1</td></tr></tbody></table></td><td><table><tbody><tr><th>Y</th></tr><tr><td>2</td></tr></tbody></table></td></tr></tbody></table>"#;
        let md = td_always(html);

        assert!(
            md.contains("| A"),
            "Expected outer markdown table, got:\n{md}"
        );
        assert!(
            md.contains("(\\*1)"),
            "Expected first marker in outer table, got:\n{md}"
        );
        assert!(
            md.contains("(\\*2)"),
            "Expected second marker in outer table, got:\n{md}"
        );

        assert!(
            md.contains("\n(*1)\n"),
            "Expected marker section for first nested table, got:\n{md}"
        );
        assert!(
            md.contains("\n(*2)\n"),
            "Expected marker section for second nested table, got:\n{md}"
        );
        assert!(
            md.contains("| X |"),
            "Expected first nested markdown table, got:\n{md}"
        );
        assert!(
            md.contains("| Y |"),
            "Expected second nested markdown table, got:\n{md}"
        );
    }

    /// [Text effects](https://confluence.atlassian.com/doc/confluence-storage-format-790796544.html#ConfluenceStorageFormat-Texteffects)
    #[test]
    fn it_should_render_strikethrough() {
        let actual = td("<span style=\"text-decoration: line-through;\">strikethrough</span>");
        let expected = "~~strikethrough~~\n";
        assert_eq!(actual, expected);
    }
}
