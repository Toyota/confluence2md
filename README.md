<p align="center">
  <img src="assets/logo.svg#gh-light-mode-only" alt="confluence2md" width="320" />
  <img src="assets/logo-dark.svg#gh-dark-mode-only" alt="confluence2md" width="320" />
</p>

> **Convert Confluence pages to clean, portable Markdown** — with images, draw.io diagrams, and PlantUML all included.

```
confluence2md [--output-path <dir>] <pageUrl>
```

---

## ✨ What It Does

```
┌──────────────┐       ┌───────────────┐       ┌──────────────────┐
│ Confluence   │──────▶│ confluence2md │──────▶│   Markdown + 📁  │
│   Page URL   │       │               │       │   Local Assets   │
└──────────────┘       └───────────────┘       └──────────────────┘
```

confluence2md fetches a Confluence page via REST API and converts it to GitHub-Flavored Markdown (GFM) with:

- 🖼️ **Images** — downloaded and stored locally in an assets directory
- 📊 **PlantUML** — source code extracted and embedded as `` ```plantuml `` fenced code blocks
- 🎨 **draw.io** — diagrams saved as `.drawio.png` with embedded XML (editable in the draw.io VS Code extension!). Multi-page diagrams produce one image per referenced page, and draw.io images rendered from included content such as Table Excerpt Include are downloaded from their source attachment page and saved locally the same way.
- 🅰  **Text effects** — Support for the strikethrough [text effect](https://confluence.atlassian.com/doc/confluence-storage-format-790796544.html#ConfluenceStorageFormat-Texteffects)
- 📋 **Tables** — preserved as GFM tables
- 🔗 **Links** — resolved relative to the Confluence base URL
- 🔗 **Jira links** — Confluence Jira issue macros are converted to simple Markdown links such as `[DEMO-1234](https://jira.example.com/browse/DEMO-1234)` without summary/status placeholder text.
- 📚 **TOC macros** — Confluence table-of-contents links are rewritten to Markdown heading anchors, so generated TOCs jump to the converted headings without inserting raw HTML anchors.
- ✅ **Task lists** — Confluence inline task lists are converted to GFM task list items, preserving checked items as `- [x]` and unchecked items as `- [ ]`.
- 💬 **Alert macros** — Confluence `info`, `panel`, `tip`, `note`, and `warning` macros are converted to [GitHub alert syntax](https://docs.github.com/en/get-started/writing-on-github/getting-started-with-writing-and-formatting-on-github/basic-writing-and-formatting-syntax#alerts):

  | Confluence macro | GitHub alert   |
  | ---------------- | -------------- |
  | `info`           | `[!IMPORTANT]` |
  | `panel`          | `[!NOTE]`      |
  | `tip`            | `[!TIP]`       |
  | `note`           | `[!WARNING]`   |
  | `warning`        | `[!CAUTION]`   |

- 🔽 **Expand macros** — Confluence `expand` macros are converted to collapsible `<details>` / `<summary>` HTML blocks (rendered natively by GitHub):

  ```html
  <details>
  <summary>変更履歴/Change history</summary>

  … content …

  </details>
  ```

- 🔗 **Google Drive Live Link macros** — Confluence `lref-gdrive-file` macros are converted to plain Markdown links with the fixed text `Google Drive Link`, pointing at the macro's `url` parameter (e.g. `[Google Drive Link](https://docs.google.com/...)`).

- 💻 **Code macros** — Confluence `code` macros are converted to fenced Markdown code blocks with the optional language identifier preserved:

  ````markdown
  ```c++
  int x = 1;
  ```
  ````
## 📦 Installation

Download the latest release from the [Releases](https://github.com/Toyota/confluence2md/releases) page according to your OS.


## 🔧 Configuration

Set the following environment variables before running:

| Variable                              | Description                                                                | Example   |
| ------------------------------------- | -------------------------------------------------------------------------- | --------- |
| `CONFLUENCE2MD_PERSONAL_ACCESS_TOKEN` | A Confluence Personal Access Token                                         | `NjQ2...` |
| `CONFLUENCE2MD_OUTPUT_PATH`           | Directory to write the output Markdown file (default: current directory)   | `out`     |
| `CONFLUENCE2MD_DUMP_STATE_PATH`       | Directory to write diagnostic state and raw intermediate files             | `dumps`   |
| `CONFLUENCE2MD_LOG_LEVEL`             | Log verbosity: `DEBUG` \| `INFO` \| `WARNING` \| `ERROR` (default: `INFO`) | `DEBUG`   |
| `CONFLUENCE2MD_TABLE_CONVERSION`      | Table conversion mode: `default` \| `always` (default: `default`)          | `always`  |
| `CONFLUENCE2MD_REMOVE_STRIKETHROUGH_TEXT` | Set to `true` to remove strikethrough text entirely             | `true`    |

You can export them in your shell profile or pass them inline:

```bash
export CONFLUENCE2MD_PERSONAL_ACCESS_TOKEN="your-token-here"
```

> 💡 **Tip:** To generate a Personal Access Token, go to your Confluence profile → **Settings** → **Personal Access Tokens** → **Create token**.

## 🚀 Usage

### Example

```bash
export CONFLUENCE2MD_PERSONAL_ACCESS_TOKEN="your-token-here"
confluence2md 'https://confluence.example.com/pages/viewpage.action?pageId=393229'
```

### Options

| Option                          | Description                                                                      | Default           |
| ------------------------------- | -------------------------------------------------------------------------------- | ----------------- |
| `--output-path <dir>`           | Directory to write the output Markdown file                                      | Current directory |
| `--dump-state-path <dir>`       | Directory to write raw API, intermediate HTML dumps, and raw `.drawio` XML files | Not written       |
| `--log-level <level>`           | Log verbosity: `DEBUG` \| `INFO` \| `WARNING` \| `ERROR`                         | `INFO`            |
| `--table-conversion <mode>`     | Table conversion mode: `default` \| `always`                                     | `default`         |
| `--remove-strikethrough-text[=true|false]` | Remove strikethrough text entirely instead of converting to `~~text~~`. Omitting the value implies `true`. When passing a value, the equals form is required (e.g. `--remove-strikethrough-text=false`). | Off               |
| `--version`                     | Print the version and exit                                                       | —                 |

> 💡 `--output-path` takes precedence over `CONFLUENCE2MD_OUTPUT_PATH`.
> 💡 `--dump-state-path` takes precedence over `CONFLUENCE2MD_DUMP_STATE_PATH`.
> 💡 `--log-level` takes precedence over `CONFLUENCE2MD_LOG_LEVEL`.
> 💡 `--table-conversion` takes precedence over `CONFLUENCE2MD_TABLE_CONVERSION`.
> 💡 `--remove-strikethrough-text` takes precedence over `CONFLUENCE2MD_REMOVE_STRIKETHROUGH_TEXT`.

#### Table conversion modes

| Mode      | Behaviour                                                                                                                                                                            |
| --------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `default` | Converts Markdown-compatible tables to GFM, promoting the first row to a header when Confluence omits `<thead>`. Merged or nested tables are kept as readable raw HTML.              |
| `always`  | Uses the custom Confluence table plugin. All tables are converted to GFM, with `colspan`/`rowspan` expanded (merged cells are split; the value is placed in the top-left cell only). |

#### Log levels

| Level     | Output                               |
| --------- | ------------------------------------ |
| `DEBUG`   | Every processing step (most verbose) |
| `INFO`    | Key milestones (default)             |
| `WARNING` | Warnings and recoverable errors only |
| `ERROR`   | Fatal errors only (least verbose)    |

### Output structure

```
out/
├── Page_Title.md                    # 📄 Converted Markdown
└── Page_Title_assets/               # 📁 Downloaded assets
    ├── image_1.png
    ├── diagram.drawio.png           # 🎨 Editable in draw.io!
    ├── diagram-<aspectHash>.drawio.png  # Per-page image for multi-page diagrams
    ├── external-diagram.drawio.png  # draw.io rendered from included external-page content
    └── ...
```

When `--dump-state-path dumps` is specified, diagnostic state and raw `.drawio` XML files are written under the dump directory:

```text
dumps/
├── content.json
├── export.html
├── storage.html
├── *.drawio
├── rewrite_drawio.html
├── rewrite_image.html
├── rewrite_plantuml.html
└── rewrite_macros.html
```

## 🧑‍💻 Appendix

### Supported URL formats

confluence2md automatically detects the page from various Confluence URL formats:

| Format                          | Example                                                                            |
| ------------------------------- | ---------------------------------------------------------------------------------- |
| `pageId` query param            | `https://confluence.example.com/pages/viewpage.action?pageId=1082335934`           |
| Cloud `/spaces/.../pages/` path | `https://confluence.example.com/wiki/spaces/DEMO/pages/9876543/My+Page`            |
| Classic `/display/` path        | `https://confluence.example.com/display/DEMO/My+Page+Title`                        |
| `spaceKey` + `title` params     | `https://confluence.example.com/pages/viewpage.action?spaceKey=DEMO&title=My+Page` |

## 🛠️ Tech Stack

| Component    | Technology                                                                                                  |
| ------------ | ----------------------------------------------------------------------------------------------------------- |
| Runtime      | [Rust](https://www.rust-lang.org/) (stable, 2024 edition) with [Tokio](https://tokio.rs/)                   |
| Language     | Rust                                                                                                        |
| HTML parsing | [`htmd`](https://crates.io/crates/htmd) + [`markup5ever_rcdom`](https://crates.io/crates/markup5ever_rcdom) |
| HTTP client  | [`reqwest`](https://crates.io/crates/reqwest) with `rustls` (bundled Mozilla WebPKI roots + OS certificate store) |
| CLI parsing  | [`clap`](https://crates.io/crates/clap) (derive macros)                                                     |
| API          | [Confluence REST API v1](https://developer.atlassian.com/cloud/confluence/rest/v1/intro/#about)             |
