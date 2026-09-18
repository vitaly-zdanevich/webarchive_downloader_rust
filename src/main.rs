use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, Local};
use clap::Parser;
use url::Url;
use webarchive_downloader_rust::DEFAULT_USER_AGENT;
use webarchive_downloader_rust::cdx::{CdxQuery, MatchType, SnapshotStrategy};
use webarchive_downloader_rust::downloader::{
    CancellationFlag, DownloadOptions, RepairOptions, build_client, download_site, list_records,
    repair_local_links, repair_output_dir,
};
use webarchive_downloader_rust::link_validation::validate_local_links;
use webarchive_downloader_rust::output_summary::{format_bytes, summarize_output_dir};
use webarchive_downloader_rust::pathmap::SiteMapper;

const BYTES_PER_MIB: u64 = 1024 * 1024;
const DEFAULT_TIMEOUT_SECONDS: u64 = 15 * 60;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Website host to download, for example "another.by" or "https://example.com/blog/".
    target: Option<String>,

    /// Directory to write the static site into.
    #[arg(short, long, default_value = "public")]
    output: PathBuf,

	/// CDX discovery scope: "domain" includes subdomains; "host" searches only the specified host and may download less.
	/// Linked content on related subdomains can still be downloaded.
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

    /// Validate an existing output directory without downloading or modifying files.
    #[arg(long)]
    validate_only: bool,

    /// Repair an existing output directory by fetching recoverable missing static assets.
    #[arg(long)]
    repair_output: bool,

	/// Repair navigation to existing local HTML files without any network requests.
	#[arg(long, conflicts_with_all = ["validate_only", "repair_output", "list", "overwrite", "no_rewrite"])]
	repair_local_links: bool,

    /// Redownload and replace files that already exist.
    #[arg(long)]
    overwrite: bool,

	/// Retry recognized local HTML error pages while preserving all other existing files.
	#[arg(long, conflicts_with_all = ["overwrite", "no_clobber", "validate_only", "repair_output", "repair_local_links", "list"])]
	recover_existing: bool,

    /// Deprecated compatibility alias. Existing files are skipped by default.
    #[arg(long, hide = true, conflicts_with = "overwrite")]
    no_clobber: bool,

    /// Keep archived links as-is instead of rewriting internal HTML/CSS links to local paths.
    #[arg(long)]
    no_rewrite: bool,

    /// Skip post-download validation of generated local links.
    #[arg(long)]
    no_validate_links: bool,

    /// Return exit code 2 for missing links, source-less images, or recognized HTML error pages.
    #[arg(long, conflicts_with = "no_validate_links")]
    strict_validate_links: bool,

    /// Maximum size, in MiB, for linked pages and resources on related hosts.
    /// Omit for no cap. Use 0 to disable the pass.
    #[arg(long, value_name = "MIB")]
    max_extra_download_size_mib: Option<u64>,

    /// Wayback root URL.
    #[arg(long, default_value = "https://web.archive.org")]
    archive_root: Url,

    /// User-Agent sent to the Internet Archive.
    #[arg(long, default_value = DEFAULT_USER_AGENT)]
    user_agent: String,

    /// Request timeout in seconds.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECONDS)]
    timeout_seconds: u64,

    /// SSH destination used as a fallback SOCKS tunnel when Wayback blocks the direct connection.
    /// Repeat to provide multiple fallback hosts.
    #[arg(long, value_name = "USER@HOST")]
    ssh: Vec<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let started_at = Instant::now();

    match run(started_at).await {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("Error: {error:#}");
            print_elapsed_time(started_at);
            ExitCode::FAILURE
        }
    }
}

async fn run(started_at: Instant) -> Result<ExitCode> {
    let cli = Cli::parse();

    if cli.validate_only {
        let exit_code = validate_output_dir(&cli.output, cli.strict_validate_links)?;
        print_elapsed_time(started_at);
        return Ok(exit_code);
    }

	if cli.repair_local_links {
		let report = repair_local_links(&cli.output).await?;
		println!("repaired {} local navigation links in {} files", report.repaired_links, report.modified_files);
		let exit_code = if cli.no_validate_links {
			ExitCode::SUCCESS
		} else {
			validate_output_dir(&cli.output, cli.strict_validate_links)?
		};
		print_elapsed_time(started_at);
		return Ok(exit_code);
	}

    let target = cli
        .target
        .as_deref()
        .context("target is required unless --validate-only or --repair-local-links is used")?;
    let mapper = SiteMapper::new(target)?;
    let mut query = CdxQuery::new(
        mapper.cdx_target().to_owned(),
        cli.match_type,
        cli.strategy,
        cli.archive_root,
    );
    query.from = cli.from;
    query.to = cli.to;
    query.limit = cli.limit;

    let client = build_client(
        &cli.user_agent,
        Duration::from_secs(cli.timeout_seconds),
        cli.ssh,
    )?;

    let extra_download_max_bytes = extra_download_max_bytes(cli.max_extra_download_size_mib);

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

    if cli.repair_output {
        let report = repair_output_dir(
            client,
            mapper,
            RepairOptions {
                output_dir: cli.output,
                archive_root: query.archive_root,
                match_type: query.match_type,
                from: query.from,
                to: query.to,
                limit: query.limit,
                strategy: query.strategy,
                extra_download_max_bytes,
                validate_links: !cli.no_validate_links,
                cancellation: cancellation.clone(),
            },
        )
        .await
        .context("repair failed")?;

        print_repair_report(&report);

        print_output_summary(&report.output_dir)?;

        let exit_code = if cancellation.is_cancelled() {
            ExitCode::from(130)
        } else if cli.strict_validate_links
            && (report.missing_local_links > 0 || report.missing_image_sources > 0 || report.unusable_html_files > 0)
        {
            ExitCode::from(2)
        } else {
            ExitCode::SUCCESS
        };
        print_elapsed_time(started_at);
        return Ok(exit_code);
    }

    let report = download_site(
        client,
        mapper,
        query,
        DownloadOptions {
            output_dir: cli.output,
            no_clobber: !cli.overwrite || cli.no_clobber,
			recover_existing: cli.recover_existing,
            rewrite_links: !cli.no_rewrite,
            extra_download_max_bytes,
            validate_links: !cli.no_validate_links,
            cancellation: cancellation.clone(),
        },
    )
    .await
    .context("download failed")?;

    if cancellation.is_cancelled() || report.cancelled > 0 {
        print_stopped_report(&report);
    } else {
        print_download_report(&report);
    }

    print_output_summary(&report.output_dir)?;

    let exit_code = if cancellation.is_cancelled() || report.cancelled > 0 {
        ExitCode::from(130)
    } else if cli.strict_validate_links
        && (report.missing_local_links > 0 || report.missing_image_sources > 0 || report.unusable_html_files > 0)
    {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    };
    print_elapsed_time(started_at);
    Ok(exit_code)
}

fn validate_output_dir(output_dir: &Path, strict: bool) -> Result<ExitCode> {
    let report = validate_local_links(output_dir).with_context(|| {
        format!(
            "failed to validate output directory {}",
            output_dir.display()
        )
    })?;

    println!(
        "validated {} local links; missing {}; unique missing targets {}; images without source {}",
        report.checked,
        report.missing.len(),
        report.unique_missing_targets(),
        report.missing_image_sources.len()
    );

    for missing in report.missing.iter().take(20) {
        let source = missing
            .source
            .strip_prefix(output_dir)
            .unwrap_or(&missing.source);
        let target = missing
            .target
            .strip_prefix(output_dir)
            .unwrap_or(&missing.target);
        println!(
            "missing local link: {} -> {} ({})",
            source.display(),
            missing.href,
            target.display()
        );
    }

    if report.missing.len() > 20 {
        println!("missing local links omitted: {}", report.missing.len() - 20);
    }

    for missing in report.missing_image_sources.iter().take(20) {
        let source = missing
            .source
            .strip_prefix(output_dir)
            .unwrap_or(&missing.source);
        println!(
            "image without source: {} ({})",
            source.display(),
            missing.descriptor
        );
    }

    if report.missing_image_sources.len() > 20 {
        println!(
            "images without source omitted: {}",
            report.missing_image_sources.len() - 20
        );
    }

	println!("unusable HTML files: {}", report.unusable_html.len());
	for file in report.unusable_html.iter().take(20) {
		println!("unusable HTML: {}", file.strip_prefix(output_dir).unwrap_or(file).display());
	}
    if strict && (!report.missing.is_empty() || !report.missing_image_sources.is_empty() || !report.unusable_html.is_empty()) {
        Ok(ExitCode::from(2))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn extra_download_max_bytes(max_extra_download_size_mib: Option<u64>) -> Option<u64> {
    match max_extra_download_size_mib {
        None => Some(u64::MAX),
        Some(0) => None,
        Some(mib) => Some(mib.saturating_mul(BYTES_PER_MIB)),
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

fn print_repair_report(report: &webarchive_downloader_rust::downloader::RepairReport) {
    println!("repair done:");
    println!(
        "  recovered static assets: {}",
        report.recovered_static_assets
    );
    println!(
        "  static asset aliases: {}",
        report.static_asset_aliases_created
    );
    println!(
        "  unavailable static assets: {}",
        report.unavailable_static_assets
    );
    println!("  aliases: {}", report.aliases_created);
    println!("  checked links: {}", report.local_links_checked);
    println!("  missing links: {}", report.missing_local_links);
    println!("  unique missing targets: {}", report.unique_missing_targets);
    println!("  images without source: {}", report.missing_image_sources);
	println!("  unusable HTML files: {}", report.unusable_html_files);
    println!("  output: {}", report.output_dir.display());
}

fn print_stopped_report(report: &webarchive_downloader_rust::downloader::DownloadReport) {
    println!("stopped:");
    println!("  discovered: {}", report.discovered);
    println!("  downloaded: {}", report.downloaded);
    println!("  skipped: {}", report.skipped);
    println!("  cancelled: {}", report.cancelled);
    println!("  failed: {}", report.failed);
    println!("  unavailable snapshots: {}", report.unavailable_snapshots);
    println!("  retained unusable captures: {}", report.retained_unusable_captures);
    println!("  output: {}", report.output_dir.display());
}

fn print_download_report(report: &webarchive_downloader_rust::downloader::DownloadReport) {
    println!("done:");
    println!("  discovered: {}", report.discovered);
    println!("  downloaded: {}", report.downloaded);
    println!("  skipped: {}", report.skipped);
    println!("  failed: {}", report.failed);
    println!("  unavailable snapshots: {}", report.unavailable_snapshots);
    println!("  retained unusable captures: {}", report.retained_unusable_captures);
    println!("  linked files downloaded: {}", report.extra_downloads);
    println!(
        "  linked files unavailable: {}",
        report.linked_files_unavailable
    );
    println!(
        "  recovered static assets: {}",
        report.recovered_static_assets
    );
    println!(
        "  static asset aliases: {}",
        report.static_asset_aliases_created
    );
    println!(
        "  unavailable static assets: {}",
        report.unavailable_static_assets
    );
    println!("  aliases: {}", report.aliases_created);
    println!("  checked links: {}", report.local_links_checked);
    println!("  missing links: {}", report.missing_local_links);
    println!("  unique missing targets: {}", report.unique_missing_targets);
    println!("  images without source: {}", report.missing_image_sources);
	println!("  unusable HTML files: {}", report.unusable_html_files);
    println!("  output: {}", report.output_dir.display());
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

/// Prints elapsed runtime followed by the completion timestamp in the machine's timezone.
fn print_elapsed_time(started_at: Instant) {
    println!(
        "{}",
        format_completion_summary(started_at.elapsed(), Local::now().fixed_offset())
    );
}

/// Formats the completion footer using the timestamp's local date and weekday.
fn format_completion_summary(duration: Duration, finished_at: DateTime<FixedOffset>) -> String {
    format!(
        "elapsed time: {}\nFinished at {}",
        format_elapsed_time(duration),
        finished_at.format("%Y %B %-d, %A, %H:%M")
    )
}

fn format_elapsed_time(duration: Duration) -> String {
    if duration.as_secs() == 0 {
        return format!("{} ms", duration.as_millis());
    }

    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

	/// Both help forms distinguish initial host discovery from linked subdomain downloads.
	#[test]
	fn help_explains_host_scope_and_linked_subdomains() {
		for flag in ["-h", "--help"] {
			let error = Cli::try_parse_from(["webarchive-downloader-rust", flag]).unwrap_err();
			assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
			let help = error.to_string().split_whitespace().collect::<Vec<_>>().join(" ");
			assert!(help.contains("\"domain\" includes subdomains"), "{help}");
			assert!(help.contains("\"host\" searches only the specified host and may download less"), "{help}");
			assert!(help.contains("Linked content on related subdomains can still be downloaded"), "{help}");
			assert!(help.contains("[default: domain]"), "{help}");
		}
	}

	/// All CLI discovery modes include indexed subdomains unless explicitly narrowed.
	#[test]
	fn default_match_type_includes_subdomains() {
		for mode in [None, Some("--list"), Some("--repair-output")] {
			let mut args = vec!["webarchive-downloader-rust", "example.com"];
			args.extend(mode);
			let cli = Cli::try_parse_from(args).unwrap();
			assert_eq!(cli.match_type, MatchType::Domain);
		}
	}

	/// Explicit matching scopes remain available when the default is broader.
	#[test]
	fn explicit_match_type_overrides_default() {
		for (argument, expected) in [
			("host", MatchType::Host),
			("prefix", MatchType::Prefix),
			("exact", MatchType::Exact),
			("domain", MatchType::Domain),
		] {
			let cli = Cli::try_parse_from([
				"webarchive-downloader-rust", "example.com", "--match-type", argument,
			]).unwrap();
			assert_eq!(cli.match_type, expected);
		}
	}

	/// Release metadata and the default HTTP identity must use the same package version.
	#[test]
	fn default_user_agent_matches_package_version() {
		let cli = Cli::try_parse_from(["webarchive-downloader-rust", "example.com"]).unwrap();
		assert_eq!(cli.user_agent, concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")));
	}

    #[test]
    fn parses_repeated_ssh_destinations() {
        let cli = Cli::try_parse_from([
            "webarchive-downloader-rust",
            "example.com",
            "--ssh",
            "ubuntu@151.145.94.114",
            "--ssh",
            "ubuntu@203.0.113.10",
        ])
        .unwrap();

        assert_eq!(
            cli.ssh,
            vec![
                "ubuntu@151.145.94.114".to_owned(),
                "ubuntu@203.0.113.10".to_owned()
            ]
        );
    }

    #[test]
    fn extra_downloads_are_uncapped_by_default() {
        let cli = Cli::try_parse_from(["webarchive-downloader-rust", "example.com"]).unwrap();

        assert_eq!(cli.max_extra_download_size_mib, None);
        assert_eq!(
            extra_download_max_bytes(cli.max_extra_download_size_mib),
            Some(u64::MAX)
        );
    }

    #[test]
    fn zero_extra_download_size_disables_extra_downloads() {
        assert_eq!(extra_download_max_bytes(Some(0)), None);
    }

    #[test]
    fn explicit_extra_download_size_sets_byte_cap() {
        assert_eq!(
            extra_download_max_bytes(Some(128)),
            Some(128 * BYTES_PER_MIB)
        );
    }

    #[test]
    fn formats_elapsed_time() {
        assert_eq!(format_elapsed_time(Duration::from_millis(42)), "42 ms");
        assert_eq!(format_elapsed_time(Duration::from_secs(9)), "9s");
        assert_eq!(format_elapsed_time(Duration::from_secs(125)), "2m 5s");
        assert_eq!(format_elapsed_time(Duration::from_secs(3661)), "1h 1m 1s");
    }

    #[test]
    fn prints_finish_time_directly_after_elapsed_time() {
        let finished_at =
            chrono::DateTime::parse_from_rfc3339("2026-09-11T23:59:00+04:00").unwrap();

        assert_eq!(
            format_completion_summary(Duration::from_secs(30_539), finished_at),
            "elapsed time: 8h 28m 59s\nFinished at 2026 September 11, Friday, 23:59"
        );
    }

    #[test]
    fn finish_time_preserves_local_date_across_utc_midnight() {
        let finished_at =
            chrono::DateTime::parse_from_rfc3339("2026-09-12T00:05:00+04:00").unwrap();

        assert_eq!(
            format_completion_summary(Duration::from_secs(1), finished_at),
            "elapsed time: 1s\nFinished at 2026 September 12, Saturday, 00:05"
        );
    }
}
