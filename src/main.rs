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
use confluence2md::drawio::{ResolveDrawioOptions, resolve_drawio_diagrams};
use confluence2md::html::{ConvertOptions, TableConversion, convert_to_md};
use confluence2md::logger::{self, parse_log_level};
use confluence2md::plantuml::{ResolvePlantUmlOptions, resolve_plantuml_diagrams};
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
        "  CONFLUENCE2MD_REMOVE_STRIKETHROUGH_TEXT  set to \"true\" to remove strikethrough text\n",
        "                                           (overridden by --remove-strikethrough-text)\n",
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

    /// Remove strikethrough text entirely instead of converting to ~~text~~.
    #[arg(long = "remove-strikethrough-text")]
    remove_strikethrough_text: bool,

    /// Confluence page URL.
    page_url: Option<String>,
}

/// Resolved configuration from CLI arguments and environment variables.
struct ResolvedConfig {
    page_url: String,
    output_dir: PathBuf,
    dump_state_dir: Option<PathBuf>,
    table_conversion: TableConversion,
    remove_strikethrough_text: bool,
}

/// Resolves all configuration from CLI arguments and environment variables.
/// Also initializes the logger as a side effect.
fn resolve_config(cli: &Cli) -> Result<ResolvedConfig> {
    let level = cli
        .log_level
        .clone()
        .or_else(|| std::env::var("CONFLUENCE2MD_LOG_LEVEL").ok())
        .map(|s| parse_log_level(&s).context("parsing log level"))
        .transpose()?
        .unwrap_or(logger::LogLevel::Info);
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

    let output_dir = cli
        .output_path
        .clone()
        .or_else(|| {
            std::env::var("CONFLUENCE2MD_OUTPUT_PATH")
                .ok()
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from("."));

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

    let remove_strikethrough_text = cli.remove_strikethrough_text
        || std::env::var("CONFLUENCE2MD_REMOVE_STRIKETHROUGH_TEXT")
            .map(|v| v == "true")
            .unwrap_or(false);

    Ok(ResolvedConfig {
        page_url,
        output_dir: absolutize_path(output_dir)?,
        dump_state_dir,
        table_conversion,
        remove_strikethrough_text,
    })
}

/// Extracts and normalizes the base URL (scheme + host + optional port) from a page URL.
fn extract_base_url(page_url: &str) -> Result<String> {
    let parsed = url::Url::parse(page_url).context("Invalid page URL")?;
    let origin = format!(
        "{}://{}{}",
        parsed.scheme(),
        parsed.host_str().unwrap_or(""),
        parsed.port().map(|p| format!(":{p}")).unwrap_or_default()
    );
    Ok(normalize_base_url(&origin))
}

fn append_markdown_header(title: &str, page_id: &str, webui: Option<&str>, body: &str) -> String {
    let mut markdown = format!("# {title}\n\n- Confluence Page ID: {page_id}\n");
    if let Some(url) = webui {
        markdown.push_str(&format!("- URL: {url}\n"));
    }
    markdown.push_str("\n---\n\n");
    markdown.push_str(body);
    markdown.push('\n');
    markdown
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
    let config = resolve_config(&cli)?;

    let base_url = extract_base_url(&config.page_url)?;
    let token = get_required_env()?.personal_access_token;
    let client = build_http_client()?;

    let page_id = match resolve_page_id_from_url(&client, &config.page_url, &base_url, &token).await
    {
        Ok(id) => id,
        Err(err) => {
            error!("failed to resolve page ID from URL: {err}");
            std::process::exit(1);
        }
    };
    debug!("Resolved page ID for \"{}\": {page_id}", config.page_url);

    ensure_dir(&config.output_dir).await?;
    if let Some(dir) = &config.dump_state_dir {
        ensure_dir(dir).await?;
    }

    let page = fetch_confluence_page(&client, &page_id, &base_url, &token).await?;
    write_dump_state(&config.dump_state_dir, "content.json", &page.content_json).await?;

    let title = if page.title.is_empty() {
        format!("page-{page_id}")
    } else {
        page.title.clone()
    };
    let output_path = config
        .output_dir
        .join(format!("{}.md", sanitize_file_name(&title)));

    write_dump_state(&config.dump_state_dir, "export.html", &page.export_html).await?;
    write_dump_state(
        &config.dump_state_dir,
        "storage.html",
        page.storage_html.as_deref().unwrap_or(""),
    )
    .await?;

    let attachments = list_attachments(&client, &page_id, &base_url, &token).await?;
    let maps = build_attachment_maps(&attachments);

    let assets_info = make_assets_info(&page_id, &title, &output_path);
    ensure_dir(&assets_info.assets_abs_dir).await?;

    let mut html = page.export_html.clone();
    let mut used_names: HashSet<String> = HashSet::new();

    let drawio_result = resolve_drawio_diagrams(
        &client,
        ResolveDrawioOptions {
            page_id: &page_id,
            storage_html: page.storage_html.as_deref(),
            export_html: &html,
            attachments_by_title: &maps.by_title,
            base_url: &base_url,
            token: &token,
            assets_abs_dir: &assets_info.assets_abs_dir,
            dump_state_abs_dir: config.dump_state_dir.as_deref(),
            markdown_image_prefix: &assets_info.markdown_image_prefix,
            used_names: &mut used_names,
        },
    )
    .await?;
    html = drawio_result.0;
    write_dump_state(&config.dump_state_dir, "rewrite_drawio.html", &html).await?;

    html = download_images_and_rewrite_html(
        &client,
        &html,
        DownloadImagesOptions {
            base_url: &base_url,
            personal_access_token: &token,
            assets_abs_dir: &assets_info.assets_abs_dir,
            markdown_image_prefix: &assets_info.markdown_image_prefix,
            used_names: &mut used_names,
        },
    )
    .await?;
    write_dump_state(&config.dump_state_dir, "rewrite_image.html", &html).await?;

    let plantuml_result = resolve_plantuml_diagrams(
        &client,
        ResolvePlantUmlOptions {
            page_id: &page_id,
            storage_html: page.storage_html.as_deref(),
            html: &html,
            attachments_by_title: &maps.by_title,
            base_url: &base_url,
            token: &token,
            assets_abs_dir: &assets_info.assets_abs_dir,
            markdown_image_prefix: &assets_info.markdown_image_prefix,
            used_names: &mut used_names,
        },
    )
    .await?;
    html = plantuml_result.0;
    write_dump_state(&config.dump_state_dir, "rewrite_plantuml.html", &html).await?;

    if let Some(storage_html) = page.storage_html.as_deref() {
        html = apply_task_list_statuses(&html, storage_html);
    }
    html = preprocess_confluence_macros(&html);
    write_dump_state(&config.dump_state_dir, "rewrite_macros.html", &html).await?;

    let markdown_body = convert_to_md(
        &html,
        ConvertOptions {
            table_conversion: config.table_conversion,
            remove_strikethrough_text: config.remove_strikethrough_text,
        },
    );

    let markdown = append_markdown_header(&title, &page_id, page.webui.as_deref(), &markdown_body);
    tokio::fs::write(&output_path, markdown).await?;
    info!("Written: {}", output_path.display());
    info!("Assets: {}", assets_info.assets_abs_dir.display());
    if let Some(dir) = &config.dump_state_dir {
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
