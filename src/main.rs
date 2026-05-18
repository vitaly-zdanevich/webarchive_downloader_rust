use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use url::Url;
use webarchive_downloader_rust::cdx::{CdxQuery, MatchType, SnapshotStrategy};
use webarchive_downloader_rust::downloader::{
    CancellationFlag, DownloadOptions, build_client, download_site, list_records,
};
use webarchive_downloader_rust::output_summary::{format_bytes, summarize_output_dir};
use webarchive_downloader_rust::pathmap::SiteMapper;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Website/domain to download, for example "another.by" or "https://example.com/blog/".
    target: String,

    /// Directory to write the static site into.
    #[arg(short, long, default_value = "public")]
    output: PathBuf,

    /// CDX match type used by the Internet Archive.
    #[arg(long, value_enum, default_value_t = MatchType::Domain)]
    match_type: MatchType,

    /// Which capture to keep when a URL has multiple archived snapshots.
    #[arg(long, value_enum, default_value_t = SnapshotStrategy::Latest)]
    strategy: SnapshotStrategy,

    /// Earliest capture timestamp to include, in YYYYMMDDhhmmss form. Trailing digits may be omitted.
    #[arg(long)]
    from: Option<String>,

    /// Latest capture timestamp to include, in YYYYMMDDhhmmss form. Trailing digits may be omitted.
    #[arg(long)]
    to: Option<String>,

    /// Maximum CDX records to read before choosing the latest/earliest copy of each URL.
    #[arg(long)]
    limit: Option<usize>,

    /// Print selected captures without downloading them.
    #[arg(long)]
    list: bool,

    /// Redownload and replace files that already exist.
    #[arg(long)]
    overwrite: bool,

    /// Deprecated compatibility alias. Existing files are skipped by default.
    #[arg(long, hide = true, conflicts_with = "overwrite")]
    no_clobber: bool,

    /// Keep archived links as-is instead of rewriting internal HTML/CSS links to local paths.
    #[arg(long)]
    no_rewrite: bool,

    /// Wayback root URL.
    #[arg(long, default_value = "https://web.archive.org")]
    archive_root: Url,

    /// User-Agent sent to the Internet Archive.
    #[arg(long, default_value = "webarchive-downloader-rust/0.1")]
    user_agent: String,

    /// Request timeout in seconds.
    #[arg(long, default_value_t = 60)]
    timeout_seconds: u64,
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let mapper = SiteMapper::new(&cli.target)?;
    let mut query = CdxQuery::new(
        mapper.cdx_target().to_owned(),
        cli.match_type,
        cli.strategy,
        cli.archive_root,
    );
    query.from = cli.from;
    query.to = cli.to;
    query.limit = cli.limit;

    let client = build_client(&cli.user_agent, Duration::from_secs(cli.timeout_seconds))?;

    if cli.list {
        let records = list_records(&client, &query).await?;
        for record in records {
            println!(
                "{} {} {}",
                record.timestamp, record.mimetype, record.original
            );
        }
        return Ok(ExitCode::SUCCESS);
    }

    let cancellation = CancellationFlag::new();
    spawn_ctrl_c_handler(cancellation.clone());

    let report = download_site(
        client,
        mapper,
        query,
        DownloadOptions {
            output_dir: cli.output,
            no_clobber: !cli.overwrite || cli.no_clobber,
            rewrite_links: !cli.no_rewrite,
            cancellation: cancellation.clone(),
        },
    )
    .await
    .context("download failed")?;

    if cancellation.is_cancelled() || report.cancelled > 0 {
        println!(
            "stopped: discovered {}, downloaded {}, skipped {}, cancelled {}, failed {}, output {}",
            report.discovered,
            report.downloaded,
            report.skipped,
            report.cancelled,
            report.failed,
            report.output_dir.display()
        );
    } else {
        println!(
            "done: discovered {}, downloaded {}, skipped {}, failed {}, output {}",
            report.discovered,
            report.downloaded,
            report.skipped,
            report.failed,
            report.output_dir.display()
        );
    }

    print_output_summary(&report.output_dir)?;

    if cancellation.is_cancelled() || report.cancelled > 0 {
        Ok(ExitCode::from(130))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn spawn_ctrl_c_handler(cancellation: CancellationFlag) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }

        cancellation.cancel();
        eprintln!("received Ctrl-C; stopping after current requests finish...");

        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("received Ctrl-C again; exiting immediately");
            std::process::exit(130);
        }
    });
}

fn print_output_summary(output_dir: &Path) -> Result<()> {
    let summary = summarize_output_dir(output_dir, 10).with_context(|| {
        format!(
            "failed to summarize output directory {}",
            output_dir.display()
        )
    })?;

    println!(
        "output size: {} ({} bytes)",
        format_bytes(summary.total_bytes),
        summary.total_bytes
    );

    if summary.biggest_files.is_empty() {
        println!("10 biggest files: none");
    } else {
        println!("10 biggest files:");
        for file in summary.biggest_files {
            println!(
                "  {:>10}  {}",
                format_bytes(file.bytes),
                file.path.display()
            );
        }
    }

    Ok(())
}
