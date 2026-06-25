//! Integration tests that exercise the modules against the
//! `tests/input/confluence_content.json` fixture. The fixture mirrors the shape
//! of a Confluence REST API page response.

use std::sync::OnceLock;

use serde_json::Value;

use confluence2md::drawio::{extract_drawio_diagram_names, replace_drawio_img_srcs};
use confluence2md::html::{ConvertOptions, TableConversion, convert_html_to_markdown};
use confluence2md::plantuml::{extract_plantuml_sources, replace_plantuml_imgs_with_code};
use confluence2md::utils::{
    apply_task_list_statuses, preprocess_confluence_macros, sanitize_file_name,
};

static FIXTURE: OnceLock<Value> = OnceLock::new();

fn fixture() -> &'static Value {
    FIXTURE.get_or_init(|| {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/input/confluence_content.json"
        );
        let raw = std::fs::read_to_string(path).expect("fixture readable");
        serde_json::from_str(&raw).expect("fixture parses as JSON")
    })
}

fn export_view() -> &'static str {
    fixture()
        .pointer("/body/export_view/value")
        .and_then(Value::as_str)
        .expect("export_view.value")
}

fn storage() -> &'static str {
    fixture()
        .pointer("/body/storage/value")
        .and_then(Value::as_str)
        .expect("storage.value")
}

fn title() -> &'static str {
    fixture()
        .get("title")
        .and_then(Value::as_str)
        .expect("title")
}

// ── sanitize_file_name ────────────────────────────────────────────

#[test]
fn sanitize_file_name_handles_fixture_title() {
    let s = sanitize_file_name(title());
    assert!(!s.is_empty());
    let forbidden = ['\\', '/', ':', '*', '?', '"', '<', '>', '|'];
    for c in s.chars() {
        assert!(!forbidden.contains(&c), "forbidden char: {c}");
    }
}

// ── HTML→Markdown integration ─────────────────────────────────────

#[test]
fn converts_export_view_html_from_fixture() {
    let html = export_view();
    assert!(!html.is_empty());
    let md = convert_html_to_markdown(html, ConvertOptions::default());
    assert!(!md.is_empty());
    assert!(
        md.lines()
            .any(|l| l.starts_with("# ") || l.starts_with("## ")),
        "no heading lines"
    );
    assert!(md.contains("confluence2md"));
}

#[test]
fn output_contains_expected_section_headings() {
    let md = convert_html_to_markdown(export_view(), ConvertOptions::default());
    assert!(md.contains("背景"));
    assert!(md.contains("PlantUML"));
    assert!(md.contains("Draw.io"));
}

#[test]
fn output_preserves_table_content() {
    let md = convert_html_to_markdown(export_view(), ConvertOptions::default());
    assert!(md.contains("列１"));
    assert!(md.contains("列５"));
}

#[test]
fn storage_html_is_also_convertible() {
    let html = storage();
    assert!(!html.is_empty());
    let md = convert_html_to_markdown(html, ConvertOptions::default());
    assert!(!md.is_empty());
}

// ── draw.io against fixture ───────────────────────────────────────

#[test]
fn extract_drawio_diagram_names_from_fixture() {
    let names = extract_drawio_diagram_names(Some(storage()));
    assert!(!names.is_empty());
    assert!(names.iter().any(|n| n == "single"));
    assert!(names.iter().any(|n| n == "multi"));
}

#[test]
fn rewrite_drawio_img_srcs_in_fixture_export_html() {
    let result = replace_drawio_img_srcs(
        export_view(),
        &[
            "assets%2Fdiagram1.drawio.png".to_owned(),
            "assets%2Fdiagram2.drawio.png".to_owned(),
            "assets%2Fdiagram3.drawio.png".to_owned(),
        ],
    );
    assert!(result.contains("drawio.png"));
    assert!(!result.contains("assets%2Fdiagram1.png"));
}

// ── PlantUML against fixture ──────────────────────────────────────

#[test]
fn extract_plantuml_sources_from_fixture() {
    let sources = extract_plantuml_sources(Some(storage()));
    assert!(!sources.is_empty());
    assert!(sources[0].contains("@startuml"));
    assert!(sources[0].contains("@enduml"));
}

#[test]
fn extract_plantuml_sources_returns_expected_blocks() {
    let sources = extract_plantuml_sources(Some(storage()));
    assert_eq!(sources.len(), 2);
}

#[test]
fn rewriting_plantuml_imgs_produces_valid_fenced_blocks() {
    let sources = extract_plantuml_sources(Some(storage()));
    let rewritten = replace_plantuml_imgs_with_code(export_view(), &sources);
    let md = convert_html_to_markdown(&rewritten, ConvertOptions::default());
    let count = md.matches("@startuml").count();
    assert_eq!(count, 2, "expected 2 @startuml occurrences, got {count}");
    assert!(md.contains("```plantuml"));
}

// ── End-to-end macro preprocessing + convert_html_to_markdown ────────────────────

#[test]
fn code_macro_roundtrip_via_preprocess_and_convert_html_to_markdown() {
    let html = r#"<ac:structured-macro ac:name="code"><ac:parameter ac:name="language">rust</ac:parameter><ac:plain-text-body><![CDATA[fn main() {
    let x = 1;
}]]></ac:plain-text-body></ac:structured-macro>"#;
    let processed = preprocess_confluence_macros(html);
    let md = convert_html_to_markdown(&processed, ConvertOptions::default());
    assert!(md.contains("```rust"));
    assert!(md.contains("fn main()"));
    assert!(md.contains("let x = 1;"));
}

#[test]
fn info_macro_renders_as_important_callout() {
    let html = r#"<ac:structured-macro ac:name="info"><ac:rich-text-body><p>Important info.</p></ac:rich-text-body></ac:structured-macro>"#;
    let processed = preprocess_confluence_macros(html);
    let md = convert_html_to_markdown(&processed, ConvertOptions::default());
    assert!(md.contains("> [!IMPORTANT]"));
    assert!(md.contains("> Important info."));
}

#[test]
fn warning_macro_renders_as_warning_callout() {
    let html = r#"<ac:structured-macro ac:name="warning"><ac:rich-text-body><p>Beware.</p></ac:rich-text-body></ac:structured-macro>"#;
    let processed = preprocess_confluence_macros(html);
    let md = convert_html_to_markdown(&processed, ConvertOptions::default());
    assert!(md.contains("> [!CAUTION]"));
}

#[test]
fn expand_macro_renders_as_details_summary() {
    let html = r#"<ac:structured-macro ac:name="expand"><ac:parameter ac:name="title">Click</ac:parameter><ac:rich-text-body><p>Hidden.</p></ac:rich-text-body></ac:structured-macro>"#;
    let processed = preprocess_confluence_macros(html);
    let md = convert_html_to_markdown(&processed, ConvertOptions::default());
    assert!(md.contains("<details>"));
    assert!(md.contains("<summary>Click</summary>"));
    assert!(md.contains("Hidden."));
}

#[test]
fn inline_tasks_render_as_markdown_checkboxes() {
    let storage = r#"<ac:task-list>
<ac:task><ac:task-id>1</ac:task-id><ac:task-status>complete</ac:task-status><ac:task-body>done</ac:task-body></ac:task>
<ac:task><ac:task-id>2</ac:task-id><ac:task-status>incomplete</ac:task-status><ac:task-body>todo</ac:task-body></ac:task>
</ac:task-list>"#;
    let rendered = r#"<ul class="inline-task-list"><li data-inline-task-id="1">done</li><li data-inline-task-id="2">todo</li></ul>"#;

    let annotated = apply_task_list_statuses(rendered, storage);
    let processed = preprocess_confluence_macros(&annotated);
    let md = convert_html_to_markdown(&processed, ConvertOptions::default());

    assert!(md.contains("- [x] done"), "{md}");
    assert!(md.contains("- [ ] todo"), "{md}");
}

#[test]
fn always_table_mode_unwraps_single_cell_table() {
    let html = "<table><tbody><tr><td>Only cell</td></tr></tbody></table>";
    let md = convert_html_to_markdown(
        html,
        ConvertOptions {
            table_conversion: TableConversion::Always,
            ..ConvertOptions::default()
        },
    );
    assert_eq!(md, "Only cell\n");
}
