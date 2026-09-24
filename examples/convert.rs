//! Convert a document to Markdown, or HTML with `--html`:
//! `cargo run --example convert -- <file> [-f csv] [--html] [-o out.md] [--assets dir]`

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anydoc::{ConvertError, Format};

const USAGE: &str = "usage: convert <file> [-f csv] [--html] [-o out.md] [--assets dir]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut format: Option<Format> = None;
    let mut assets: Option<PathBuf> = None;
    let mut html = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--output" => {
                i += 1;
                output = args.get(i).map(PathBuf::from);
            }
            "-f" | "--format" => {
                i += 1;
                let Some(name) = args.get(i) else { break };
                let Some(named) = Format::from_extension(name) else {
                    eprintln!("error: unknown format: {name}");
                    return ExitCode::FAILURE;
                };
                format = Some(named);
            }
            "--html" => html = true,
            "--assets" => {
                i += 1;
                assets = args.get(i).map(PathBuf::from);
            }
            other => input = Some(PathBuf::from(other)),
        }
        i += 1;
    }
    let Some(input) = input else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };

    match run(&input, output.as_deref(), format, assets.as_deref(), html) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(
    input: &Path,
    output: Option<&Path>,
    format: Option<Format>,
    assets: Option<&Path>,
    html: bool,
) -> Result<(), ConvertError> {
    let bytes = std::fs::read(input)?;
    // Without -f the format comes from the file content, with the extension as
    // the fallback.
    let format =
        match format.or_else(|| Format::from_bytes(&bytes)).or_else(|| Format::from_path(input)) {
            Some(format) => format,
            None => {
                return Err(ConvertError::Unsupported(format!(
                    "unrecognized file content and extension: {}",
                    input.display()
                )));
            }
        };

    let start = std::time::Instant::now();
    let document = anydoc::to_document(&bytes, format)?;
    let stem = input.file_stem().unwrap_or_default().to_string_lossy();
    let text = if html {
        // With --assets the page links the files written below; without, it
        // carries the images inline.
        let mut options = anydoc::HtmlOptions::default();
        if assets.is_some() {
            options.asset_prefix = Some(format!("{stem}-"));
        }
        anydoc::document_to_html(&document, &options)
    } else {
        anydoc::document_to_markdown(&document)
    };
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    eprintln!("converted {} in {}", input.display(), millis(elapsed));

    match output {
        Some(out) => std::fs::write(out, text)?,
        None => {
            use std::io::Write;
            let _ = std::io::stdout().write_all(text.as_bytes());
        }
    }

    // Images and embedded objects live on the document model; Markdown shows
    // only their alt text.
    if let Some(dir) = assets {
        std::fs::create_dir_all(dir)?;
        for asset in &document.assets {
            let name = format!("{stem}-{}.{}", asset.id.0, asset.extension());
            std::fs::write(dir.join(name), &asset.bytes)?;
        }
        eprintln!("wrote {} assets to {}", document.assets.len(), dir.display());
    }
    Ok(())
}

fn millis(ms: f64) -> String {
    if ms < 10.0 { format!("{ms:.2}ms") } else { format!("{ms:.0}ms") }
}
