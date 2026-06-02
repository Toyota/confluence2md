//! `confluence2md` command-line entry point.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

use confluence2md::confluence::{
    DownloadImagesOptions, build_attachment_maps, build_http_client,
    download_images_and_rewrite_html, fetch_confluence_page, get_required_env, list_attachments,
    resolve_page_id_from_url,
};
use confluence2md::drawio::{ResolveDrawioOptions, resolve_drawio_fallbacks};
use confluence2md::export_html::{ConvertOptions, TableConversion, convert_to_md};
use confluence2md::logger::{self, parse_log_level};
use confluence2md::plantuml::{ResolvePlantUmlOptions, resolve_plantuml_fallbacks};
use confluence2md::utils::{
    apply_task_list_statuses, ensure_dir, make_assets_info, normalize_base_url,
    preprocess_confluence_macros, sanitize_file_name,
};
use tracing::{debug, error, info};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug)]
#[command(
    name = "confluence2md",
    version = VERSION,
    about = "Convert Confluence pages to clean, portable Markdown.",
    long_about = None,
        after_help = concat!(
        "Environment variables:\n",
        "  CONFLUENCE2MD_PERSONAL_ACCESS_TOKEN  personal access token\n",
        "  CONFLUENCE2MD_OUTPUT_PATH            output directory (overridden by --output-path)\n",
        "  CONFLUENCE2MD_DUMP_STATE_PATH        dump-state directory (overridden by --dump-state-path)\n",
        "  CONFLUENCE2MD_LOG_LEVEL              log level (overridden by --log-level)\n",
        "  CONFLUENCE2MD_TABLE_CONVERSION       table conversion mode (overridden by --table-conversion)\n",
        "\n",
        "Example:\n",
        "  CONFLUENCE2MD_PERSONAL_ACCESS_TOKEN=\"xxx\" \\\n",
        "  confluence2md --output-path out ",
        "'https://confluence.example.com/pages/viewpage.action?pageId=393229'",
        ),
)]
struct Cli {
    /// Directory to write the output markdown file (default: current directory).
    #[arg(long = "output-path", value_name = "DIR")]
    output_path: Option<PathBuf>,

    /// Directory to write raw API and intermediate HTML dump files.
    #[arg(long = "dump-state-path", value_name = "DIR")]
    dump_state_path: Option<PathBuf>,

    /// Log verbosity: DEBUG | INFO | WARNING | ERROR (default: INFO).
    #[arg(long = "log-level", value_name = "LEVEL")]
    log_level: Option<String>,

    /// Table conversion mode: default | always (default: default).
    #[arg(long = "table-conversion", value_name = "MODE")]
    table_conversion: Option<String>,

    /// Confluence page URL.
    page_url: Option<String>,
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        error!("{err:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    let level = if let Some(level_str) = cli
        .log_level
        .clone()
        .or_else(|| std::env::var("CONFLUENCE2MD_LOG_LEVEL").ok())
    {
        parse_log_level(&level_str).context("parsing log level")?
    } else {
        logger::LogLevel::Info
    };
    logger::init(level);

    let table_mode_str = cli
        .table_conversion
        .clone()
        .or_else(|| std::env::var("CONFLUENCE2MD_TABLE_CONVERSION").ok())
        .unwrap_or_else(|| "default".to_owned());
    let table_conversion = match table_mode_str.as_str() {
        "default" => TableConversion::Default,
        "always" => TableConversion::Always,
        other => anyhow::bail!(
            "Invalid --table-conversion value: \"{other}\". Must be \"default\" or \"always\"."
        ),
    };

    let page_url = cli.page_url.clone().ok_or_else(|| {
        anyhow::anyhow!("Missing required <pageUrl> argument. Use --help for usage.")
    })?;

    let output_dir_input = cli
        .output_path
        .clone()
        .or_else(|| {
            std::env::var("CONFLUENCE2MD_OUTPUT_PATH")
                .ok()
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from("."));
    let output_dir = absolutize_path(output_dir_input)?;
    let dump_state_dir = cli
        .dump_state_path
        .clone()
        .or_else(|| {
            std::env::var("CONFLUENCE2MD_DUMP_STATE_PATH")
                .ok()
                .map(PathBuf::from)
        })
        .map(absolutize_path)
        .transpose()?;

    let parsed_url = url::Url::parse(&page_url).context("Invalid page URL")?;
    let origin = format!(
        "{}://{}{}",
        parsed_url.scheme(),
        parsed_url.host_str().unwrap_or(""),
        parsed_url
            .port()
            .map(|p| format!(":{p}"))
            .unwrap_or_default()
    );
    let base_url = normalize_base_url(&origin);
    let env = get_required_env()?;
    let token = env.personal_access_token;

    let client = build_http_client()?;

    let page_id = match resolve_page_id_from_url(&client, &page_url, &base_url, &token).await {
        Ok(id) => id,
        Err(err) => {
            error!("failed to resolve page ID from URL: {err}");
            std::process::exit(1);
        }
    };
    debug!("Resolved page ID for \"{page_url}\": {page_id}");

    ensure_dir(&output_dir).await?;
    if let Some(dir) = &dump_state_dir {
        ensure_dir(dir).await?;
    }

    let page = fetch_confluence_page(&client, &page_id, &base_url, &token).await?;
    write_dump_state(&dump_state_dir, "content.json", &page.content_json).await?;

    let title = if page.title.is_empty() {
        format!("page-{page_id}")
    } else {
        page.title.clone()
    };
    let output_file_name = format!("{}.md", sanitize_file_name(&title));
    let output_path = output_dir.join(&output_file_name);

    write_dump_state(&dump_state_dir, "export.html", &page.export_html).await?;
    write_dump_state(
        &dump_state_dir,
        "storage.html",
        page.storage_html.as_deref().unwrap_or(""),
    )
    .await?;

    let attachments = list_attachments(&client, &page_id, &base_url, &token).await?;
    let maps = build_attachment_maps(&attachments);

    let mut html_for_markdown: String = page.export_html.clone();

    let assets_info = make_assets_info(&page_id, &title, &output_path);
    ensure_dir(&assets_info.assets_abs_dir).await?;

    let mut used_names: HashSet<String> = HashSet::new();

    let drawio_result = resolve_drawio_fallbacks(
        &client,
        ResolveDrawioOptions {
            page_id: &page_id,
            storage_html: page.storage_html.as_deref(),
            export_html: &html_for_markdown,
            attachments_by_title: &maps.by_title,
            base_url: &base_url,
            token: &token,
            assets_abs_dir: &assets_info.assets_abs_dir,
            dump_state_abs_dir: dump_state_dir.as_deref(),
            markdown_image_prefix: &assets_info.markdown_image_prefix,
            used_names: &mut used_names,
        },
    )
    .await?;
    html_for_markdown = drawio_result.html;
    write_dump_state(&dump_state_dir, "rewrite_drawio.html", &html_for_markdown).await?;

    html_for_markdown = download_images_and_rewrite_html(
        &client,
        &html_for_markdown,
        DownloadImagesOptions {
            base_url: &base_url,
            personal_access_token: &token,
            assets_abs_dir: &assets_info.assets_abs_dir,
            markdown_image_prefix: &assets_info.markdown_image_prefix,
            used_names: &mut used_names,
        },
    )
    .await?;
    write_dump_state(&dump_state_dir, "rewrite_image.html", &html_for_markdown).await?;

    let plantuml_result = resolve_plantuml_fallbacks(
        &client,
        ResolvePlantUmlOptions {
            page_id: &page_id,
            storage_html: page.storage_html.as_deref(),
            html: &html_for_markdown,
            attachments_by_title: &maps.by_title,
            base_url: &base_url,
            token: &token,
            assets_abs_dir: &assets_info.assets_abs_dir,
            markdown_image_prefix: &assets_info.markdown_image_prefix,
            used_names: &mut used_names,
        },
    )
    .await?;
    html_for_markdown = plantuml_result.html;
    write_dump_state(&dump_state_dir, "rewrite_plantuml.html", &html_for_markdown).await?;

    if let Some(storage_html) = page.storage_html.as_deref() {
        html_for_markdown = apply_task_list_statuses(&html_for_markdown, storage_html);
    }
    html_for_markdown = preprocess_confluence_macros(&html_for_markdown);
    write_dump_state(&dump_state_dir, "rewrite_macros.html", &html_for_markdown).await?;

    let markdown_body = convert_to_md(&html_for_markdown, ConvertOptions { table_conversion });

    let mut markdown = String::new();
    markdown.push_str(&format!("# {title}\n"));
    markdown.push('\n');
    markdown.push_str(&format!("- Confluence Page ID: {page_id}\n"));
    if let Some(webui) = &page.webui {
        markdown.push_str(&format!("- URL: {webui}\n"));
    }
    markdown.push_str("- HTML source used for conversion: body.export_view\n");
    markdown.push('\n');
    markdown.push_str("---\n");
    markdown.push('\n');
    markdown.push_str(&markdown_body);
    markdown.push('\n');

    tokio::fs::write(&output_path, markdown).await?;
    info!("Written: {}", output_path.display());
    info!("Assets: {}", assets_info.assets_abs_dir.display());
    if let Some(dir) = &dump_state_dir {
        info!("Dump state: {}", dir.display());
    }

    Ok(())
}

fn absolutize_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

async fn write_dump_state(dir: &Option<PathBuf>, file_name: &str, contents: &str) -> Result<()> {
    if let Some(dir) = dir {
        tokio::fs::write(dir.join(file_name), contents).await?;
    }
    Ok(())
}
