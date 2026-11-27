use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{
    Response, StatusCode,
    header::{HeaderMap, RETRY_AFTER},
};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;
use url::Url;

use crate::alias_repair::{
    create_missing_directory_aliases, create_missing_topic_aliases,
};
use crate::cdx::{
    CdxQuery, CdxRecord, CdxRetryPolicy, MAX_WAYBACK_RETRY_DELAY_SECONDS, MatchType,
    SnapshotStrategy, fetch_all_records_with_policy, fetch_latest_records,
    is_cdx_connectivity_error, is_redirect_status_code, sort_records_for_strategy,
};
use crate::download_refs::{extract_related_references, is_interactive_forum_extra_reference};
use crate::link_validation::{collect_html_files, extract_document_references, repair_local_navigation_html, resolve_local_reference, validate_local_links};
use crate::recovery::{AliasEvent, CaptureEvent, RecoveryState, record_alias_event};
use crate::noise::is_archive_noise_record;
use crate::pathmap::{
    SiteMapper, is_css_mimetype, is_html_mimetype, normalize_lookup_url, relative_link,
    strip_volatile_query_params, unwrap_wayback_url,
};
use crate::retry::{format_retry_delay, should_try_ssh_after_status};
use crate::rewrite::{RewriteContext, rewrite_css, rewrite_html};
use crate::soft_redirect::is_unusable_html_capture;
use crate::text_encoding::{decode_text, utf8_bytes};
use crate::wayback_client::WaybackClient;

#[derive(Clone, Debug)]
pub struct DownloadOptions {
    pub output_dir: PathBuf,
    pub no_clobber: bool,
	/// Replaces recognized local HTML error pages only after a usable capture is found.
	pub recover_existing: bool,
    pub rewrite_links: bool,
    /// Inserts a non-visible Wayback Machine snapshot URL comment at the top of rewritten HTML pages.
    pub add_wayback_comment: bool,
    pub extra_download_max_bytes: Option<u64>,
    pub validate_links: bool,
    pub cancellation: CancellationFlag,
}

#[derive(Clone, Debug, Default)]
pub struct DownloadReport {
    pub discovered: usize,
    pub downloaded: usize,
    pub skipped: usize,
    pub cancelled: usize,
    pub failed: usize,
    pub unavailable_snapshots: usize,
    /// Captures saved during this run that still matched the unusable-HTML detector.
    pub retained_unusable_captures: usize,
    pub aliases_created: usize,
    pub extra_downloads: usize,
    pub linked_files_unavailable: usize,
    pub recovered_static_assets: usize,
    pub static_asset_aliases_created: usize,
    pub unavailable_static_assets: usize,
    pub local_links_checked: usize,
    pub missing_local_links: usize,
    /// Distinct missing filesystem targets, not reference occurrences.
    pub unique_missing_targets: usize,
    pub missing_image_sources: usize,
	/// Recognized error/redirect pages remaining in the final output after validation.
	pub unusable_html_files: usize,
    pub output_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct RepairOptions {
    pub output_dir: PathBuf,
    pub archive_root: Url,
    pub match_type: MatchType,
    pub from: Option<String>,
    pub to: Option<String>,
    pub limit: Option<usize>,
    pub strategy: SnapshotStrategy,
    pub extra_download_max_bytes: Option<u64>,
    pub validate_links: bool,
    pub cancellation: CancellationFlag,
}

#[derive(Clone, Debug, Default)]
pub struct RepairReport {
    pub aliases_created: usize,
    pub recovered_static_assets: usize,
    pub static_asset_aliases_created: usize,
    pub unavailable_static_assets: usize,
    pub local_links_checked: usize,
    pub missing_local_links: usize,
    /// Distinct missing filesystem targets, not reference occurrences.
    pub unique_missing_targets: usize,
    pub missing_image_sources: usize,
	/// Recognized error/redirect pages remaining in the final output after validation.
	pub unusable_html_files: usize,
    pub output_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct CancellationFlag {
    cancelled: Arc<AtomicBool>,
}

impl CancellationFlag {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

impl Default for CancellationFlag {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
struct DownloadJob {
    record: CdxRecord,
    local_path: PathBuf,
    /// Actual response-byte limit for a linked download; primary captures are uncapped.
    max_bytes: Option<u64>,
}

#[derive(Clone, Debug)]
struct FallbackOptions {
    from: Option<String>,
    to: Option<String>,
    strategy: SnapshotStrategy,
	recovery: Arc<RecoveryState>,
	originals_by_path: HashMap<PathBuf, CdxRecord>,
	candidates_by_path: HashMap<PathBuf, Vec<CdxRecord>>,
}

impl FallbackOptions {
    fn discovered_candidates_for(
        &self,
        local_path: Option<&Path>,
        current_record: &CdxRecord,
    ) -> Vec<CdxRecord> {
        if let Some(path) = local_path
            && let Some(candidates) = self.candidates_by_path.get(path)
        {
            return candidates.clone();
        }
        let normalized_current = normalize_lookup_url(&current_record.original);
        for candidates in self.candidates_by_path.values() {
            if candidates.iter().any(|candidate| {
                candidate.original == current_record.original
                    || normalize_lookup_url(&candidate.original) == normalized_current
            }) {
                return candidates.clone();
            }
        }
        Vec::new()
    }

    fn fallback_targets_for(
        &self,
        local_path: Option<&Path>,
        current_record: &CdxRecord,
    ) -> (Vec<String>, Vec<CdxRecord>) {
        let discovered = self.discovered_candidates_for(local_path, current_record);
        let mut targets = Vec::new();
        let mut seen_targets = HashSet::new();
        for target in fallback_capture_targets(&current_record.original) {
            if seen_targets.insert(target.clone()) {
                targets.push(target);
            }
        }
        for candidate in &discovered {
            for target in fallback_capture_targets(&candidate.original) {
                if seen_targets.insert(target.clone()) {
                    targets.push(target);
                }
            }
        }
        (targets, discovered)
    }
}

/// Reuses exact CDX responses across fallback, linked, and static-asset passes.
async fn recovery_records(client: &WaybackClient, query: &CdxQuery, fallback: &FallbackOptions) -> Result<Vec<CdxRecord>> {
	let key = format!("{}#{:?}", query.search_url()?, query.strategy);
	if let Some(records) = fallback.recovery.lookup(&key) {
		return Ok(records);
	}
	let records = fetch_all_records_with_policy(client, query, CdxRetryPolicy::recovery()).await?;
	fallback.recovery.remember_lookup(key, records.clone());
	Ok(records)
}

const MAX_ALTERNATE_CDX_CONNECTIVITY_FAILURES: usize = 3;
const SNAPSHOT_ALTERNATE_CAPTURE_AFTER_ATTEMPTS: usize = 5;
const FIRST_VERBOSE_SNAPSHOT_RETRY_ATTEMPTS: usize = 5;
const SNAPSHOT_RETRY_LOG_EVERY_ATTEMPTS: usize = 10;

pub async fn download_site(
    client: WaybackClient,
    mapper: SiteMapper,
    query: CdxQuery,
    options: DownloadOptions,
) -> Result<DownloadReport> {
    let mut records = fetch_latest_records(&client, &query).await?;
    let discovered = records.len();
    let skipped_archive_noise = filter_archive_noise(&mut records);
    let (mut known_paths, candidates_by_path, selected_records_by_path) =
        index_records_by_local_path(&mapper, &records, query.strategy)?;
    let recovery_records_by_path = recoverable_static_records_by_path(
        &mapper,
        &records,
        query.strategy,
        options.extra_download_max_bytes.unwrap_or(u64::MAX),
    )?;

    fs::create_dir_all(&options.output_dir)
        .await
        .with_context(|| {
            format!(
                "failed to create output directory {}",
                options.output_dir.display()
            )
        })?;
    ensure_nojekyll_marker(&options.output_dir).await?;

    let mut jobs = Vec::new();
    let mut seen_paths = HashSet::new();
    let mut duplicate_paths = 0;
    for record in records {
        let Some(local_path) = known_paths
            .get(&normalize_lookup_url(&record.original))
            .cloned()
        else {
            continue;
        };

        if seen_paths.insert(local_path.clone()) {
            let selected = selected_records_by_path
                .get(&local_path)
                .cloned()
                .unwrap_or(record);
            jobs.push(DownloadJob {
                record: selected,
                local_path,
                max_bytes: None,
            });
        } else {
            duplicate_paths += 1;
        }
    }
    println!(
        "discovered {discovered} archived URLs; selected {} unique output paths",
        jobs.len()
    );
    if duplicate_paths > 0 {
        println!("skipping {duplicate_paths} duplicate local path mappings");
    }
    if skipped_archive_noise > 0 {
        println!("skipping {skipped_archive_noise} session/query/placeholder noise URLs");
    }

    let fallback_options = FallbackOptions {
        from: query.from.clone(),
        to: query.to.clone(),
        strategy: query.strategy,
		recovery: Arc::new(RecoveryState::open(&options.output_dir)?),
		originals_by_path: selected_records_by_path.clone(),
		candidates_by_path,
    };
    let archive_root = query.archive_root;

    let mut report = DownloadReport {
        discovered,
        skipped: duplicate_paths,
        output_dir: options.output_dir.clone(),
        ..DownloadReport::default()
    };
    let mut extra_download_refs = HashSet::new();

    let primary_total = jobs.len();
    for (index, job) in jobs.into_iter().enumerate() {
        let result = download_one(
            &client,
            &mapper,
            &archive_root,
            &options,
            &fallback_options,
            &known_paths,
            job,
        )
        .await;
        match result {
            Ok(DownloadStatus::Downloaded {
                extra_download_refs: refs,
                retained_unusable,
            }) => {
                report.downloaded += 1;
                report.retained_unusable_captures += usize::from(retained_unusable);
                extra_download_refs.extend(refs);
            }
            Ok(DownloadStatus::Skipped { extra_download_refs: refs }) => {
                report.skipped += 1;
                extra_download_refs.extend(refs);
            }
            Ok(DownloadStatus::SizeLimitExceeded) => report.skipped += 1,
            Ok(DownloadStatus::Cancelled) => report.cancelled += 1,
            Err(error) if is_unavailable_snapshot_error(&error) => {
                report.unavailable_snapshots += 1;
                eprintln!("snapshot unavailable: {error:#}");
            }
            Err(error) => {
                report.failed += 1;
                eprintln!("download failed: {error:#}");
            }
        }
        if (index + 1).is_multiple_of(100) || index + 1 == primary_total {
            println!("primary progress: {}/{}; downloaded {}; skipped {}; failed {}; unavailable {}",
                index + 1, primary_total, report.downloaded, report.skipped,
                report.failed, report.unavailable_snapshots);
        }
    }

    if options.rewrite_links && report.cancelled == 0 {
        if let Some(max_bytes) = options.extra_download_max_bytes {
			let mut checked_references = HashSet::new();
			while !extra_download_refs.is_empty() && !options.cancellation.is_cancelled() {
				extra_download_refs.retain(|reference| {
					checked_references.insert(canonical_extra_reference_key(&mapper, reference))
				});
				if extra_download_refs.is_empty() {
					break;
				}
				let extra_resolution = resolve_extra_download_jobs(
					&client,
					&archive_root,
					&mapper,
					&fallback_options,
					&extra_download_refs,
					&mut known_paths,
					&mut seen_paths,
					max_bytes,
					&options.cancellation,
				)
				.await?;
				extra_download_refs.clear();
				if !extra_resolution.jobs.is_empty() {
					if is_unlimited_extra_download_size(max_bytes) {
						println!("selected {} linked files without size cap", extra_resolution.jobs.len());
					} else {
						println!("selected {} linked files under {} bytes", extra_resolution.jobs.len(), max_bytes);
					}
				}
				if extra_resolution.unavailable > 0 {
					if is_unlimited_extra_download_size(max_bytes) {
						println!("linked files unavailable in Wayback: {}", extra_resolution.unavailable);
					} else {
						println!("linked files unavailable in Wayback or over size limit: {}", extra_resolution.unavailable);
					}
				}
				report.linked_files_unavailable += extra_resolution.unavailable;

				for job in extra_resolution.jobs {
					let result = download_one(
						&client,
						&mapper,
						&archive_root,
						&options,
						&fallback_options,
						&known_paths,
						job,
					)
					.await;
					match result {
						Ok(DownloadStatus::Downloaded { retained_unusable, extra_download_refs: refs }) => {
							report.downloaded += 1;
							report.retained_unusable_captures += usize::from(retained_unusable);
							report.extra_downloads += 1;
							extra_download_refs.extend(refs);
						}
						Ok(DownloadStatus::Skipped { extra_download_refs: refs }) => {
							report.skipped += 1;
							extra_download_refs.extend(refs);
						}
						Ok(DownloadStatus::SizeLimitExceeded) => {
							report.skipped += 1;
							report.linked_files_unavailable += 1;
						}
						Ok(DownloadStatus::Cancelled) => report.cancelled += 1,
						Err(error) if is_unavailable_snapshot_error(&error) => {
							report.unavailable_snapshots += 1;
							report.linked_files_unavailable += 1;
							eprintln!("linked file snapshot unavailable: {error:#}");
						}
						Err(error) => {
							report.failed += 1;
							eprintln!("download failed: {error:#}");
						}
					}
				}

				if !extra_resolution.redirects.is_empty() && !options.cancellation.is_cancelled() {
					let redirect_aliases = materialize_archived_redirects(
						&options.output_dir,
						&archive_root,
						&extra_resolution.redirects,
						options.add_wayback_comment,
					)
					.await?;
					if redirect_aliases > 0 {
						println!(
							"created {redirect_aliases} local redirects from archived 3xx captures"
						);
						report.aliases_created += redirect_aliases;
					}
				}
			}
        }

        if !options.cancellation.is_cancelled() {
            let alias_report = create_missing_topic_aliases(&options.output_dir)?;
            if alias_report.created > 0 {
                println!(
                    "created {} local alias files for missing topic links",
                    alias_report.created
                );
            }
            let directory_aliases = create_missing_directory_aliases(&options.output_dir)?;
            if directory_aliases > 0 {
                println!("created {directory_aliases} local directory index aliases");
            }
            report.aliases_created += alias_report.created + directory_aliases;
        }

        if let Some(max_bytes) = options.extra_download_max_bytes
            && !options.cancellation.is_cancelled()
        {
            let recovery_report = recover_missing_static_assets(
                &client,
                &archive_root,
                &mapper,
                &fallback_options,
                &recovery_records_by_path,
                &options.output_dir,
                max_bytes,
                &options.cancellation,
            )
            .await?;
            if recovery_report.recovered > 0 {
                println!(
                    "recovered {} missing static assets from Wayback",
                    recovery_report.recovered
                );
            }
            report.recovered_static_assets = recovery_report.recovered;
            report.unavailable_static_assets = recovery_report.unavailable;
        }

        let alternate_page_aliases = if !options.cancellation.is_cancelled() {
            let created = create_missing_static_asset_aliases_from_alternate_pages(
                &client,
                &archive_root,
                &fallback_options,
                &selected_records_by_path,
                &recovery_records_by_path,
                &options.output_dir,
                options.extra_download_max_bytes.unwrap_or(u64::MAX),
                &options.cancellation,
            )
            .await?;
            if created > 0 {
                println!("created {created} local aliases from alternate page captures");
            }
            created
        } else {
            0
        };

        let post_recovery_static_aliases = if !options.cancellation.is_cancelled() {
            let created =
                create_missing_static_asset_aliases(&options.output_dir, &options.cancellation)
                    .await?;
            if created > 0 {
                println!("created {created} local aliases for missing static assets");
            }
            created
        } else {
            0
        };
        report.static_asset_aliases_created =
            alternate_page_aliases + post_recovery_static_aliases;
        report.unavailable_static_assets = report
            .unavailable_static_assets
            .saturating_sub(report.static_asset_aliases_created);
        if report.unavailable_static_assets > 0 {
            if options
                .extra_download_max_bytes
                .is_some_and(is_unlimited_extra_download_size)
            {
                println!(
                    "missing static assets unavailable in Wayback: {}",
                    report.unavailable_static_assets
                );
            } else {
                println!(
                    "missing static assets unavailable in Wayback or over size limit: {}",
                    report.unavailable_static_assets
                );
            }
        }
        if options.cancellation.is_cancelled() {
            report.cancelled += 1;
            return Ok(report);
        }
    }

    if options.validate_links && report.cancelled == 0 {
        let validation_report = validate_local_links(&options.output_dir)?;
        report.local_links_checked = validation_report.checked;
        report.missing_local_links = validation_report.missing.len();
        report.unique_missing_targets = validation_report.unique_missing_targets();
        report.missing_image_sources = validation_report.missing_image_sources.len();
		report.unusable_html_files = validation_report.unusable_html.len();
		println!("unusable HTML files: {}", report.unusable_html_files);
        println!(
            "validated {} local links; missing {}; unique missing targets {}; images without source {}",
            validation_report.checked,
            validation_report.missing.len(),
            report.unique_missing_targets,
            validation_report.missing_image_sources.len()
        );

        for missing in validation_report.missing.iter().take(20) {
            let source = missing
                .source
                .strip_prefix(&options.output_dir)
                .unwrap_or(&missing.source);
            let target = missing
                .target
                .strip_prefix(&options.output_dir)
                .unwrap_or(&missing.target);
            println!(
                "missing local link: {} -> {} ({})",
                source.display(),
                missing.href,
                target.display()
            );
        }

        if validation_report.missing.len() > 20 {
            println!(
                "missing local links omitted: {}",
                validation_report.missing.len() - 20
            );
        }

        for missing in validation_report.missing_image_sources.iter().take(20) {
            let source = missing
                .source
                .strip_prefix(&options.output_dir)
                .unwrap_or(&missing.source);
            println!(
                "image without source: {} ({})",
                source.display(),
                missing.descriptor
            );
        }

        if validation_report.missing_image_sources.len() > 20 {
            println!(
                "images without source omitted: {}",
                validation_report.missing_image_sources.len() - 20
            );
        }
    }

    Ok(report)
}

/// Counts local navigation changes independently of downloaded or aliased files.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LocalLinkRepairReport {
	/// Number of anchor/area destinations rewritten to existing HTML files.
	pub repaired_links: usize,
	/// Number of HTML files atomically replaced by the repair.
	pub modified_files: usize,
}

/// Repairs existing HTML navigation offline without downloading or inventing content.
///
/// Only links with an existing HTML counterpart and no meaningful query are changed.
/// This is explicit rather than part of resume, which promises not to overwrite files.
pub async fn repair_local_links(root: &Path) -> Result<LocalLinkRepairReport> {
	let root = fs::canonicalize(root).await.context("failed to open local repair directory")?;
	ensure_nojekyll_marker(&root).await?;
	let mut files = Vec::new();
	collect_html_files(&root, &mut files)?;
	files.sort();
	let mut report = LocalLinkRepairReport::default();
	for file in files {
		let input = fs::read(&file).await.with_context(|| format!("failed to read {}", file.display()))?;
		let (output, repaired) = repair_local_navigation_html(&root, &file, &input)
			.with_context(|| format!("failed to repair {}", file.display()))?;
		if repaired > 0 {
			write_bytes_atomic(&file, &output).await?;
			report.repaired_links += repaired;
			report.modified_files += 1;
		}
	}
	Ok(report)
}

pub async fn repair_output_dir(
    client: WaybackClient,
    mapper: SiteMapper,
    options: RepairOptions,
) -> Result<RepairReport> {
    fs::create_dir_all(&options.output_dir)
        .await
        .with_context(|| {
            format!(
                "failed to create output directory {}",
                options.output_dir.display()
            )
        })?;
    ensure_nojekyll_marker(&options.output_dir).await?;

    let mut report = RepairReport {
        output_dir: options.output_dir.clone(),
        ..RepairReport::default()
    };

    if let Some(max_bytes) = options.extra_download_max_bytes {
        let records = fetch_recovery_records(&client, &mapper, &options).await?;
        let (_, candidates_by_path, selected_records_by_path) =
            index_records_by_local_path(&mapper, &records, options.strategy)?;
        let recovery_records_by_path =
            recoverable_static_records_by_path(&mapper, &records, options.strategy, max_bytes)?;
        let fallback_options = FallbackOptions {
            from: options.from.clone(),
            to: options.to.clone(),
            strategy: options.strategy,
			recovery: Arc::new(RecoveryState::default()),
			originals_by_path: selected_records_by_path.clone(),
			candidates_by_path,
        };
        let recovery_report = recover_missing_static_assets(
            &client,
            &options.archive_root,
            &mapper,
            &fallback_options,
            &recovery_records_by_path,
            &options.output_dir,
            max_bytes,
            &options.cancellation,
        )
        .await?;
        if recovery_report.recovered > 0 {
            println!(
                "recovered {} missing static assets from Wayback",
                recovery_report.recovered
            );
        }
        report.recovered_static_assets = recovery_report.recovered;
        report.unavailable_static_assets = recovery_report.unavailable;

        if !options.cancellation.is_cancelled() {
            let alternate_page_aliases = create_missing_static_asset_aliases_from_alternate_pages(
                &client,
                &options.archive_root,
                &fallback_options,
                &selected_records_by_path,
                &recovery_records_by_path,
                &options.output_dir,
                max_bytes,
                &options.cancellation,
            )
            .await?;
            if alternate_page_aliases > 0 {
                println!(
                    "created {alternate_page_aliases} local aliases from alternate page captures"
                );
            }
            report.static_asset_aliases_created += alternate_page_aliases;
        }
    }

    if !options.cancellation.is_cancelled() {
        let alias_report = create_missing_topic_aliases(&options.output_dir)?;
        if alias_report.created > 0 {
            println!(
                "created {} local alias files for missing topic links",
                alias_report.created
            );
        }
        let directory_aliases = create_missing_directory_aliases(&options.output_dir)?;
        if directory_aliases > 0 {
            println!("created {directory_aliases} local directory index aliases");
        }
        report.aliases_created = alias_report.created + directory_aliases;

        let static_asset_aliases =
            create_missing_static_asset_aliases(&options.output_dir, &options.cancellation).await?;
        if static_asset_aliases > 0 {
            println!("created {static_asset_aliases} local aliases for missing static assets");
        }
        report.static_asset_aliases_created += static_asset_aliases;
        report.unavailable_static_assets = report
            .unavailable_static_assets
            .saturating_sub(report.static_asset_aliases_created);
        if report.unavailable_static_assets > 0 {
            if options
                .extra_download_max_bytes
                .is_some_and(is_unlimited_extra_download_size)
            {
                println!(
                    "missing static assets unavailable in Wayback: {}",
                    report.unavailable_static_assets
                );
            } else {
                println!(
                    "missing static assets unavailable in Wayback or over size limit: {}",
                    report.unavailable_static_assets
                );
            }
        }
    }

    if options.validate_links && !options.cancellation.is_cancelled() {
        let validation_report = validate_local_links(&options.output_dir)?;
        report.local_links_checked = validation_report.checked;
        report.missing_local_links = validation_report.missing.len();
        report.unique_missing_targets = validation_report.unique_missing_targets();
        report.missing_image_sources = validation_report.missing_image_sources.len();
		report.unusable_html_files = validation_report.unusable_html.len();
		println!("unusable HTML files: {}", report.unusable_html_files);
        println!(
            "validated {} local links; missing {}; unique missing targets {}; images without source {}",
            validation_report.checked,
            validation_report.missing.len(),
            report.unique_missing_targets,
            validation_report.missing_image_sources.len()
        );

        for missing in validation_report.missing.iter().take(20) {
            let source = missing
                .source
                .strip_prefix(&options.output_dir)
                .unwrap_or(&missing.source);
            let target = missing
                .target
                .strip_prefix(&options.output_dir)
                .unwrap_or(&missing.target);
            println!(
                "missing local link: {} -> {} ({})",
                source.display(),
                missing.href,
                target.display()
            );
        }

        if validation_report.missing.len() > 20 {
            println!(
                "missing local links omitted: {}",
                validation_report.missing.len() - 20
            );
        }

        for missing in validation_report.missing_image_sources.iter().take(20) {
            let source = missing
                .source
                .strip_prefix(&options.output_dir)
                .unwrap_or(&missing.source);
            println!(
                "image without source: {} ({})",
                source.display(),
                missing.descriptor
            );
        }

        if validation_report.missing_image_sources.len() > 20 {
            println!(
                "images without source omitted: {}",
                validation_report.missing_image_sources.len() - 20
            );
        }
    }

    Ok(report)
}

pub async fn list_records(client: &WaybackClient, query: &CdxQuery) -> Result<Vec<CdxRecord>> {
    let mut records = fetch_latest_records(client, query).await?;
    filter_archive_noise(&mut records);
    Ok(records)
}

async fn fetch_recovery_records(
    client: &WaybackClient,
    mapper: &SiteMapper,
    options: &RepairOptions,
) -> Result<Vec<CdxRecord>> {
    let mut records = Vec::new();
    let mut seen = HashSet::new();
    let mut failed_targets = 0;
    let targets = recovery_cdx_targets(mapper, options.match_type);

    for target in &targets {
        let mut query = CdxQuery::new(
            target.clone(),
            options.match_type,
            options.strategy,
            options.archive_root.clone(),
        );
        query.from = options.from.clone();
        query.to = options.to.clone();
        query.limit = options.limit;

        let mut fetched = match fetch_latest_records(client, &query).await {
            Ok(fetched) => fetched,
            Err(error) => {
                failed_targets += 1;
                eprintln!("repair CDX lookup failed for {target}: {error:#}");
                continue;
            }
        };
        filter_archive_noise(&mut fetched);
        for record in fetched {
            if seen.insert((
                record.timestamp.clone(),
                normalize_lookup_url(&record.original),
            )) {
                records.push(record);
            }
        }
    }

    if records.is_empty() && failed_targets > 0 {
        bail!("failed to fetch any Wayback CDX records for repair");
    }

    Ok(records)
}

fn recovery_cdx_targets(mapper: &SiteMapper, match_type: MatchType) -> Vec<String> {
    match match_type {
        MatchType::Host => mapper.primary_hosts().to_vec(),
        MatchType::Domain | MatchType::Exact | MatchType::Prefix => {
            vec![mapper.cdx_target().to_owned()]
        }
    }
}

fn recoverable_static_records_by_path(
    mapper: &SiteMapper,
    records: &[CdxRecord],
    strategy: SnapshotStrategy,
    max_bytes: u64,
) -> Result<HashMap<PathBuf, CdxRecord>> {
    let mut strategy_ordered = records.to_vec();
    sort_records_for_strategy(&mut strategy_ordered, strategy);
    let mut records_by_path = HashMap::new();

    for record in &strategy_ordered {
        if record.length.is_some_and(|length| length > max_bytes) {
            continue;
        }

        let local_path = mapper.local_path_for_url(&record.original, &record.mimetype)?;
        if !is_recoverable_static_asset(&local_path) {
            continue;
        }

        records_by_path
            .entry(local_path)
            .or_insert_with(|| record.clone());
    }

    Ok(records_by_path)
}

fn index_records_by_local_path(
    mapper: &SiteMapper,
    records: &[CdxRecord],
    strategy: SnapshotStrategy,
) -> Result<(
    HashMap<String, PathBuf>,
    HashMap<PathBuf, Vec<CdxRecord>>,
    HashMap<PathBuf, CdxRecord>,
)> {
    let mut strategy_ordered = records.to_vec();
    sort_records_for_strategy(&mut strategy_ordered, strategy);
    let known_paths = mapper.records_to_paths(&strategy_ordered)?;
    let mut candidates_by_path: HashMap<PathBuf, Vec<CdxRecord>> = HashMap::new();

    for record in strategy_ordered {
        let key = normalize_lookup_url(&record.original);
        let local_path = match known_paths.get(&key) {
            Some(path) => path.clone(),
            None => mapper.local_path_for_url(&record.original, &record.mimetype)?,
        };
        candidates_by_path
            .entry(local_path)
            .or_default()
            .push(record);
    }

    let selected_records_by_path = candidates_by_path
        .iter()
        .filter_map(|(path, candidates)| {
            candidates
                .first()
                .cloned()
                .map(|record| (path.clone(), record))
        })
        .collect();

    Ok((known_paths, candidates_by_path, selected_records_by_path))
}

fn filter_archive_noise(records: &mut Vec<CdxRecord>) -> usize {
    let before = records.len();
    records.retain(|record| !is_archive_noise_record(record));
    before - records.len()
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DownloadStatus {
    Downloaded { extra_download_refs: Vec<String>, retained_unusable: bool },
    Skipped { extra_download_refs: Vec<String> },
    SizeLimitExceeded,
    Cancelled,
}

async fn download_one(
    client: &WaybackClient,
    mapper: &SiteMapper,
    archive_root: &Url,
    options: &DownloadOptions,
    fallback_options: &FallbackOptions,
    known_paths: &HashMap<String, PathBuf>,
    job: DownloadJob,
) -> Result<DownloadStatus> {
	let requested = job.record.clone();
	let local_path = job.local_path.clone();
	let result = download_one_inner(client, mapper, archive_root, options, fallback_options, known_paths, job).await;
	if let Err(error) = &result {
		fallback_options.recovery.record(CaptureEvent {
			record: requested, local_path,
			outcome: if is_unavailable_snapshot_error(error) { "unavailable" } else { "failed" }.into(),
			references: Vec::new(), error: Some(format!("{error:#}")),
		})?;
	}
	result
}

/// Reads preserved local references without replaying healthy pages or guessing query hashes.
fn resumed_references(mapper: &SiteMapper, options: &DownloadOptions, fallback: &FallbackOptions, job: &DownloadJob, bytes: &[u8]) -> Result<Vec<String>> {
	if !options.rewrite_links || options.extra_download_max_bytes.is_none()
		|| !should_rewrite_as_text(&job.record, &job.local_path) {
		return Ok(Vec::new());
	}
	let root = std::fs::canonicalize(&options.output_dir)?;
	let file = root.join(&job.local_path);
	let mut references = fallback.recovery.references(&job.local_path);
	for reference in extract_document_references(&String::from_utf8_lossy(bytes), is_css_mimetype(&job.record.mimetype))? {
		let value = reference.trim();
		if let Ok(url) = Url::parse(value) {
			references.push(url.to_string());
			continue;
		}
		if value.starts_with("//") {
			if let Ok(url) = Url::parse(&job.record.original)?.join(value) {
				references.push(url.to_string());
			}
			continue;
		}
		let Some(target) = resolve_local_reference(&root, &file, value) else { continue; };
		let relative = target.strip_prefix(&root)?;
		let original = fallback.originals_by_path.get(relative).map(|record| record.original.clone())
			.or_else(|| fallback.recovery.original_for_path(relative));
		if let Some(original) = original {
			references.push(original);
		} else if !relative.to_string_lossy().contains("__q_") && !target.is_file() {
			let mut original_path = relative.to_owned();
			if let Some(name) = relative.file_name().and_then(|name| name.to_str())
				&& [".php.html", ".cgi.html", ".asp.html", ".aspx.html", ".jsp.html", ".cfm.html"].iter().any(|suffix| name.ends_with(suffix)) {
				original_path.set_file_name(name.trim_end_matches(".html"));
			}
			let query = Url::parse("http://local.invalid/")?.join(value)?.query().map(str::to_owned);
			for candidate in mapper.original_url_candidates_for_local_path(&original_path) {
				let mut url = Url::parse(&candidate)?;
				url.set_query(query.as_deref());
				references.push(url.to_string());
			}
		}
	}
	references.retain(|reference| Url::parse(reference).is_ok_and(|url|
		matches!(url.scheme(), "http" | "https") && url.host_str().is_some_and(|host| mapper.is_related_host(host))
		&& !crate::noise::is_archive_noise_reference(reference)
		&& !is_interactive_forum_extra_reference(&url)));
	references.sort();
	references.dedup();
	Ok(references)
}

/// Downloads a replacement atomically, retaining existing bytes when recovery is unsuccessful.
async fn download_one_inner(
	client: &WaybackClient,
	mapper: &SiteMapper,
	archive_root: &Url,
	options: &DownloadOptions,
	fallback_options: &FallbackOptions,
	known_paths: &HashMap<String, PathBuf>,
	job: DownloadJob,
) -> Result<DownloadStatus> {
    if options.cancellation.is_cancelled() {
        return Ok(DownloadStatus::Cancelled);
    }

    let destination = options.output_dir.join(&job.local_path);
	let existing = has_non_empty_file(&destination).await?;
	let local_bytes = if existing && should_rewrite_as_text(&job.record, &job.local_path) {
		fs::read(&destination).await?
	} else { Vec::new() };
	let local_unusable = existing && should_detect_soft_redirect(&job.record, &job.local_path)
		&& is_unusable_html_capture(&String::from_utf8_lossy(&local_bytes));
	if existing && options.no_clobber && !(options.recover_existing && local_unusable) {
		return Ok(DownloadStatus::Skipped {
			extra_download_refs: resumed_references(mapper, options, fallback_options, &job, &local_bytes)?,
		});
	}

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }

    println!("capture {}: {} -> {}", job.record.timestamp, job.record.original, job.local_path.display());

    let mut retained_unusable = false;

    if should_buffer_response(&job.record, &job.local_path, options.rewrite_links) {
        let (available_record, body) = fetch_record_bytes_with_alternate_capture(
            client,
            archive_root,
            fallback_options,
            &job.record,
            &options.cancellation,
        )
        .await?;
        let (record, body) = maybe_replace_unusable_capture(
            client,
            archive_root,
            fallback_options,
            &options.cancellation,
            &available_record,
            body,
            &job.local_path,
        )
        .await?;

        if options.cancellation.is_cancelled() {
            return Ok(DownloadStatus::Cancelled);
        }

        let is_css = is_css_document(&record, &job.local_path);
        retained_unusable = should_detect_soft_redirect(&record, &job.local_path)
            && is_unusable_html_capture(&body.html_for_inspection());
		let text = if options.rewrite_links && should_rewrite_as_text(&record, &job.local_path) {
			Some(
				decode_text(
					&body.bytes,
					body.content_type.as_deref(),
					is_css,
				)
				.with_context(|| format!("failed to decode {}", record.original))?,
			)
		} else {
			None
		};

		let extra_download_refs = if let Some(text) = text.as_deref() {
            extract_related_references(text, &Url::parse(&record.original)?, mapper, is_css)?
        } else {
            Vec::new()
        };
		if existing && retained_unusable {
			eprintln!("keeping existing {}: no usable replacement capture found", job.local_path.display());
			fallback_options.recovery.record(CaptureEvent {
				record, local_path: job.local_path.clone(), outcome: "replacement_unusable".into(),
				references: extra_download_refs, error: Some("Existing file retained; replacement is an HTML error/redirect".into()),
			})?;
			return Ok(DownloadStatus::Skipped {
				extra_download_refs: resumed_references(mapper, options, fallback_options, &job, &local_bytes)?,
			});
		}

        if job
            .max_bytes
            .is_some_and(|limit| body.bytes.len() as u64 > limit)
        {
            eprintln!(
                "linked file exceeds configured size limit: {}",
                record.original
            );
            return Ok(DownloadStatus::SizeLimitExceeded);
        }

		if let Some(text) = text.as_deref() {
            let context = RewriteContext::new_with_mapper(
                &record.original,
                job.local_path.clone(),
                known_paths,
                mapper,
            )?;
            let mut rewritten = if is_css {
                rewrite_css(text, &context)
            } else {
                rewrite_html(text, &context)?
            };
            if !is_css && options.add_wayback_comment {
                let replay_url = wayback_replay_url(archive_root, &record)?;
                rewritten = insert_wayback_source_comment(&rewritten, replay_url.as_str());
            }
			write_bytes_atomic(&destination, &utf8_bytes(rewritten)).await?;
			fallback_options.recovery.record(CaptureEvent {
				record, local_path: job.local_path.clone(),
				outcome: if retained_unusable { "unusable" } else { "downloaded" }.into(),
				references: extra_download_refs.clone(), error: None,
			})?;
            return Ok(DownloadStatus::Downloaded {
                extra_download_refs,
                retained_unusable,
            });
        } else {
            write_bytes_atomic(&destination, &body.bytes).await?;
			fallback_options.recovery.record(CaptureEvent {
				record, local_path: job.local_path.clone(),
				outcome: if retained_unusable { "unusable" } else { "downloaded" }.into(),
				references: extra_download_refs, error: None,
			})?;
        }
    } else if let Some(max_bytes) = job.max_bytes {
        if !download_record_to_file_limited(
            client,
            archive_root,
            &job.record,
            &destination,
            max_bytes,
            Some(fallback_options),
            &options.cancellation,
        )
        .await?
        {
            eprintln!(
                "linked file exceeds configured size limit: {}",
                job.record.original
            );
            return Ok(DownloadStatus::SizeLimitExceeded);
        }
    } else {
        download_record_to_file(
            client,
            archive_root,
            &job.record,
            &destination,
            fallback_options,
            &options.cancellation,
        )
        .await?;
    }

    Ok(DownloadStatus::Downloaded {
        extra_download_refs: Vec::new(),
        retained_unusable,
    })
}

const MAX_ARCHIVED_REDIRECT_HOPS: usize = 5;

#[derive(Clone, Debug, Default)]
struct ExtraDownloadResolution {
    jobs: Vec<DownloadJob>,
    redirects: Vec<PendingArchivedRedirect>,
    unavailable: usize,
}

#[derive(Clone, Debug)]
struct PendingArchivedRedirect {
    redirect_record: CdxRecord,
    source_local_path: PathBuf,
    target_local_path: PathBuf,
}

fn canonical_extra_reference_key(mapper: &SiteMapper, reference: &str) -> String {
    let normalized = normalize_lookup_url(reference);
    let Ok(mut url) = Url::parse(&normalized) else {
        return normalized;
    };
    if matches!(url.scheme(), "http" | "https")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url
            .host_str()
            .is_some_and(|host| mapper.is_primary_host(host))
    {
        let _ = url.set_scheme("http");
        let _ = url.set_host(Some(mapper.cdx_target()));
        return url.to_string();
    }
    normalized
}

async fn resolve_extra_download_jobs(
    client: &WaybackClient,
    archive_root: &Url,
    mapper: &SiteMapper,
    fallback_options: &FallbackOptions,
    references: &HashSet<String>,
    known_paths: &mut HashMap<String, PathBuf>,
    seen_paths: &mut HashSet<PathBuf>,
    max_bytes: u64,
    cancellation: &CancellationFlag,
) -> Result<ExtraDownloadResolution> {
    let mut resolution = ExtraDownloadResolution::default();
    let mut references = references.iter().cloned().collect::<Vec<_>>();
    references.sort();

    let total = references.len();
    for (index, reference) in references.into_iter().enumerate() {
        if cancellation.is_cancelled() {
            break;
        }
        let lookup_key = normalize_lookup_url(&reference);
        if Url::parse(&reference).ok().and_then(|url| mapper.known_path_for_url(&url, known_paths)).is_some() {
            continue;
        }

        println!("linked lookup [{}/{}]: {}", index + 1, total, reference);

        let lookup = find_extra_download_record(
            client,
            archive_root,
            mapper,
            fallback_options,
            known_paths,
            &reference,
            max_bytes,
            cancellation,
        )
        .await?;
        match lookup {
            ExtraDownloadOutcome::Direct(record) => {
                let local_path = mapper.local_path_for_url(&record.original, &record.mimetype)?;
                known_paths.insert(lookup_key, local_path.clone());
                known_paths.insert(normalize_lookup_url(&record.original), local_path.clone());
                if seen_paths.insert(local_path.clone()) {
                    resolution.jobs.push(DownloadJob {
                        record,
                        local_path,
                        max_bytes: Some(max_bytes),
                    });
                }
            }
            ExtraDownloadOutcome::RedirectToDownload {
                redirect_record,
                final_record,
            } => {
                let target_local_path =
                    mapper.local_path_for_url(&final_record.original, &final_record.mimetype)?;
                let source_local_path =
                    mapper.local_path_for_url(&reference, &final_record.mimetype)?;
                known_paths.insert(
                    normalize_lookup_url(&final_record.original),
                    target_local_path.clone(),
                );
                known_paths.insert(lookup_key, source_local_path.clone());
                if seen_paths.insert(target_local_path.clone()) {
                    resolution.jobs.push(DownloadJob {
                        record: final_record,
                        local_path: target_local_path.clone(),
                        max_bytes: Some(max_bytes),
                    });
                }
                if source_local_path != target_local_path && seen_paths.insert(source_local_path.clone()) {
                    resolution.redirects.push(PendingArchivedRedirect {
                        redirect_record,
                        source_local_path,
                        target_local_path,
                    });
                }
            }
            ExtraDownloadOutcome::RedirectToExisting {
                redirect_record,
                target_local_path,
            } => {
                let target_mime = if is_html_path(&target_local_path) {
                    "text/html"
                } else {
                    "application/octet-stream"
                };
                let source_local_path = mapper.local_path_for_url(&reference, target_mime)?;
                known_paths.insert(lookup_key, source_local_path.clone());
                if source_local_path != target_local_path && seen_paths.insert(source_local_path.clone()) {
                    resolution.redirects.push(PendingArchivedRedirect {
                        redirect_record,
                        source_local_path,
                        target_local_path,
                    });
                }
            }
            ExtraDownloadOutcome::Unavailable => {
                resolution.unavailable += 1;
            }
        }
    }

    Ok(resolution)
}

async fn materialize_archived_redirects(
    output_dir: &Path,
    archive_root: &Url,
    redirects: &[PendingArchivedRedirect],
    add_wayback_comment: bool,
) -> Result<usize> {
    let mut created = 0usize;
    for redirect in redirects {
        let target_file = output_dir.join(&redirect.target_local_path);
        if !has_non_empty_file(&target_file).await? {
            continue;
        }
        let destination = output_dir.join(&redirect.source_local_path);
        if has_non_empty_file(&destination).await? {
            continue;
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
        if is_html_path(&redirect.source_local_path) {
            let relative = relative_link(&redirect.source_local_path, &redirect.target_local_path);
            let replay_url = wayback_replay_url(archive_root, &redirect.redirect_record)?;
            let stub = build_archived_redirect_stub(
                redirect.redirect_record.status_code,
                &relative,
                replay_url.as_str(),
                add_wayback_comment,
            );
            write_bytes_atomic(&destination, &utf8_bytes(stub)).await?;
        } else {
            fs::copy(&target_file, &destination).await.with_context(|| {
                format!(
                    "failed to materialize archived redirect {} from {}",
                    destination.display(),
                    target_file.display()
                )
            })?;
        }
        record_alias_event(
            output_dir,
            &AliasEvent {
                target_path: redirect.source_local_path.clone(),
                source_path: redirect.target_local_path.clone(),
                reason: format!(
                    "archived_http_{}_redirect",
                    redirect.redirect_record.status_code
                ),
                replaced_unusable: false,
                referred_from: None,
            },
        )?;
        created += 1;
    }
    Ok(created)
}

fn build_archived_redirect_stub(
    status_code: u16,
    relative_target: &str,
    replay_url: &str,
    add_wayback_comment: bool,
) -> String {
    let escaped_attr = html_escape::encode_double_quoted_attribute(relative_target);
    let escaped_text = html_escape::encode_text(relative_target);
    let comment_line = if add_wayback_comment {
        format!(
            "<!-- Archived HTTP {status_code} redirect from Wayback Machine: {} -->\n",
            sanitize_html_comment_url(replay_url)
        )
    } else {
        String::new()
    };
    format!(
        "<!DOCTYPE html>\n{comment_line}<html data-webarchive-redirect=\"{status_code}\"><head><meta charset=\"utf-8\"><meta http-equiv=\"refresh\" content=\"0; url={escaped_attr}\"><title>Redirecting</title></head><body><p>Archived HTTP {status_code} redirect to <a href=\"{escaped_attr}\">{escaped_text}</a>.</p></body></html>\n"
    )
}

fn is_html_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("html")
                || extension.eq_ignore_ascii_case("htm")
                || extension.eq_ignore_ascii_case("xhtml")
        })
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct StaticAssetRecoveryReport {
    recovered: usize,
    unavailable: usize,
}

/// Recovers available assets while counting exhausted replay failures as unavailable.
/// Lookup and filesystem errors remain fatal so incomplete recovery is not hidden.
async fn recover_missing_static_assets(
    client: &WaybackClient,
    archive_root: &Url,
    mapper: &SiteMapper,
    fallback_options: &FallbackOptions,
    records_by_path: &HashMap<PathBuf, CdxRecord>,
    output_dir: &Path,
    max_bytes: u64,
    cancellation: &CancellationFlag,
) -> Result<StaticAssetRecoveryReport> {
    let targets = missing_static_asset_targets(output_dir)?;
    if targets.is_empty() {
        return Ok(StaticAssetRecoveryReport::default());
    }

    println!(
        "checking {} missing static asset targets against Wayback",
        targets.len()
    );

    let mut report = StaticAssetRecoveryReport::default();
    for target in targets {
        if cancellation.is_cancelled() {
            break;
        }

        let destination = output_dir.join(&target);
        if has_non_empty_file(&destination).await? {
            continue;
        }

        let record = if let Some(record) = records_by_path.get(&target) {
            record.clone()
        } else {
            let lookup = find_missing_static_asset_record(
                client,
                archive_root,
                mapper,
                fallback_options,
                &target,
                max_bytes,
            )
            .await?;
            match lookup.record {
                Some(record) => record,
                None => {
                    report.unavailable += 1;
                    continue;
                }
            }
        };
        if record.length.is_some_and(|length| length > max_bytes) {
            report.unavailable += 1;
            continue;
        }

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }

        println!("{}", record.original);
        let downloaded = match download_record_to_file_limited(
            client,
            archive_root,
            &record,
            &destination,
            max_bytes,
            Some(fallback_options),
            cancellation,
        )
        .await
        {
            Ok(downloaded) => downloaded,
			Err(error) if is_unavailable_snapshot_error(&error) => {
				eprintln!("static asset snapshot unavailable: {error:#}");
				false
			}
            Err(error) => return Err(error),
        };
        if downloaded {
            report.recovered += 1;
        } else {
            report.unavailable += 1;
        }
    }

    Ok(report)
}

async fn create_missing_static_asset_aliases(
    output_dir: &Path,
    cancellation: &CancellationFlag,
) -> Result<usize> {
    let links = missing_static_asset_links(output_dir)?;
    let mut created = 0;

    for link in links {
        if cancellation.is_cancelled() {
            break;
        }

        let destination = output_dir.join(&link.target);
        if has_non_empty_file(&destination).await? {
            continue;
        }

        let Some((source, reason)) =
            existing_static_asset_alias_source(output_dir, &link.source, &link.target).await?
        else {
            continue;
        };
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
        fs::copy(&source, &destination).await.with_context(|| {
            format!(
                "failed to create static asset alias {} from {}",
                destination.display(),
                source.display()
            )
        })?;
        record_alias_event(
            output_dir,
            &AliasEvent {
                target_path: link.target.clone(),
                source_path: source.strip_prefix(output_dir).unwrap_or(&source).to_path_buf(),
                reason: reason.to_owned(),
                replaced_unusable: false,
                referred_from: Some(link.source.clone()),
            },
        )?;
        created += 1;
    }

    Ok(created)
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct MissingStaticAssetLink {
    source: PathBuf,
    target: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StaticAssetAliasRequest {
    target: PathBuf,
    candidate: PathBuf,
}

async fn create_missing_static_asset_aliases_from_alternate_pages(
    client: &WaybackClient,
    archive_root: &Url,
    fallback_options: &FallbackOptions,
    selected_records_by_path: &HashMap<PathBuf, CdxRecord>,
    recovery_records_by_path: &HashMap<PathBuf, CdxRecord>,
    output_dir: &Path,
    max_bytes: u64,
    cancellation: &CancellationFlag,
) -> Result<usize> {
    let output_dir = normalize_path(output_dir);
    let requests_by_source = missing_static_asset_alias_requests_by_source(
        recovery_records_by_path,
        &output_dir,
        selected_records_by_path,
    )
    .await?;
    if requests_by_source.is_empty() {
        return Ok(0);
    }

    let mut created = 0;
    let mut sources = requests_by_source.keys().cloned().collect::<Vec<_>>();
    sources.sort();
    let mut consecutive_connectivity_failures = 0;

    for source in sources {
        if cancellation.is_cancelled() {
            break;
        }

        let Some(source_record) = selected_records_by_path.get(&source) else {
            continue;
        };
        let mut requests = requests_by_source.get(&source).cloned().unwrap_or_default();
        if requests.is_empty() {
            continue;
        }

        let alternate_records = match alternate_text_records_for_source(
            client,
            archive_root,
            fallback_options,
            source_record,
            &source,
        )
        .await
        {
            Ok(records) => {
                consecutive_connectivity_failures = 0;
                records
            }
            Err(error) => {
                eprintln!(
                    "alternate capture CDX lookup failed for {}: {error:#}",
                    source_record.original
                );
                if is_cdx_connectivity_error(&error) {
                    consecutive_connectivity_failures += 1;
                    if consecutive_connectivity_failures >= MAX_ALTERNATE_CDX_CONNECTIVITY_FAILURES
                    {
                        eprintln!(
                            "stopping alternate-capture CDX lookups after {consecutive_connectivity_failures} consecutive network failures; remaining aliases will need a later repair run"
                        );
                        break;
                    }
                } else {
                    consecutive_connectivity_failures = 0;
                }
                continue;
            }
        };

        for alternate_record in alternate_records {
            if cancellation.is_cancelled() || requests.is_empty() {
                break;
            }

            println!("{}", alternate_record.original);
            let bytes =
                match fetch_record_bytes(client, archive_root, &alternate_record, cancellation)
                    .await
                {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        eprintln!(
                            "alternate capture failed for {} at {}: {error:#}",
                            alternate_record.original, alternate_record.timestamp
                        );
                        continue;
                    }
                };
            let text = bytes.html_for_inspection();

            let mut index = 0;
            while index < requests.len() {
                if !candidate_reference_appears(&text, &source, &requests[index].candidate) {
                    index += 1;
                    continue;
                }

                let request = requests.swap_remove(index);
                if create_static_asset_alias_from_candidate(
                    client,
                    archive_root,
                    fallback_options,
                    recovery_records_by_path,
                    &output_dir,
                    &source,
                    &request.target,
                    &request.candidate,
                    max_bytes,
                    cancellation,
                )
                .await?
                {
                    created += 1;
                }
            }
        }
    }

    Ok(created)
}

async fn missing_static_asset_alias_requests_by_source(
    recovery_records_by_path: &HashMap<PathBuf, CdxRecord>,
    output_dir: &Path,
    selected_records_by_path: &HashMap<PathBuf, CdxRecord>,
) -> Result<HashMap<PathBuf, Vec<StaticAssetAliasRequest>>> {
    let report = validate_local_links(output_dir)?;
    let mut requests_by_source: HashMap<PathBuf, Vec<StaticAssetAliasRequest>> = HashMap::new();
    let mut seen_requests = HashSet::new();

    for missing in report.missing {
        let Ok(source) = missing.source.strip_prefix(output_dir) else {
            continue;
        };
        let Ok(target) = missing.target.strip_prefix(output_dir) else {
            continue;
        };
        if !is_recoverable_static_asset(target) {
            continue;
        }
        if !selected_records_by_path.contains_key(source) {
            continue;
        }
        if has_non_empty_file(&output_dir.join(target)).await? {
            continue;
        }

        let candidates =
            available_static_asset_alias_candidates(recovery_records_by_path, output_dir, target)
                .await?;
        if candidates.len() != 1 {
            continue;
        }
        let candidate = candidates.into_iter().next().unwrap();
        let source = source.to_path_buf();
        let target = target.to_path_buf();
        if !seen_requests.insert((source.clone(), target.clone(), candidate.clone())) {
            continue;
        }

        requests_by_source
            .entry(source)
            .or_default()
            .push(StaticAssetAliasRequest { target, candidate });
    }

    for requests in requests_by_source.values_mut() {
        requests.sort_by(|left, right| {
            left.target
                .cmp(&right.target)
                .then(left.candidate.cmp(&right.candidate))
        });
    }

    Ok(requests_by_source)
}

async fn available_static_asset_alias_candidates(
    recovery_records_by_path: &HashMap<PathBuf, CdxRecord>,
    output_dir: &Path,
    target: &Path,
) -> Result<Vec<PathBuf>> {
    let mut candidates = Vec::new();
    for candidate in static_asset_alias_candidates(target) {
        if has_non_empty_file(&output_dir.join(&candidate)).await?
            || recovery_records_by_path.contains_key(&candidate)
        {
            candidates.push(candidate);
        }
    }

    Ok(candidates)
}

async fn alternate_text_records_for_source(
    client: &WaybackClient,
    archive_root: &Url,
    fallback_options: &FallbackOptions,
    source_record: &CdxRecord,
    source_path: &Path,
) -> Result<Vec<CdxRecord>> {
    let mut alternate_records = Vec::new();
    let mut seen_records = HashSet::new();
    let mut seen_digests = HashSet::new();
    remember_digest(&mut seen_digests, source_record);
    let (targets, discovered) =
        fallback_options.fallback_targets_for(Some(source_path), source_record);

    for (target_index, target) in targets.into_iter().enumerate() {
        let mut query = CdxQuery::new(
            target,
            MatchType::Exact,
            fallback_options.strategy,
            archive_root.clone(),
        );
        query.from = fallback_options.from.clone();
        query.to = fallback_options.to.clone();

        let mut records = recovery_records(client, &query, fallback_options).await?;
        if target_index == 0 && !discovered.is_empty() {
            records.extend(discovered.iter().cloned());
            sort_records_for_strategy(&mut records, fallback_options.strategy);
        }
        for record in records {
            if record.timestamp == source_record.timestamp
                && record.original == source_record.original
            {
                continue;
            }
            if !should_rewrite_as_text(&record, source_path) {
                continue;
            }
            if !remember_digest(&mut seen_digests, &record) {
                continue;
            }
            if !seen_records.insert((
                record.timestamp.clone(),
                normalize_lookup_url(&record.original),
            )) {
                continue;
            }

            alternate_records.push(record);
        }
    }

    Ok(alternate_records)
}

async fn create_static_asset_alias_from_candidate(
    client: &WaybackClient,
    archive_root: &Url,
    fallback_options: &FallbackOptions,
    recovery_records_by_path: &HashMap<PathBuf, CdxRecord>,
    output_dir: &Path,
    referred_from: &Path,
    target: &Path,
    candidate: &Path,
    max_bytes: u64,
    cancellation: &CancellationFlag,
) -> Result<bool> {
    if cancellation.is_cancelled() {
        return Ok(false);
    }

    let destination = output_dir.join(target);
    if has_non_empty_file(&destination).await? {
        return Ok(false);
    }

    let Some(source) = ensure_static_asset_alias_source(
        client,
        archive_root,
        fallback_options,
        recovery_records_by_path,
        output_dir,
        candidate,
        max_bytes,
        cancellation,
    )
    .await?
    else {
        return Ok(false);
    };

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    fs::copy(&source, &destination).await.with_context(|| {
        format!(
            "failed to create static asset alias {} from {}",
            destination.display(),
            source.display()
        )
    })?;
    record_alias_event(
        output_dir,
        &AliasEvent {
            target_path: target.to_path_buf(),
            source_path: candidate.to_path_buf(),
            reason: "static_asset_alternate_page".to_owned(),
            replaced_unusable: false,
            referred_from: Some(referred_from.to_path_buf()),
        },
    )?;

    Ok(true)
}

/// Finds an alias source without aborting recovery when all its replays are unavailable.
async fn ensure_static_asset_alias_source(
    client: &WaybackClient,
    archive_root: &Url,
    fallback_options: &FallbackOptions,
    recovery_records_by_path: &HashMap<PathBuf, CdxRecord>,
    output_dir: &Path,
    candidate: &Path,
    max_bytes: u64,
    cancellation: &CancellationFlag,
) -> Result<Option<PathBuf>> {
    let source = output_dir.join(candidate);
    if has_non_empty_file(&source).await? {
        return Ok(Some(source));
    }

    let Some(record) = recovery_records_by_path.get(candidate) else {
        return Ok(None);
    };
    if record.length.is_some_and(|length| length > max_bytes) {
        return Ok(None);
    }

    if let Some(parent) = source.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }

    println!("{}", record.original);
    let downloaded = match download_record_to_file_limited(
        client,
        archive_root,
        record,
        &source,
        max_bytes,
        Some(fallback_options),
        cancellation,
    )
    .await
    {
        Ok(downloaded) => downloaded,
		Err(error) if is_unavailable_snapshot_error(&error) => {
			eprintln!("static asset alias source unavailable: {error:#}");
			false
		}
        Err(error) => return Err(error),
    };
    if downloaded {
        Ok(Some(source))
    } else {
        Ok(None)
    }
}

async fn existing_static_asset_alias_source(
    output_dir: &Path,
    source: &Path,
    target: &Path,
) -> Result<Option<(PathBuf, &'static str)>> {
    let mut matches = Vec::new();
    for candidate in static_asset_alias_candidates(target) {
        if !local_candidate_reference_exists(output_dir, source, &candidate).await? {
            continue;
        }

        let candidate = output_dir.join(candidate);
        if has_non_empty_file(&candidate).await? {
            matches.push(candidate);
        }
    }
    if matches.len() == 1 {
        return Ok(matches
            .into_iter()
            .next()
            .map(|path| (path, "static_asset_filename_variant")));
    }

    if let Some(smiley) = same_host_smiley_alias_candidate(target) {
        let candidate = output_dir.join(smiley);
        if has_non_empty_file(&candidate).await? {
            return Ok(Some((candidate, "static_asset_smiley_dir")));
        }
    }

    if let Some(template_source) = same_host_template_image_alias_source(output_dir, target).await? {
        return Ok(Some((template_source, "static_asset_theme_icon")));
    }

    Ok(cross_host_static_asset_alias_source(output_dir, target)
        .await?
        .map(|path| (path, "static_asset_cross_host")))
}

fn same_host_smiley_alias_candidate(target: &Path) -> Option<PathBuf> {
    let components: Vec<_> = target
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect();
    for index in 0..components.len().saturating_sub(2) {
        if components[index].eq_ignore_ascii_case("images") {
            let replacement = if components[index + 1].eq_ignore_ascii_case("smiles") {
                Some("smilies")
            } else if components[index + 1].eq_ignore_ascii_case("smilies") {
                Some("smiles")
            } else {
                None
            };
            if let Some(replacement) = replacement {
                let mut candidate = PathBuf::new();
                for (i, part) in components.iter().enumerate() {
                    if i == index + 1 {
                        candidate.push(replacement);
                    } else {
                        candidate.push(part);
                    }
                }
                return Some(candidate);
            }
        }
    }
    None
}

async fn same_host_template_image_alias_source(
    output_dir: &Path,
    target: &Path,
) -> Result<Option<PathBuf>> {
    let components: Vec<_> = target
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect();
    for index in 0..components.len().saturating_sub(3) {
        if components[index].eq_ignore_ascii_case("templates")
            && components[index + 2].eq_ignore_ascii_case("images")
        {
            let mut templates_dir = output_dir.to_path_buf();
            for part in &components[..=index] {
                templates_dir.push(part);
            }
            let tail: PathBuf = components[index + 2..].iter().collect();

            let preferred = templates_dir.join("subSilver").join(&tail);
            if has_non_empty_file(&preferred).await? {
                return Ok(Some(preferred));
            }

            let mut entries = match fs::read_dir(&templates_dir).await {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to read directory {}", templates_dir.display())
                    });
                }
            };
            let mut candidates = Vec::new();
            while let Some(entry) = entries.next_entry().await? {
                if !entry.file_type().await?.is_dir() {
                    continue;
                }
                let candidate = entry.path().join(&tail);
                if has_non_empty_file(&candidate).await? {
                    candidates.push(candidate);
                }
            }
            candidates.sort();
            return unambiguous_file_source(&candidates).await;
        }
    }
    Ok(None)
}

fn strip_host_prefix(target: &Path) -> &Path {
    let mut components = target.components();
    if matches!(
        components.next(),
        Some(Component::Normal(first)) if first == "_hosts"
    ) && matches!(components.next(), Some(Component::Normal(_)))
    {
        let rest = components.as_path();
        if !rest.as_os_str().is_empty() {
            return rest;
        }
    }
    target
}

async fn cross_host_static_asset_alias_source(
    output_dir: &Path,
    target: &Path,
) -> Result<Option<PathBuf>> {
    let relative_subpath = strip_host_prefix(target);
    if relative_subpath.as_os_str().is_empty() {
        return Ok(None);
    }

    let mut subpaths = vec![relative_subpath.to_path_buf()];
    if let Some(smiley) = same_host_smiley_alias_candidate(relative_subpath) {
        subpaths.push(smiley);
    }

    let hosts_dir = output_dir.join("_hosts");
    let mut host_roots = Vec::new();
    if let Ok(mut entries) = fs::read_dir(&hosts_dir).await {
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                host_roots.push(entry.path());
            }
        }
    }
    host_roots.sort();

    for subpath in subpaths {
        let primary_candidate = output_dir.join(&subpath);
        if primary_candidate != output_dir.join(target)
            && has_non_empty_file(&primary_candidate).await?
        {
            return Ok(Some(primary_candidate));
        }

        let mut candidates = Vec::new();
        for host_root in &host_roots {
            let candidate = host_root.join(&subpath);
            if candidate != output_dir.join(target) && has_non_empty_file(&candidate).await? {
                candidates.push(candidate);
            }
        }
        if let Some(resolved) = unambiguous_file_source(&candidates).await? {
            return Ok(Some(resolved));
        }
    }

    Ok(None)
}

async fn unambiguous_file_source(candidates: &[PathBuf]) -> Result<Option<PathBuf>> {
    if candidates.is_empty() {
        return Ok(None);
    }
    if candidates.len() == 1 {
        return Ok(Some(candidates[0].clone()));
    }
    let first_bytes = fs::read(&candidates[0])
        .await
        .with_context(|| format!("failed to read {}", candidates[0].display()))?;
    for candidate in &candidates[1..] {
        let bytes = fs::read(candidate)
            .await
            .with_context(|| format!("failed to read {}", candidate.display()))?;
        if bytes != first_bytes {
            return Ok(None);
        }
    }
    Ok(Some(candidates[0].clone()))
}

fn static_asset_alias_candidates(target: &Path) -> Vec<PathBuf> {
    let Some(file_name) = target.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    let Some((stem, extension)) = file_name.rsplit_once('.') else {
        return Vec::new();
    };

    let Some(alias_stem) = static_asset_alias_stem(stem) else {
        return Vec::new();
    };

    let mut candidate = target.to_path_buf();
    candidate.set_file_name(format!("{alias_stem}.{extension}"));
    vec![candidate]
}

fn static_asset_alias_stem(stem: &str) -> Option<String> {
    if let Some(rest) = stem.strip_prefix("screen")
        && !rest.is_empty()
        && rest.starts_with(|character: char| character.is_ascii_digit())
    {
        return Some(format!("screenshot{rest}"));
    }

    if let Some(rest) = stem.strip_prefix("screenshot")
        && !rest.is_empty()
        && rest.starts_with(|character: char| character.is_ascii_digit())
    {
        return Some(format!("screen{rest}"));
    }

    None
}

fn candidate_reference_appears(input: &str, source: &Path, candidate: &Path) -> bool {
    let relative = relative_link(source, candidate);
    if !relative.is_empty() && input.contains(&relative) {
        return true;
    }

    let slash_path = slash_path(candidate);
    !slash_path.is_empty() && input.contains(&format!("/{slash_path}"))
}

fn slash_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(component) => Some(component.to_string_lossy().replace('\\', "/")),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

async fn local_candidate_reference_exists(
    output_dir: &Path,
    source: &Path,
    candidate: &Path,
) -> Result<bool> {
    let source_dir = source.parent().unwrap_or_else(|| Path::new(""));
    let search_dir = output_dir.join(source_dir);
    let mut entries = match fs::read_dir(&search_dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read directory {}", search_dir.display()));
        }
    };

    while let Some(entry) = entries
        .next_entry()
        .await
        .with_context(|| format!("failed to read directory {}", search_dir.display()))?
    {
        let file_type = entry
            .file_type()
            .await
            .with_context(|| format!("failed to read file type for {}", entry.path().display()))?;
        if !file_type.is_file() {
            continue;
        }

        let relative_file = source_dir.join(entry.file_name());
        if !is_reference_text_file(&relative_file) {
            continue;
        }

        let input = match fs::read_to_string(entry.path()).await {
            Ok(input) => input,
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read {}", entry.path().display()));
            }
        };
        if candidate_reference_appears(&input, &relative_file, candidate) {
            return Ok(true);
        }
    }

    Ok(false)
}

fn is_reference_text_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "css" | "htm" | "html" | "js" | "mjs" | "txt" | "xhtml"
            )
        })
}

fn missing_static_asset_links(output_dir: &Path) -> Result<Vec<MissingStaticAssetLink>> {
    let output_dir = normalize_path(output_dir);
    let report = validate_local_links(&output_dir)?;
    let mut links = BTreeSet::new();

    for missing in report.missing {
        let Ok(relative_source) = missing.source.strip_prefix(&output_dir) else {
            continue;
        };
        let Ok(relative_target) = missing.target.strip_prefix(&output_dir) else {
            continue;
        };
        if is_recoverable_static_asset(relative_target) {
            links.insert(MissingStaticAssetLink {
                source: relative_source.to_path_buf(),
                target: relative_target.to_path_buf(),
            });
        }
    }

    Ok(links.into_iter().collect())
}

fn missing_static_asset_targets(output_dir: &Path) -> Result<Vec<PathBuf>> {
    let output_dir = normalize_path(output_dir);
    let report = validate_local_links(&output_dir)?;
    let mut targets = BTreeSet::new();

    for missing in report.missing {
        let Ok(relative_target) = missing.target.strip_prefix(&output_dir) else {
            continue;
        };
        if is_recoverable_static_asset(relative_target) {
            targets.insert(relative_target.to_path_buf());
        }
    }

    Ok(targets.into_iter().collect())
}

fn is_recoverable_static_asset(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
        return false;
    };

    matches!(
        extension.to_ascii_lowercase().as_str(),
        "atom"
            | "bmp"
            | "css"
            | "eot"
            | "gif"
            | "ico"
            | "jpe"
            | "jpeg"
            | "jpg"
            | "js"
            | "json"
            | "m4a"
            | "mid"
            | "midi"
            | "mjs"
            | "mov"
            | "mp3"
            | "mp4"
            | "ogg"
            | "otf"
            | "pdf"
            | "png"
            | "rss"
            | "svg"
            | "swf"
            | "ttf"
            | "txt"
            | "wav"
            | "webm"
            | "webp"
            | "woff"
            | "woff2"
            | "xml"
    )
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ExtraDownloadLookup {
    record: Option<CdxRecord>,
}

/// Keeps unknown-length captures eligible; actual response sizes are checked on download.
fn extra_download_record_fits_limit(record: &CdxRecord, max_bytes: u64) -> bool {
    record.length.is_none_or(|length| length <= max_bytes)
}

async fn find_missing_static_asset_record(
    client: &WaybackClient,
    archive_root: &Url,
    mapper: &SiteMapper,
    fallback_options: &FallbackOptions,
    target: &Path,
    max_bytes: u64,
) -> Result<ExtraDownloadLookup> {
    let mut lookup = ExtraDownloadLookup::default();

    for candidate in mapper.original_url_candidates_for_local_path(target) {
        for target in fallback_capture_targets(&candidate) {
            let mut query = CdxQuery::new(
                target.clone(),
                MatchType::Exact,
                fallback_options.strategy,
                archive_root.clone(),
            );
            query.from = fallback_options.from.clone();
            query.to = fallback_options.to.clone();

            let records =
                match recovery_records(client, &query, fallback_options)
                    .await
                {
                    Ok(records) => records,
                    Err(error) if is_cdx_connectivity_error(&error) => return Err(error),
                    Err(error) => {
                        eprintln!("static asset CDX lookup failed for {target}: {error:#}");
                        continue;
                    }
                };

            for record in records {
                if record.length.is_none_or(|length| length <= max_bytes) {
                    lookup.record = Some(record);
                    return Ok(lookup);
                }
            }
        }
    }

    Ok(lookup)
}

enum ExtraDownloadOutcome {
    Direct(CdxRecord),
    RedirectToDownload {
        redirect_record: CdxRecord,
        final_record: CdxRecord,
    },
    RedirectToExisting {
        redirect_record: CdxRecord,
        target_local_path: PathBuf,
    },
    Unavailable,
}

async fn find_extra_download_record(
    client: &WaybackClient,
    archive_root: &Url,
    mapper: &SiteMapper,
    fallback_options: &FallbackOptions,
    known_paths: &HashMap<String, PathBuf>,
    reference: &str,
    max_bytes: u64,
    cancellation: &CancellationFlag,
) -> Result<ExtraDownloadOutcome> {
    let mut visited = HashSet::new();
    visited.insert(normalize_lookup_url(reference));

    let (direct, redirects) =
        query_extra_download_candidates(client, archive_root, fallback_options, reference, max_bytes)
            .await?;
    if let Some(record) = direct {
        return Ok(ExtraDownloadOutcome::Direct(record));
    }

    for first_redirect in redirects {
        if cancellation.is_cancelled() {
            break;
        }
        if let Some(resolved) = follow_archived_redirect_chain(
            client,
            archive_root,
            mapper,
            fallback_options,
            known_paths,
            first_redirect,
            max_bytes,
            &mut visited,
            cancellation,
        )
        .await?
        {
            return Ok(resolved);
        }
    }

    Ok(ExtraDownloadOutcome::Unavailable)
}

async fn query_extra_download_candidates(
    client: &WaybackClient,
    archive_root: &Url,
    fallback_options: &FallbackOptions,
    reference: &str,
    max_bytes: u64,
) -> Result<(Option<CdxRecord>, Vec<CdxRecord>)> {
    let mut redirect_candidates = Vec::new();
    let mut seen_redirects = HashSet::new();

    for target in extra_download_capture_targets(reference) {
        let mut query = CdxQuery::new(
            target.clone(),
            MatchType::Exact,
            fallback_options.strategy,
            archive_root.clone(),
        );
        query.from = fallback_options.from.clone();
        query.to = fallback_options.to.clone();
        query.include_redirects = true;

        let records = match recovery_records(client, &query, fallback_options).await {
            Ok(records) => records,
            Err(error) if is_cdx_connectivity_error(&error) => return Err(error),
            Err(error) => {
                eprintln!("extra download CDX lookup failed for {target}: {error:#}");
                continue;
            }
        };

        for record in &records {
            if record.status_code == 200 && extra_download_record_fits_limit(record, max_bytes) {
                return Ok((Some(record.clone()), Vec::new()));
            }
        }
        for record in records {
            if is_redirect_status_code(record.status_code)
                && seen_redirects.insert((record.timestamp.clone(), record.original.clone()))
            {
                redirect_candidates.push(record);
            }
        }
    }

    Ok((None, redirect_candidates))
}

async fn follow_archived_redirect_chain(
    client: &WaybackClient,
    archive_root: &Url,
    mapper: &SiteMapper,
    fallback_options: &FallbackOptions,
    known_paths: &HashMap<String, PathBuf>,
    first_redirect: CdxRecord,
    max_bytes: u64,
    visited: &mut HashSet<String>,
    cancellation: &CancellationFlag,
) -> Result<Option<ExtraDownloadOutcome>> {
    let mut current_redirect = first_redirect.clone();

    for _hop in 0..MAX_ARCHIVED_REDIRECT_HOPS {
        if cancellation.is_cancelled() {
            return Ok(None);
        }
        let Some(target_url) = fetch_archived_redirect_target(
            client,
            archive_root,
            mapper,
            &current_redirect,
            cancellation,
        )
        .await?
        else {
            return Ok(None);
        };

        if let Some(existing_path) = mapper.known_path_for_url(&target_url, known_paths) {
            return Ok(Some(ExtraDownloadOutcome::RedirectToExisting {
                redirect_record: first_redirect,
                target_local_path: existing_path.clone(),
            }));
        }

        let normalized_target = normalize_lookup_url(target_url.as_str());
        if !visited.insert(normalized_target) {
            return Ok(None);
        }

        let (direct, next_redirects) = query_extra_download_candidates(
            client,
            archive_root,
            fallback_options,
            target_url.as_str(),
            max_bytes,
        )
        .await?;
        if let Some(final_record) = direct {
            return Ok(Some(ExtraDownloadOutcome::RedirectToDownload {
                redirect_record: first_redirect,
                final_record,
            }));
        }
        let Some(next_redirect) = next_redirects.into_iter().next() else {
            return Ok(None);
        };
        current_redirect = next_redirect;
    }

    Ok(None)
}

async fn fetch_archived_redirect_target(
    client: &WaybackClient,
    archive_root: &Url,
    mapper: &SiteMapper,
    record: &CdxRecord,
    cancellation: &CancellationFlag,
) -> Result<Option<Url>> {
    let url = snapshot_url(archive_root, record)?;
    let mut attempt = 0usize;
    let started_at = Instant::now();
    let mut suppressed_retry_messages = 0usize;

    loop {
        if cancellation.is_cancelled() {
            return Ok(None);
        }
        let route = client.active_route_label();
        let response = match client.get_no_redirect(url.clone()).send().await {
            Ok(response) => response,
            Err(error) => {
                let error = anyhow::Error::new(error)
                    .context(format!("failed to inspect redirect {}", record.original));
                if try_activate_ssh_for_snapshot_error(client, record, &error) {
                    attempt = 0;
                    suppressed_retry_messages = 0;
                    continue;
                }
                retry_snapshot_after_error(
                    record,
                    &route,
                    &mut attempt,
                    started_at,
                    &mut suppressed_retry_messages,
                    &error,
                    cancellation,
                )
                .await?;
                continue;
            }
        };

        let status = response.status();
        if is_retryable_snapshot_status(status) {
            let headers = response.headers().clone();
            if try_activate_ssh_for_snapshot_status(client, record, status, attempt) {
                attempt = 0;
                suppressed_retry_messages = 0;
                continue;
            }
            retry_snapshot_after_status(
                record,
                &route,
                &mut attempt,
                started_at,
                &mut suppressed_retry_messages,
                &headers,
                status,
                cancellation,
            )
            .await?;
            continue;
        }
        if status == StatusCode::FORBIDDEN
            && try_activate_ssh_for_snapshot_status(client, record, status, attempt)
        {
            attempt = 0;
            suppressed_retry_messages = 0;
            continue;
        }
        if is_unavailable_snapshot_status(status) || !status.is_redirection() {
            return Ok(None);
        }

        let Some(target) =
            extract_archived_redirect_location(archive_root, &record.original, response.headers())
        else {
            return Ok(None);
        };
        if !target
            .host_str()
            .is_some_and(|host| mapper.is_related_host(host))
        {
            return Ok(None);
        }
        if normalize_lookup_url(target.as_str()) == normalize_lookup_url(&record.original) {
            return Ok(None);
        }
        return Ok(Some(target));
    }
}

fn extract_archived_redirect_location(
    archive_root: &Url,
    record_original: &str,
    headers: &HeaderMap,
) -> Option<Url> {
    let raw = headers
        .get("x-archive-orig-location")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            headers
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| unwrap_archive_root_url(archive_root, value))
        })?;

    let base = Url::parse(record_original).ok()?;
    let mut resolved = base.join(&raw).ok()?;
    resolved.set_fragment(None);
    matches!(resolved.scheme(), "http" | "https").then_some(resolved)
}

fn unwrap_archive_root_url(archive_root: &Url, value: &str) -> String {
    if let Ok(parsed) = Url::parse(value)
        && parsed.origin() == archive_root.origin()
        && let Some((_, original)) = parsed
            .path()
            .strip_prefix("/web/")
            .and_then(|path| path.split_once('/'))
        && let Ok(mut original_url) = Url::parse(original)
    {
        original_url.set_query(parsed.query());
        original_url.set_fragment(parsed.fragment());
        return original_url.to_string();
    }
    unwrap_wayback_url(value)
}

fn extra_download_capture_targets(reference: &str) -> Vec<String> {
    let stripped = strip_volatile_query_params(reference);
    let normalized = normalize_lookup_url(reference);
    if stripped == normalized {
        vec![stripped]
    } else {
        vec![stripped, normalized]
    }
}

fn should_buffer_response(record: &CdxRecord, local_path: &Path, rewrite_links: bool) -> bool {
    should_detect_soft_redirect(record, local_path)
        || (rewrite_links && should_rewrite_as_text(record, local_path))
}

/// Keeps transport encoding metadata attached to the exact replay body, including fallbacks.
struct SnapshotBody {
	bytes: Vec<u8>,
	content_type: Option<String>,
}

impl SnapshotBody {
	/// Error-page inspection must not prevent `--no-rewrite` from preserving original bytes.
	fn html_for_inspection(&self) -> Cow<'_, str> {
		decode_text(&self.bytes, self.content_type.as_deref(), false)
			.unwrap_or_else(|_| String::from_utf8_lossy(&self.bytes))
	}
}

async fn fetch_record_bytes(
    client: &WaybackClient,
    archive_root: &Url,
    record: &CdxRecord,
    cancellation: &CancellationFlag,
) -> Result<SnapshotBody> {
    let snapshot_url = snapshot_url(archive_root, record)?;
    let mut attempt = 0usize;
    let started_at = Instant::now();
    let mut suppressed_retry_messages = 0usize;

    loop {
        let route = client.active_route_label();
        match fetch_snapshot_response_once(client, &snapshot_url, record).await {
            Ok(SnapshotResponseAttempt::Ready(response)) => {
				let content_type = response
					.headers()
					.get("x-archive-orig-content-type")
					.or_else(|| response.headers().get(reqwest::header::CONTENT_TYPE))
					.and_then(|value| value.to_str().ok())
					.map(str::to_owned);
                match response
                    .bytes()
                    .await
                    .with_context(|| format!("failed to read {}", record.original))
                {
					Ok(bytes) => {
						return Ok(SnapshotBody {
							bytes: bytes.to_vec(),
							content_type,
						});
					}
                    Err(error) if is_retryable_snapshot_error(&error) => {
                        if try_activate_ssh_for_snapshot_error(client, record, &error) {
                            attempt = 0;
                            suppressed_retry_messages = 0;
                            continue;
                        }
                        retry_snapshot_after_error(
                            record,
                            &route,
                            &mut attempt,
                            started_at,
                            &mut suppressed_retry_messages,
                            &error,
                            cancellation,
                        )
                        .await?;
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(SnapshotResponseAttempt::RetryStatus { status, headers }) => {
                if try_activate_ssh_for_snapshot_status(client, record, status, attempt) {
                    attempt = 0;
                    suppressed_retry_messages = 0;
                    continue;
                }
                retry_snapshot_after_status(
                    record,
                    &route,
                    &mut attempt,
                    started_at,
                    &mut suppressed_retry_messages,
                    &headers,
                    status,
                    cancellation,
                )
                .await?;
            }
            Ok(SnapshotResponseAttempt::SshFallbackStatus { status }) => {
                if try_activate_ssh_for_snapshot_status(client, record, status, attempt) {
                    attempt = 0;
                    suppressed_retry_messages = 0;
                    continue;
                }
                bail!("Wayback returned {status} for {}", record.original);
            }
            Err(error) if is_retryable_snapshot_error(&error) => {
                if try_activate_ssh_for_snapshot_error(client, record, &error) {
                    attempt = 0;
                    suppressed_retry_messages = 0;
                    continue;
                }
                retry_snapshot_after_error(
                    record,
                    &route,
                    &mut attempt,
                    started_at,
                    &mut suppressed_retry_messages,
                    &error,
                    cancellation,
                )
                .await?;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Fetches buffered bytes and changes captures when a CDX-selected replay is unavailable.
async fn fetch_record_bytes_with_alternate_capture(
    client: &WaybackClient,
    archive_root: &Url,
    fallback_options: &FallbackOptions,
    record: &CdxRecord,
    cancellation: &CancellationFlag,
) -> Result<(CdxRecord, SnapshotBody)> {
    let mut current_record = record.clone();
    let mut current_snapshot_url = snapshot_url(archive_root, &current_record)?;
    let mut seen_records = HashSet::new();
    let mut alternate_switches = 0usize;
    remember_snapshot_record(&mut seen_records, &current_record);

    loop {
        match fetch_record_bytes(client, archive_root, &current_record, cancellation).await {
            Ok(bytes) => return Ok((current_record, bytes)),
            Err(error) => {
                let Some(status) = unavailable_snapshot_error_status(&error) else {
                    return Err(error);
                };
                if !should_try_alternate_snapshot_after_status(status) {
                    return Err(error);
                }
                let reason = format!("returned {status}");
                if switch_to_alternate_snapshot(
                    client,
                    archive_root,
                    Some(fallback_options),
                    &mut current_record,
                    &mut current_snapshot_url,
                    &mut seen_records,
                    &mut alternate_switches,
                    None,
                    &reason,
                    cancellation,
                )
                .await?
                {
                    continue;
                }
                return Err(error);
            }
        }
    }
}

enum SnapshotResponseAttempt {
    Ready(Response),
    RetryStatus {
        status: StatusCode,
        headers: HeaderMap,
    },
    SshFallbackStatus {
        status: StatusCode,
    },
}

async fn download_record_to_file(
    client: &WaybackClient,
    archive_root: &Url,
    record: &CdxRecord,
    destination: &Path,
    fallback_options: &FallbackOptions,
    cancellation: &CancellationFlag,
) -> Result<()> {
    let mut current_record = record.clone();
    let mut snapshot_url = snapshot_url(archive_root, &current_record)?;
    let mut attempt = 0usize;
    let mut started_at = Instant::now();
    let mut suppressed_retry_messages = 0usize;
    let mut seen_records = HashSet::new();
    let mut alternate_switches = 0usize;
    remember_snapshot_record(&mut seen_records, &current_record);

    loop {
        let route = client.active_route_label();
        match fetch_snapshot_response_once(client, &snapshot_url, &current_record).await {
            Ok(SnapshotResponseAttempt::Ready(response)) => {
                match stream_response_to_file(response, destination).await {
                    Ok(()) => return Ok(()),
                    Err(error) if is_retryable_snapshot_error(&error) => {
                        if try_activate_ssh_for_snapshot_error(client, &current_record, &error) {
                            attempt = 0;
                            suppressed_retry_messages = 0;
                            continue;
                        }
                        if maybe_switch_to_alternate_snapshot(
                            client,
                            archive_root,
                            Some(fallback_options),
                            &mut current_record,
                            &mut snapshot_url,
                            &mut seen_records,
                            &mut alternate_switches,
                            None,
                            attempt,
                            &error,
                            cancellation,
                        )
                        .await?
                        {
                            attempt = 0;
                            started_at = Instant::now();
                            suppressed_retry_messages = 0;
                            continue;
                        }
                        retry_snapshot_after_error(
                            &current_record,
                            &route,
                            &mut attempt,
                            started_at,
                            &mut suppressed_retry_messages,
                            &error,
                            cancellation,
                        )
                        .await?;
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(SnapshotResponseAttempt::RetryStatus { status, headers }) => {
                if try_activate_ssh_for_snapshot_status(client, &current_record, status, attempt) {
                    attempt = 0;
                    suppressed_retry_messages = 0;
                    continue;
                }
                retry_snapshot_after_status(
                    &current_record,
                    &route,
                    &mut attempt,
                    started_at,
                    &mut suppressed_retry_messages,
                    &headers,
                    status,
                    cancellation,
                )
                .await?;
            }
            Ok(SnapshotResponseAttempt::SshFallbackStatus { status }) => {
                if try_activate_ssh_for_snapshot_status(client, &current_record, status, attempt) {
                    attempt = 0;
                    suppressed_retry_messages = 0;
                    continue;
                }
                bail!("Wayback returned {status} for {}", current_record.original);
            }
            Err(error) if is_retryable_snapshot_error(&error) => {
                if try_activate_ssh_for_snapshot_error(client, &current_record, &error) {
                    attempt = 0;
                    suppressed_retry_messages = 0;
                    continue;
                }
                if maybe_switch_to_alternate_snapshot(
                    client,
                    archive_root,
                    Some(fallback_options),
                    &mut current_record,
                    &mut snapshot_url,
                    &mut seen_records,
                    &mut alternate_switches,
                    None,
                    attempt,
                    &error,
                    cancellation,
                )
                .await?
                {
                    attempt = 0;
                    started_at = Instant::now();
                    suppressed_retry_messages = 0;
                    continue;
                }
                retry_snapshot_after_error(
                    &current_record,
                    &route,
                    &mut attempt,
                    started_at,
                    &mut suppressed_retry_messages,
                    &error,
                    cancellation,
                )
                .await?;
            }
            Err(error)
                if unavailable_snapshot_error_status(&error)
                    .is_some_and(should_try_alternate_snapshot_after_status) =>
            {
                let status = unavailable_snapshot_error_status(&error).expect("status checked");
                let reason = format!("returned {status}");
                if switch_to_alternate_snapshot(
                    client,
                    archive_root,
                    Some(fallback_options),
                    &mut current_record,
                    &mut snapshot_url,
                    &mut seen_records,
                    &mut alternate_switches,
                    None,
                    &reason,
                    cancellation,
                )
                .await?
                {
                    attempt = 0;
                    started_at = Instant::now();
                    suppressed_retry_messages = 0;
                    continue;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }
}

async fn download_record_to_file_limited(
    client: &WaybackClient,
    archive_root: &Url,
    record: &CdxRecord,
    destination: &Path,
    max_bytes: u64,
    fallback_options: Option<&FallbackOptions>,
    cancellation: &CancellationFlag,
) -> Result<bool> {
    let mut current_record = record.clone();
    let mut snapshot_url = snapshot_url(archive_root, &current_record)?;
    let mut attempt = 0usize;
    let mut started_at = Instant::now();
    let mut suppressed_retry_messages = 0usize;
    let mut seen_records = HashSet::new();
    let mut alternate_switches = 0usize;
    remember_snapshot_record(&mut seen_records, &current_record);

    loop {
        let route = client.active_route_label();
        match fetch_snapshot_response_once(client, &snapshot_url, &current_record).await {
            Ok(SnapshotResponseAttempt::Ready(response)) => {
                match stream_response_to_file_limited(response, destination, max_bytes).await {
                    Ok(downloaded) => return Ok(downloaded),
                    Err(error) if is_retryable_snapshot_error(&error) => {
                        if try_activate_ssh_for_snapshot_error(client, &current_record, &error) {
                            attempt = 0;
                            suppressed_retry_messages = 0;
                            continue;
                        }
                        if maybe_switch_to_alternate_snapshot(
                            client,
                            archive_root,
                            fallback_options,
                            &mut current_record,
                            &mut snapshot_url,
                            &mut seen_records,
                            &mut alternate_switches,
                            Some(max_bytes),
                            attempt,
                            &error,
                            cancellation,
                        )
                        .await?
                        {
                            attempt = 0;
                            started_at = Instant::now();
                            suppressed_retry_messages = 0;
                            continue;
                        }
                        retry_snapshot_after_error(
                            &current_record,
                            &route,
                            &mut attempt,
                            started_at,
                            &mut suppressed_retry_messages,
                            &error,
                            cancellation,
                        )
                        .await?;
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(SnapshotResponseAttempt::RetryStatus { status, headers }) => {
                if try_activate_ssh_for_snapshot_status(client, &current_record, status, attempt) {
                    attempt = 0;
                    suppressed_retry_messages = 0;
                    continue;
                }
                retry_snapshot_after_status(
                    &current_record,
                    &route,
                    &mut attempt,
                    started_at,
                    &mut suppressed_retry_messages,
                    &headers,
                    status,
                    cancellation,
                )
                .await?;
            }
            Ok(SnapshotResponseAttempt::SshFallbackStatus { status }) => {
                if try_activate_ssh_for_snapshot_status(client, &current_record, status, attempt) {
                    attempt = 0;
                    suppressed_retry_messages = 0;
                    continue;
                }
                bail!("Wayback returned {status} for {}", current_record.original);
            }
            Err(error) if is_retryable_snapshot_error(&error) => {
                if try_activate_ssh_for_snapshot_error(client, &current_record, &error) {
                    attempt = 0;
                    suppressed_retry_messages = 0;
                    continue;
                }
                if maybe_switch_to_alternate_snapshot(
                    client,
                    archive_root,
                    fallback_options,
                    &mut current_record,
                    &mut snapshot_url,
                    &mut seen_records,
                    &mut alternate_switches,
                    Some(max_bytes),
                    attempt,
                    &error,
                    cancellation,
                )
                .await?
                {
                    attempt = 0;
                    started_at = Instant::now();
                    suppressed_retry_messages = 0;
                    continue;
                }
                retry_snapshot_after_error(
                    &current_record,
                    &route,
                    &mut attempt,
                    started_at,
                    &mut suppressed_retry_messages,
                    &error,
                    cancellation,
                )
                .await?;
            }
            Err(error)
                if unavailable_snapshot_error_status(&error)
                    .is_some_and(should_try_alternate_snapshot_after_status) =>
            {
                let status = unavailable_snapshot_error_status(&error).expect("status checked");
                let reason = format!("returned {status}");
                if switch_to_alternate_snapshot(
                    client,
                    archive_root,
                    fallback_options,
                    &mut current_record,
                    &mut snapshot_url,
                    &mut seen_records,
                    &mut alternate_switches,
                    Some(max_bytes),
                    &reason,
                    cancellation,
                )
                .await?
                {
                    attempt = 0;
                    started_at = Instant::now();
                    suppressed_retry_messages = 0;
                    continue;
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Switches a streaming snapshot download to another capture after repeated body read failures.
async fn maybe_switch_to_alternate_snapshot(
    client: &WaybackClient,
    archive_root: &Url,
    fallback_options: Option<&FallbackOptions>,
    current_record: &mut CdxRecord,
    current_snapshot_url: &mut Url,
    seen_records: &mut HashSet<(String, String)>,
    alternate_switches: &mut usize,
    max_bytes: Option<u64>,
    attempt: usize,
    error: &anyhow::Error,
    cancellation: &CancellationFlag,
) -> Result<bool> {
    let Some(fallback_options) = fallback_options else {
        return Ok(false);
    };
    if !should_try_alternate_snapshot_after_error(error, attempt) {
        return Ok(false);
    }

    match switch_to_alternate_snapshot(
        client,
        archive_root,
        Some(fallback_options),
        current_record,
        current_snapshot_url,
        seen_records,
        alternate_switches,
        max_bytes,
        "failed repeatedly while reading bytes",
        cancellation,
    )
    .await
    {
        Ok(switched) => Ok(switched),
        Err(error) => {
            eprintln!(
                "failed to look up alternate captures for {} after body read failures: {error:#}; continuing selected capture",
                current_record.original
            );
            Ok(false)
        }
    }
}

/// Switches to the next indexed capture after the current replay cannot be read.
async fn switch_to_alternate_snapshot(
    client: &WaybackClient,
    archive_root: &Url,
    fallback_options: Option<&FallbackOptions>,
    current_record: &mut CdxRecord,
    current_snapshot_url: &mut Url,
    seen_records: &mut HashSet<(String, String)>,
    alternate_switches: &mut usize,
    max_bytes: Option<u64>,
    reason: &str,
    cancellation: &CancellationFlag,
) -> Result<bool> {
    let Some(fallback_options) = fallback_options else {
        return Ok(false);
    };
    let alternate = match find_alternate_snapshot_record(
        client,
        archive_root,
        fallback_options,
        current_record,
        seen_records,
        max_bytes,
        cancellation,
    )
    .await?
    {
        Some(alternate) => alternate,
        None => {
            eprintln!(
                "no alternate capture found for {} after it {}",
                current_record.original, reason
            );
            return Ok(false);
        }
    };

    eprintln!(
        "snapshot for {} at {} {}; trying alternate capture {} at {}",
        current_record.original,
        current_record.timestamp,
        reason,
        alternate.original,
        alternate.timestamp
    );

    *current_record = alternate;
    *current_snapshot_url = snapshot_url(archive_root, current_record)?;
    *alternate_switches = (*alternate_switches).saturating_add(1);
    Ok(true)
}

/// Finds the next unique capture for the current original URL and date strategy.
async fn find_alternate_snapshot_record(
    client: &WaybackClient,
    archive_root: &Url,
    fallback_options: &FallbackOptions,
    current_record: &CdxRecord,
    seen_records: &mut HashSet<(String, String)>,
    max_bytes: Option<u64>,
    cancellation: &CancellationFlag,
) -> Result<Option<CdxRecord>> {
    let (targets, discovered) = fallback_options.fallback_targets_for(None, current_record);
    for (target_index, target) in targets.into_iter().enumerate() {
        let mut query = CdxQuery::new(
            target,
            MatchType::Exact,
            fallback_options.strategy,
            archive_root.clone(),
        );
        query.from = fallback_options.from.clone();
        query.to = fallback_options.to.clone();

        let mut records = recovery_records(client, &query, fallback_options).await?;
        if target_index == 0 && !discovered.is_empty() {
            records.extend(discovered.iter().cloned());
            sort_records_for_strategy(&mut records, fallback_options.strategy);
        }
        if let Some(candidate) = alternate_snapshot_candidate_from_records(
            records,
            seen_records,
            max_bytes,
            cancellation,
        ) {
            return Ok(Some(candidate));
        }
    }

    Ok(None)
}

/// Selects the next capture we have not already tried for this download.
fn alternate_snapshot_candidate_from_records(
    records: Vec<CdxRecord>,
    seen_records: &mut HashSet<(String, String)>,
    max_bytes: Option<u64>,
    cancellation: &CancellationFlag,
) -> Option<CdxRecord> {
    for record in records {
        if cancellation.is_cancelled() {
            return None;
        }
        if max_bytes.is_some_and(|limit| record.length.is_some_and(|length| length > limit)) {
            continue;
        }
        if !remember_snapshot_record(seen_records, &record) {
            continue;
        }

        return Some(record);
    }

    None
}

/// Returns true once a snapshot retry has enough body read failures to try another capture.
fn should_try_alternate_snapshot_after_error(error: &anyhow::Error, attempt: usize) -> bool {
    is_retryable_snapshot_body_error(error)
        && attempt.saturating_add(1) >= SNAPSHOT_ALTERNATE_CAPTURE_AFTER_ATTEMPTS
}

/// Remembers exact replay identities, independently of normalized local path identities.
fn remember_snapshot_record(
    seen_records: &mut HashSet<(String, String)>,
    record: &CdxRecord,
) -> bool {
    seen_records.insert((
        record.timestamp.clone(),
        record.original.clone(),
    ))
}

async fn fetch_snapshot_response_once(
    client: &WaybackClient,
    snapshot_url: &Url,
    record: &CdxRecord,
) -> Result<SnapshotResponseAttempt> {
    let response = client
        .get(snapshot_url.clone())
        .send()
        .await
        .with_context(|| format!("failed to request {}", record.original))?;
    let status = response.status();
    if is_retryable_snapshot_status(status) {
        return Ok(SnapshotResponseAttempt::RetryStatus {
            status,
            headers: response.headers().clone(),
        });
    }
    if status == StatusCode::FORBIDDEN {
        return Ok(SnapshotResponseAttempt::SshFallbackStatus { status });
    }

    let response = response
        .error_for_status()
        .map_err(|error| {
            let error = anyhow::Error::from(error);
            if is_unavailable_snapshot_status(status) {
                error.context(UnavailableSnapshot(status))
            } else {
                error
            }
        })
        .with_context(|| format!("Wayback returned an error for {}", record.original))?;
    Ok(SnapshotResponseAttempt::Ready(response))
}

async fn retry_snapshot_after_status(
    record: &CdxRecord,
    route: &str,
    attempt: &mut usize,
    started_at: Instant,
    suppressed_retry_messages: &mut usize,
    headers: &HeaderMap,
    status: StatusCode,
    cancellation: &CancellationFlag,
) -> Result<()> {
    let attempt_number = (*attempt).saturating_add(1);
    let delay = snapshot_retry_after_delay(headers, *attempt);
    if should_log_snapshot_retry_attempt(attempt_number) {
        eprintln!(
            "Wayback snapshot for {} returned {status} via {route} on attempt {} after {}{}; retrying in {}",
            record.original,
            attempt_number,
            format_snapshot_retry_elapsed(started_at.elapsed()),
            format_snapshot_suppressed_retries(*suppressed_retry_messages),
            format_retry_delay(delay)
        );
        *suppressed_retry_messages = 0;
    } else {
        *suppressed_retry_messages = (*suppressed_retry_messages).saturating_add(1);
    }

    *attempt = (*attempt).saturating_add(1);
    sleep_before_snapshot_retry(record, delay, cancellation).await
}

async fn retry_snapshot_after_error(
    record: &CdxRecord,
    route: &str,
    attempt: &mut usize,
    started_at: Instant,
    suppressed_retry_messages: &mut usize,
    error: &anyhow::Error,
    cancellation: &CancellationFlag,
) -> Result<()> {
    let attempt_number = (*attempt).saturating_add(1);
    let delay = snapshot_retry_after_delay(&HeaderMap::new(), *attempt);
    if should_log_snapshot_retry_attempt(attempt_number) {
        eprintln!(
            "Wayback snapshot for {} failed via {route} on attempt {} after {}{}: {}; retrying in {}",
            record.original,
            attempt_number,
            format_snapshot_retry_elapsed(started_at.elapsed()),
            format_snapshot_suppressed_retries(*suppressed_retry_messages),
            format_anyhow_error_chain(error),
            format_retry_delay(delay)
        );
        *suppressed_retry_messages = 0;
    } else {
        *suppressed_retry_messages = (*suppressed_retry_messages).saturating_add(1);
    }

    *attempt = (*attempt).saturating_add(1);
    sleep_before_snapshot_retry(record, delay, cancellation).await
}

async fn sleep_before_snapshot_retry(
    record: &CdxRecord,
    delay: Duration,
    cancellation: &CancellationFlag,
) -> Result<()> {
    let mut remaining = delay;
    while remaining > Duration::ZERO {
        if cancellation.is_cancelled() {
            bail!("cancelled while waiting to retry {}", record.original);
        }
        let step = remaining.min(Duration::from_secs(1));
        sleep(step).await;
        remaining = remaining.saturating_sub(step);
    }
    Ok(())
}

fn is_retryable_snapshot_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn is_unavailable_snapshot_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::NOT_FOUND | StatusCode::GONE | StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS
    )
}

fn should_try_alternate_snapshot_after_status(status: StatusCode) -> bool {
    is_unavailable_snapshot_status(status)
}

/// Marks replay errors so CDX lookup failures cannot be mistaken for missing content.
#[derive(Debug)]
struct UnavailableSnapshot(StatusCode);

impl std::fmt::Display for UnavailableSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "snapshot unavailable ({})", self.0)
    }
}

/// Returns only statuses attached to replay failures, not statuses from CDX queries.
fn unavailable_snapshot_error_status(error: &anyhow::Error) -> Option<StatusCode> {
    error
        .downcast_ref::<UnavailableSnapshot>()
        .map(|marker| marker.0)
}

fn is_unavailable_snapshot_error(error: &anyhow::Error) -> bool {
    unavailable_snapshot_error_status(error).is_some()
}

fn try_activate_ssh_for_snapshot_status(
    client: &WaybackClient,
    record: &CdxRecord,
    status: StatusCode,
    attempt: usize,
) -> bool {
    if !should_try_ssh_after_status(status, attempt, client.is_using_ssh()) {
        return false;
    }

    try_activate_ssh_client(
        client,
        &format!("snapshot for {} returned {status}", record.original),
    )
}

fn try_activate_ssh_for_snapshot_error(
    client: &WaybackClient,
    record: &CdxRecord,
    error: &anyhow::Error,
) -> bool {
    try_activate_ssh_client(
        client,
        &format!(
            "snapshot for {} failed: {}",
            record.original,
            format_anyhow_error_chain(error)
        ),
    )
}

fn try_activate_ssh_client(client: &WaybackClient, reason: &str) -> bool {
    if client.is_using_ssh() {
        match client.recover_from_active_ssh_failure(reason) {
            Ok(switched) => return switched,
            Err(error) => {
                eprintln!(
                    "SSH fallback recovery failed: {error:#}; continuing current Wayback retries"
                );
                return false;
            }
        }
    }

    match client.activate_ssh(reason) {
        Ok(activated) => activated,
        Err(error) => {
            eprintln!("SSH fallback failed: {error:#}; continuing direct Wayback retries");
            false
        }
    }
}

fn is_retryable_snapshot_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<reqwest::Error>().is_some_and(|error| {
            error.is_connect()
                || error.is_timeout()
                || error.is_body()
                || error.is_decode()
                || error.status().is_some_and(is_retryable_snapshot_status)
        })
    })
}

fn is_retryable_snapshot_body_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|error| error.is_body() || error.is_decode())
    })
}

fn snapshot_retry_after_delay(headers: &HeaderMap, attempt: usize) -> Duration {
    if let Some(seconds) = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_secs(seconds.clamp(1, MAX_WAYBACK_RETRY_DELAY_SECONDS));
    }

    let seconds = (5u64 << attempt.min(15)).min(MAX_WAYBACK_RETRY_DELAY_SECONDS);
    Duration::from_secs(seconds)
}

fn should_log_snapshot_retry_attempt(attempt_number: usize) -> bool {
    attempt_number <= FIRST_VERBOSE_SNAPSHOT_RETRY_ATTEMPTS
        || attempt_number.is_multiple_of(SNAPSHOT_RETRY_LOG_EVERY_ATTEMPTS)
}

fn format_snapshot_suppressed_retries(count: usize) -> String {
    match count {
        0 => String::new(),
        1 => " (1 similar retry message suppressed)".to_owned(),
        count => format!(" ({count} similar retry messages suppressed)"),
    }
}

fn format_anyhow_error_chain(error: &anyhow::Error) -> String {
    let mut message = String::new();
    for cause in error.chain() {
        let part = cause.to_string();
        if !message.contains(&part) {
            if !message.is_empty() {
                message.push_str(": ");
            }
            message.push_str(&part);
        }
    }
    message
}

fn format_snapshot_retry_elapsed(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;

    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

async fn maybe_replace_unusable_capture(
    client: &WaybackClient,
    archive_root: &Url,
    fallback_options: &FallbackOptions,
    cancellation: &CancellationFlag,
    current_record: &CdxRecord,
    current_bytes: SnapshotBody,
    local_path: &Path,
) -> Result<(CdxRecord, SnapshotBody)> {
    if !should_detect_soft_redirect(current_record, local_path) {
        return Ok((current_record.clone(), current_bytes));
    }

    let text = current_bytes.html_for_inspection();
    if !is_unusable_html_capture(&text) {
        return Ok((current_record.clone(), current_bytes));
    }

    println!(
        "unusable capture for {} at {}; trying another capture",
        current_record.original, current_record.timestamp
    );

    match find_usable_capture(
        client,
        archive_root,
        fallback_options,
        cancellation,
        current_record,
        local_path,
    )
    .await
    {
        Ok(Some(replacement)) => Ok(replacement),
        Ok(None) => {
            eprintln!(
                "no usable fallback capture found for {}; keeping selected capture",
                current_record.original
            );
            Ok((current_record.clone(), current_bytes))
        }
        Err(error) => {
            eprintln!(
                "failed to look up fallback captures for {}: {error:#}",
                current_record.original
            );
            Ok((current_record.clone(), current_bytes))
        }
    }
}

async fn find_usable_capture(
    client: &WaybackClient,
    archive_root: &Url,
    fallback_options: &FallbackOptions,
    cancellation: &CancellationFlag,
    current_record: &CdxRecord,
    local_path: &Path,
) -> Result<Option<(CdxRecord, SnapshotBody)>> {
    let identity_terms = site_identity_terms(&current_record.original);
    let mut seen_digests = HashSet::new();
    let mut seen_candidates = HashSet::new();
    remember_digest(&mut seen_digests, current_record);
    seen_candidates.insert((
        current_record.timestamp.clone(),
        current_record.original.clone(),
    ));
    let (targets, discovered) =
        fallback_options.fallback_targets_for(Some(local_path), current_record);
    for (target_index, target) in targets.into_iter().enumerate() {
        let mut query = CdxQuery::new(
            target,
            MatchType::Exact,
            fallback_options.strategy,
            archive_root.clone(),
        );
        query.from = fallback_options.from.clone();
        query.to = fallback_options.to.clone();

        let mut records = recovery_records(client, &query, fallback_options).await?;
        if target_index == 0 && !discovered.is_empty() {
            records.extend(discovered.iter().cloned());
            sort_records_for_strategy(&mut records, fallback_options.strategy);
        }
        for candidate in records {
            if cancellation.is_cancelled() {
                return Ok(None);
            }
            if !seen_candidates.insert((candidate.timestamp.clone(), candidate.original.clone())) {
                continue;
            }
            if !candidate.digest.is_empty() && candidate.digest != "-" && seen_digests.contains(&candidate.digest) {
                continue;
            }
            if !should_detect_soft_redirect(&candidate, local_path) {
                continue;
            }

            println!("fallback capture {}: {}", candidate.timestamp, candidate.original);
            let bytes =
                match fetch_record_bytes(client, archive_root, &candidate, cancellation).await {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        eprintln!(
                            "fallback capture failed for {} at {}: {error:#}",
                            candidate.original, candidate.timestamp
                        );
                        continue;
                    }
                };
            let text = bytes.html_for_inspection();
            remember_digest(&mut seen_digests, &candidate);
            if is_unusable_html_capture(&text) {
                println!(
                    "unusable capture for {} at {}; trying another capture",
                    candidate.original, candidate.timestamp
                );
                continue;
            }

            if !mentions_site_identity(&text, &identity_terms) {
                println!(
                    "fallback capture {} at {} does not mention {}; trying another capture",
                    candidate.original,
                    candidate.timestamp,
                    identity_terms.join("/")
                );
                continue;
            }

            return Ok(Some((candidate, bytes)));
        }
    }

    Ok(None)
}

fn fallback_capture_targets(original: &str) -> Vec<String> {
    let normalized = normalize_lookup_url(original);
    if normalized == original {
        vec![original.to_owned()]
    } else {
        vec![original.to_owned(), normalized]
    }
}

fn remember_digest(seen_digests: &mut HashSet<String>, record: &CdxRecord) -> bool {
    if record.digest.is_empty() || record.digest == "-" {
        return true;
    }
    seen_digests.insert(record.digest.clone())
}

fn should_detect_soft_redirect(record: &CdxRecord, local_path: &Path) -> bool {
    is_html_mimetype(&record.mimetype)
        || local_path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("html")
                    || extension.eq_ignore_ascii_case("htm")
                    || extension.eq_ignore_ascii_case("xhtml")
            })
}

fn site_identity_terms(original: &str) -> Vec<String> {
    let Ok(url) = Url::parse(original) else {
        return Vec::new();
    };
    let Some(host) = url.host_str() else {
        return Vec::new();
    };

    let host = host
        .trim_start_matches("www.")
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let mut terms = vec![host.clone()];
    if let Some(label) = host.split('.').next()
        && label.len() >= 4
    {
        terms.push(label.to_owned());
    }
    terms.sort();
    terms.dedup();
    terms
}

fn mentions_site_identity(input: &str, terms: &[String]) -> bool {
    if terms.is_empty() {
        return true;
    }

    let lower = input.to_ascii_lowercase();
    if terms.iter().any(|term| lower.contains(term)) {
        return true;
    }

    let compact_input = compact_identity_text(&lower);
    terms
        .iter()
        .map(|term| compact_identity_text(term))
        .any(|term| !term.is_empty() && compact_input.contains(&term))
}

fn compact_identity_text(input: &str) -> String {
    input
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect()
}

fn is_css_document(record: &CdxRecord, local_path: &Path) -> bool {
    is_css_mimetype(&record.mimetype)
        || (!is_html_mimetype(&record.mimetype)
            && local_path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("css")))
}

fn should_rewrite_as_text(record: &CdxRecord, local_path: &Path) -> bool {
    is_html_mimetype(&record.mimetype)
        || is_css_mimetype(&record.mimetype)
        || local_path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("css")
            })
}

async fn stream_response_to_file(response: reqwest::Response, destination: &Path) -> Result<()> {
    let temp_path = temp_path_for(destination);
    let result = async {
        let mut file = fs::File::create(&temp_path)
            .await
            .with_context(|| format!("failed to create {}", temp_path.display()))?;
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("failed while reading archived bytes")?;
            file.write_all(&chunk)
                .await
                .with_context(|| format!("failed to write {}", temp_path.display()))?;
        }

        file.flush()
            .await
            .with_context(|| format!("failed to flush {}", temp_path.display()))?;
        drop(file);
        fs::rename(&temp_path, destination).await.with_context(|| {
            format!(
                "failed to move {} to {}",
                temp_path.display(),
                destination.display()
            )
        })?;
        Ok(())
    }
    .await;

    if result.is_err() {
        let _ = fs::remove_file(&temp_path).await;
    }

    result
}

async fn stream_response_to_file_limited(
    response: reqwest::Response,
    destination: &Path,
    max_bytes: u64,
) -> Result<bool> {
    let temp_path = temp_path_for(destination);
    let result = async {
        if response
            .content_length()
            .is_some_and(|length| length > max_bytes)
        {
            return Ok(false);
        }

        let mut file = fs::File::create(&temp_path)
            .await
            .with_context(|| format!("failed to create {}", temp_path.display()))?;
        let mut stream = response.bytes_stream();
        let mut written = 0u64;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("failed while reading archived bytes")?;
            written = written.saturating_add(chunk.len() as u64);
            if written > max_bytes {
                return Ok(false);
            }
            file.write_all(&chunk)
                .await
                .with_context(|| format!("failed to write {}", temp_path.display()))?;
        }

        file.flush()
            .await
            .with_context(|| format!("failed to flush {}", temp_path.display()))?;
        drop(file);
        fs::rename(&temp_path, destination).await.with_context(|| {
            format!(
                "failed to move {} to {}",
                temp_path.display(),
                destination.display()
            )
        })?;
        Ok(true)
    }
    .await;

    if !matches!(result, Ok(true)) {
        let _ = fs::remove_file(&temp_path).await;
    }

    result
}

fn is_unlimited_extra_download_size(max_bytes: u64) -> bool {
    max_bytes == u64::MAX
}

/// Disables Jekyll so GitHub Pages publishes underscore-prefixed archive paths.
///
/// Creates an empty marker exclusively, preserving any existing regular file.
/// Other filesystem entries and write failures must not silently leave an unusable marker.
async fn ensure_nojekyll_marker(root: &Path) -> Result<()> {
    let marker = root.join(".nojekyll");
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .await
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(&marker)
                .await
                .with_context(|| format!("failed to inspect {}", marker.display()))?;
            if !metadata.is_file() {
                bail!("GitHub Pages marker {} must be a regular file", marker.display());
            }
            Ok(())
        }
        Err(error) => Err(error)
            .with_context(|| format!("failed to create GitHub Pages marker {}", marker.display())),
    }
}

async fn write_bytes_atomic(destination: &Path, bytes: &[u8]) -> Result<()> {
    let temp_path = temp_path_for(destination);
    let result = async {
        let mut file = fs::File::create(&temp_path)
            .await
            .with_context(|| format!("failed to create {}", temp_path.display()))?;
		if let Ok(metadata) = fs::metadata(destination).await {
			file.set_permissions(metadata.permissions()).await?;
		}
        file.write_all(bytes)
            .await
            .with_context(|| format!("failed to write {}", temp_path.display()))?;
        file.flush()
            .await
            .with_context(|| format!("failed to flush {}", temp_path.display()))?;
        drop(file);
        fs::rename(&temp_path, destination).await.with_context(|| {
            format!(
                "failed to move {} to {}",
                temp_path.display(),
                destination.display()
            )
        })?;
        Ok(())
    }
    .await;

    if result.is_err() {
        let _ = fs::remove_file(&temp_path).await;
    }

    result
}

fn temp_path_for(destination: &Path) -> PathBuf {
    let file_name = destination
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "download".into());
    destination.with_file_name(format!(".{file_name}.webarchive-downloader-rust.tmp"))
}

async fn has_non_empty_file(path: &Path) -> Result<bool> {
    match fs::metadata(path).await {
        Ok(metadata) => Ok(metadata.is_file() && metadata.len() > 0),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("failed to read metadata for {}", path.display()))
        }
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
        }
    }
    normalized
}

pub fn snapshot_url(archive_root: &Url, record: &CdxRecord) -> Result<Url> {
    archive_root
        .join(&format!("/web/{}id_/{}", record.timestamp, record.original))
        .context("failed to build Wayback snapshot URL")
}

/// Builds the browseable Wayback Machine replay URL for an archived capture.
pub fn wayback_replay_url(archive_root: &Url, record: &CdxRecord) -> Result<Url> {
    archive_root
        .join(&format!("/web/{}/{}", record.timestamp, record.original))
        .context("failed to build Wayback replay URL")
}

/// Inserts a non-visible Wayback Machine provenance comment at the top of an HTML document.
///
/// Places the comment right after any leading XML declaration and `<!DOCTYPE ...>` prologue
/// so browsers remain in standards mode and XHTML prologues stay well-formed.
pub fn insert_wayback_source_comment(html: &str, replay_url: &str) -> String {
    let comment = format!(
        "<!-- Archived from Wayback Machine: {} -->",
        sanitize_html_comment_url(replay_url)
    );
    let insert_at = html_prologue_end(html);
    let (prefix, rest) = html.split_at(insert_at);
    if rest.trim_start_matches(['\r', '\n']).starts_with(&comment) {
        return html.to_owned();
    }

    let mut output = String::with_capacity(html.len() + comment.len() + 4);
    output.push_str(prefix);
    if prefix.is_empty() || prefix == "\u{feff}" {
        output.push_str(&comment);
        output.push('\n');
        output.push_str(rest);
    } else if rest.starts_with("\r\n") {
        output.push_str("\r\n");
        output.push_str(&comment);
        output.push_str(rest);
    } else if rest.starts_with('\n') {
        output.push('\n');
        output.push_str(&comment);
        output.push_str(rest);
    } else {
        output.push('\n');
        output.push_str(&comment);
        output.push('\n');
        output.push_str(rest);
    }
    output
}

/// Percent-encodes comment-breaking sequences so archived URLs cannot close `<!-- ... -->`.
fn sanitize_html_comment_url(url: &str) -> String {
    url.replace("--", "%2D%2D")
        .replace('<', "%3C")
        .replace('>', "%3E")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// Returns the byte offset right after any leading BOM, XML declaration, and `<!DOCTYPE ...>`.
fn html_prologue_end(html: &str) -> usize {
    let mut pos = if html.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };

    let after_ws = skip_ascii_whitespace(html, pos);
    if let Some(xml_end) = xml_declaration_end(html, after_ws) {
        pos = xml_end;
    }

    let after_ws = skip_ascii_whitespace(html, pos);
    if let Some(doctype_end) = doctype_declaration_end(html, after_ws) {
        pos = doctype_end;
    }

    pos
}

fn skip_ascii_whitespace(input: &str, start: usize) -> usize {
    let bytes = input.as_bytes();
    let mut index = start;
    while index < bytes.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    index
}

fn xml_declaration_end(input: &str, start: usize) -> Option<usize> {
    let bytes = &input.as_bytes()[start..];
    if bytes.len() < 5 || !bytes[..5].eq_ignore_ascii_case(b"<?xml") {
        return None;
    }
    let next = *bytes.get(5)?;
    if !next.is_ascii_whitespace() && next != b'?' {
        return None;
    }
    let close = input[start + 5..].find("?>")?;
    Some(start + 5 + close + 2)
}

fn doctype_declaration_end(input: &str, start: usize) -> Option<usize> {
    let bytes = &input.as_bytes()[start..];
    if bytes.len() < 9 || !bytes[..9].eq_ignore_ascii_case(b"<!doctype") {
        return None;
    }
    let next = *bytes.get(9)?;
    if !next.is_ascii_whitespace() && next != b'>' {
        return None;
    }
    let mut quote: Option<u8> = None;
    let mut bracket_depth = 0usize;
    for (offset, &byte) in bytes.iter().enumerate().skip(9) {
        if let Some(active_quote) = quote {
            if byte == active_quote {
                quote = None;
            }
            continue;
        }
        match byte {
            b'"' | b'\'' => quote = Some(byte),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b'>' if bracket_depth == 0 => return Some(start + offset + 1),
            _ => {}
        }
    }
    None
}

pub fn build_client(
    user_agent: &str,
    timeout: Duration,
    ssh_destinations: Vec<String>,
) -> Result<WaybackClient> {
    WaybackClient::new(user_agent, timeout, ssh_destinations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

	/// Distinct replay originals can share a local path without sharing availability.
	#[test]
	fn replay_attempts_keep_original_host_and_query_variants() {
		let mut seen = HashSet::new();
		let record = CdxRecord {
			timestamp: "20200101000000".into(), original: "http://www.example.com/topic.php?t=7&sid=one".into(),
			mimetype: "text/html".into(), status_code: 200, digest: "SAME".into(), length: None,
		};
		assert!(remember_snapshot_record(&mut seen, &record));
		for original in [
			"http://example.com/topic.php?t=7&sid=one",
			"http://www.example.com/topic.php?t=7&sid=two",
			"http://www.example.com:80/topic.php?t=7&sid=one",
		] {
			assert!(remember_snapshot_record(&mut seen, &CdxRecord { original: original.into(), ..record.clone() }), "{original}");
		}
		assert!(!remember_snapshot_record(&mut seen, &record));
	}

	/// Cached exact queries must not replace a different request with an earlier empty result.
	#[tokio::test]
	async fn recovery_lookup_cache_keeps_explicit_port_queries_distinct() {
		use wiremock::matchers::{path, query_param};
		use wiremock::{Mock, MockServer, ResponseTemplate};
		let server = MockServer::start().await;
		let first = "http://example.com:80/page.html";
		let second = "http://example.com/page.html";
		for (original, rows) in [(first, String::new()), (second, format!("20200101000000 {second} text/html 200 PAGE 100\n"))] {
			Mock::given(path("/cdx/search/cdx")).and(query_param("url", original))
				.respond_with(ResponseTemplate::new(200).set_body_string(rows)).expect(1).mount(&server).await;
		}
		let client = build_client("lookup-test", Duration::from_secs(5), Vec::new()).unwrap();
		let fallback = FallbackOptions { from: None, to: None, strategy: SnapshotStrategy::Latest, recovery: Arc::new(RecoveryState::default()), originals_by_path: HashMap::new(), candidates_by_path: HashMap::new() };
		let query = CdxQuery::new(first.into(), MatchType::Exact, SnapshotStrategy::Latest, Url::parse(&server.uri()).unwrap());
		assert!(recovery_records(&client, &query, &fallback).await.unwrap().is_empty());
		let query = CdxQuery { target: second.into(), ..query };
		assert_eq!(recovery_records(&client, &query, &fallback).await.unwrap().len(), 1);
		assert_eq!(recovery_records(&client, &query, &fallback).await.unwrap().len(), 1);
	}

	/// A working replay with another session query must still be tried after a same-timestamp 404.
	#[tokio::test]
	async fn unavailable_replay_recovers_from_same_timestamp_query_variant() {
		use wiremock::matchers::{path, query_param};
		use wiremock::{Mock, MockServer, ResponseTemplate};
		let server = MockServer::start().await;
		let original = "http://example.com/topic.php?t=7&sid=one";
		let alternate = "http://example.com/topic.php?t=7&sid=two";
		let timestamp = "20200101000000";
		Mock::given(path("/cdx/search/cdx")).and(query_param("url", original))
			.respond_with(ResponseTemplate::new(200).set_body_string(format!("{timestamp} {alternate} text/html 200 PAGE 100\n")))
			.expect(1).mount(&server).await;
		for (query, status, body) in [("one", 404, "Missing"), ("two", 200, "<p>Preserved topic</p>")] {
			Mock::given(path(format!("/web/{timestamp}id_/http://example.com/topic.php")))
				.and(query_param("sid", query)).respond_with(ResponseTemplate::new(status).set_body_string(body))
				.expect(1).mount(&server).await;
		}
		let client = build_client("replay-test", Duration::from_secs(5), Vec::new()).unwrap();
		let record = CdxRecord { timestamp: timestamp.into(), original: original.into(), mimetype: "text/html".into(), status_code: 200, digest: "PAGE".into(), length: None };
		let fallback = FallbackOptions { from: None, to: None, strategy: SnapshotStrategy::Latest, recovery: Arc::new(RecoveryState::default()), originals_by_path: HashMap::new(), candidates_by_path: HashMap::new() };
		let (saved, body) = fetch_record_bytes_with_alternate_capture(&client, &Url::parse(&server.uri()).unwrap(), &fallback, &record, &CancellationFlag::new()).await.unwrap();
		assert_eq!(saved.original, alternate);
		assert_eq!(body.bytes, b"<p>Preserved topic</p>");
	}

	/// Static repair must inspect unique older page bodies beyond the former twenty-capture cap.
	#[tokio::test]
	async fn keeps_all_alternate_page_candidates() {
		let server = wiremock::MockServer::start().await;
		let original = "http://example.com/page.html";
		let rows = (1..=23).map(|day| format!("202001{day:02}000000 {original} text/html 200 DIGEST{day} 100\n")).collect::<String>();
		wiremock::Mock::given(wiremock::matchers::path("/cdx/search/cdx"))
			.respond_with(wiremock::ResponseTemplate::new(200).set_body_string(rows))
			.mount(&server).await;
		let client = build_client("capture-test", Duration::from_secs(5), Vec::new()).unwrap();
		let source = CdxRecord { timestamp: "20200201000000".into(), original: original.into(), mimetype: "text/html".into(), status_code: 200, digest: "SOURCE".into(), length: None };
		let fallback = FallbackOptions { from: None, to: None, strategy: SnapshotStrategy::Latest, recovery: Arc::new(RecoveryState::default()), originals_by_path: HashMap::new(), candidates_by_path: HashMap::new() };
		let candidates = alternate_text_records_for_source(&client, &Url::parse(&server.uri()).unwrap(), &fallback, &source, Path::new("page.html")).await.unwrap();
		assert_eq!(candidates.len(), 23);
	}

	/// A truncated replay remains eligible for a new candidate after twenty previous switches.
	#[tokio::test]
	async fn body_failure_can_switch_beyond_twenty_captures() {
		use std::io::{Read, Write};
		let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		let address = listener.local_addr().unwrap();
		let responder = std::thread::spawn(move || {
			let (mut connection, _) = listener.accept().unwrap();
			connection.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
			let mut buffer = [0; 4096];
			let mut request = Vec::new();
			while !request.windows(4).any(|window| window == b"\r\n\r\n") {
				let count = connection.read(&mut buffer).unwrap();
				assert!(count > 0, "client closed before sending request headers");
				request.extend_from_slice(&buffer[..count]);
			}
			connection.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort").unwrap();
		});
		let response = reqwest::Client::new().get(format!("http://{address}")).timeout(Duration::from_secs(5)).send().await.unwrap();
		let error = anyhow::Error::from(response.bytes().await.unwrap_err());
		responder.join().unwrap();
		assert!(is_retryable_snapshot_body_error(&error));
		let server = wiremock::MockServer::start().await;
		wiremock::Mock::given(wiremock::matchers::path("/cdx/search/cdx"))
			.respond_with(wiremock::ResponseTemplate::new(200).set_body_string("20200101000000 http://example.com/game.zip application/zip 200 FILE 100\n"))
			.mount(&server).await;
		let client = build_client("capture-test", Duration::from_secs(5), Vec::new()).unwrap();
		let root = Url::parse(&server.uri()).unwrap();
		let mut current = CdxRecord { timestamp: "20200201000000".into(), original: "http://example.com/game.zip".into(), mimetype: "application/zip".into(), status_code: 200, digest: "FILE".into(), length: None };
		let mut replay = snapshot_url(&root, &current).unwrap();
		let mut seen = HashSet::new();
		remember_snapshot_record(&mut seen, &current);
		let mut switches = 20;
		let fallback = FallbackOptions { from: None, to: None, strategy: SnapshotStrategy::Latest, recovery: Arc::new(RecoveryState::default()), originals_by_path: HashMap::new(), candidates_by_path: HashMap::new() };
		assert!(maybe_switch_to_alternate_snapshot(&client, &root, Some(&fallback), &mut current, &mut replay, &mut seen, &mut switches, None, 4, &error, &CancellationFlag::new()).await.unwrap());
		assert_eq!(switches, 21);
		assert_eq!(current.timestamp, "20200101000000");
	}

    #[test]
    fn builds_identity_snapshot_url() {
        let root = Url::parse("https://web.archive.org").unwrap();
        let record = CdxRecord {
            timestamp: "20200102030405".to_owned(),
            original: "http://example.com/".to_owned(),
            mimetype: "text/html".to_owned(),
            status_code: 200,
            digest: "digest".to_owned(),
            length: None,
        };

        assert_eq!(
            snapshot_url(&root, &record).unwrap().as_str(),
            "https://web.archive.org/web/20200102030405id_/http://example.com/"
        );
    }

    #[test]
    fn retries_only_transient_snapshot_statuses() {
        assert!(is_retryable_snapshot_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_snapshot_status(StatusCode::BAD_GATEWAY));
        assert!(!is_retryable_snapshot_status(StatusCode::NOT_FOUND));
        assert!(!is_retryable_snapshot_status(StatusCode::FORBIDDEN));
    }

    #[test]
    fn classifies_permanently_unavailable_snapshot_statuses() {
        assert!(is_unavailable_snapshot_status(StatusCode::NOT_FOUND));
        assert!(is_unavailable_snapshot_status(StatusCode::GONE));
        assert!(is_unavailable_snapshot_status(
            StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS
        ));
        assert!(!is_unavailable_snapshot_status(StatusCode::FORBIDDEN));
        assert!(!is_unavailable_snapshot_status(
            StatusCode::TOO_MANY_REQUESTS
        ));
        assert!(!is_unavailable_snapshot_status(StatusCode::BAD_GATEWAY));
    }

    #[test]
    fn tries_alternate_capture_for_unavailable_selected_snapshot() {
        assert!(should_try_alternate_snapshot_after_status(
            StatusCode::NOT_FOUND
        ));
        assert!(should_try_alternate_snapshot_after_status(StatusCode::GONE));
        assert!(!should_try_alternate_snapshot_after_status(
            StatusCode::TOO_MANY_REQUESTS
        ));
    }

    #[test]
    fn treats_forbidden_snapshot_status_as_ssh_fallback_only() {
        assert!(!is_retryable_snapshot_status(StatusCode::FORBIDDEN));
    }

    #[test]
    fn caps_snapshot_retry_delay() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("999999"));

        assert_eq!(
            snapshot_retry_after_delay(&headers, 0),
            Duration::from_secs(MAX_WAYBACK_RETRY_DELAY_SECONDS)
        );
        assert_eq!(
            snapshot_retry_after_delay(&HeaderMap::new(), 100),
            Duration::from_secs(MAX_WAYBACK_RETRY_DELAY_SECONDS)
        );
    }

    #[test]
    fn selects_alternate_snapshot_candidate_after_skipping_known_and_oversized_captures() {
        let current = CdxRecord {
            timestamp: "20200101000000".to_owned(),
            original: "http://www.example.com:80/downloads/game.exe".to_owned(),
            mimetype: "application/octet-stream".to_owned(),
            status_code: 200,
            digest: "same-digest".to_owned(),
            length: Some(100),
        };
        let mut seen_records = HashSet::new();
        remember_snapshot_record(&mut seen_records, &current);

        let records = vec![
            current.clone(),
            CdxRecord {
                timestamp: "20200202000000".to_owned(),
                original: "http://example.com/downloads/game.exe".to_owned(),
                mimetype: "application/octet-stream".to_owned(),
                status_code: 200,
                digest: "same-digest".to_owned(),
                length: Some(2_000),
            },
            CdxRecord {
                timestamp: "20200303000000".to_owned(),
                original: "http://example.com/downloads/game.exe".to_owned(),
                mimetype: "application/octet-stream".to_owned(),
                status_code: 200,
                digest: "same-digest".to_owned(),
                length: Some(900),
            },
        ];

        let selected = alternate_snapshot_candidate_from_records(
            records,
            &mut seen_records,
            Some(1_000),
            &CancellationFlag::new(),
        )
        .unwrap();

        assert_eq!(selected.timestamp, "20200303000000");
        assert_eq!(selected.digest, "same-digest");
    }

    #[test]
    fn accepts_extra_downloads_with_unknown_cdx_length() {
        let record = CdxRecord {
            timestamp: "20200101000000".to_owned(),
            original: "http://downloads.example.com/game.exe".to_owned(),
            mimetype: "application/octet-stream".to_owned(),
            status_code: 200,
            digest: "digest".to_owned(),
            length: None,
        };

        assert!(extra_download_record_fits_limit(&record, u64::MAX));
    }

    #[test]
    fn extracts_site_identity_terms_from_url() {
        assert_eq!(
            site_identity_terms("https://www.smallrockets.com/"),
            vec!["smallrockets".to_owned(), "smallrockets.com".to_owned()]
        );
    }

    #[test]
    fn checks_site_identity_case_insensitively() {
        let terms = site_identity_terms("https://smallrockets.com/");

        assert!(mentions_site_identity(
            "Small Rockets - Creators of downloadable games",
            &terms
        ));
        assert!(!mentions_site_identity(
            "The Blue Ribbon Foundation - Male Health and Wellbeing Charity",
            &terms
        ));
    }

    #[test]
    fn fallback_targets_include_canonical_url_without_volatile_query() {
        assert_eq!(
            fallback_capture_targets("http://example.com/forums/viewforum.php?f=21&sid=abcdef"),
            vec![
                "http://example.com/forums/viewforum.php?f=21&sid=abcdef".to_owned(),
                "http://example.com/forums/viewforum.php?f=21".to_owned(),
            ]
        );
    }

    #[test]
    fn classifies_recoverable_static_assets() {
        assert!(is_recoverable_static_asset(Path::new("images/screen5.jpg")));
        assert!(is_recoverable_static_asset(Path::new("css/site.css")));
        assert!(!is_recoverable_static_asset(Path::new(
            "forums/viewtopic.php"
        )));
        assert!(!is_recoverable_static_asset(Path::new("index.html")));
    }

    #[test]
    fn collects_missing_static_asset_targets_for_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(
            root.join("index.html"),
            r#"<img src="images/missing.gif"><a href="missing.html">Missing page</a>"#,
        )
        .unwrap();

        let targets = missing_static_asset_targets(root).unwrap();

        assert_eq!(targets, vec![PathBuf::from("images/missing.gif")]);
    }

    #[test]
    fn indexes_recoverable_static_records_by_local_path() {
        let mapper = SiteMapper::new("example.com").unwrap();
        let records = vec![
            CdxRecord {
                timestamp: "20200102030405".to_owned(),
                original: "http://www.example.com:80/images/logo.gif".to_owned(),
                mimetype: "image/gif".to_owned(),
                status_code: 200,
                digest: "digest".to_owned(),
                length: Some(512),
            },
            CdxRecord {
                timestamp: "20200102030405".to_owned(),
                original: "http://www.example.com/about/".to_owned(),
                mimetype: "text/html".to_owned(),
                status_code: 200,
                digest: "digest".to_owned(),
                length: Some(512),
            },
        ];

        let records_by_path = recoverable_static_records_by_path(
            &mapper,
            &records,
            SnapshotStrategy::Latest,
            1024,
        )
        .unwrap();

        assert_eq!(
            records_by_path
                .get(Path::new("images/logo.gif"))
                .map(|record| record.original.as_str()),
            Some("http://www.example.com:80/images/logo.gif")
        );
        assert!(!records_by_path.contains_key(Path::new("about/index.html")));
    }

    #[test]
    fn builds_conservative_static_asset_alias_candidates() {
        assert_eq!(
            static_asset_alias_candidates(Path::new("pc/poker/images/screen4.jpg")),
            vec![PathBuf::from("pc/poker/images/screenshot4.jpg")]
        );
        assert_eq!(
            static_asset_alias_candidates(Path::new("pc/poker/images/screenshot4.jpg")),
            vec![PathBuf::from("pc/poker/images/screen4.jpg")]
        );
        assert!(static_asset_alias_candidates(Path::new("images/screenplay.jpg")).is_empty());
    }

    #[test]
    fn detects_static_asset_alias_reference_in_alternate_capture() {
        let source = Path::new("pc/poker/screenshots__q_bd691b51ee9522b5.htm");
        let candidate = Path::new("pc/poker/images/screenshot4.jpg");

        assert!(candidate_reference_appears(
            r#"<img src="images/screenshot4.jpg">"#,
            source,
            candidate
        ));
        assert!(candidate_reference_appears(
            r#"<img src="/pc/poker/images/screenshot4.jpg">"#,
            source,
            candidate
        ));
        assert!(!candidate_reference_appears(
            r#"<img src="images/other.jpg">"#,
            source,
            candidate
        ));
    }

    #[tokio::test]
    async fn creates_missing_static_asset_alias_from_clear_sibling() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir_all(root.join("pc/poker/images")).unwrap();
        std::fs::write(root.join("pc/poker/images/screenshot4.jpg"), b"jpg").unwrap();
        std::fs::write(
            root.join("pc/poker/page.html"),
            r#"<img src="images/screen4.jpg">"#,
        )
        .unwrap();
        std::fs::write(
            root.join("pc/poker/screenshot4.htm"),
            r#"<img src="images/screenshot4.jpg">"#,
        )
        .unwrap();

        let created = create_missing_static_asset_aliases(root, &CancellationFlag::new())
            .await
            .unwrap();

        assert_eq!(created, 1);
        assert_eq!(
            std::fs::read(root.join("pc/poker/images/screen4.jpg")).unwrap(),
            b"jpg"
        );
    }

    #[tokio::test]
    async fn leaves_static_asset_alias_uncreated_without_local_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir_all(root.join("pc/poker/images")).unwrap();
        std::fs::write(root.join("pc/poker/images/screenshot4.jpg"), b"jpg").unwrap();
        std::fs::write(
            root.join("pc/poker/page.html"),
            r#"<img src="images/screen4.jpg">"#,
        )
        .unwrap();

        let created = create_missing_static_asset_aliases(root, &CancellationFlag::new())
            .await
            .unwrap();

        assert_eq!(created, 0);
        assert!(!root.join("pc/poker/images/screen4.jpg").exists());
    }

    /// Multiple URL variants (such as bare vs `www` host or different session IDs)
    /// that map to the same local path must select the capture matching the requested
    /// snapshot strategy (`Latest` or `Earliest`) rather than alphabetical URL order.
    #[test]
    fn selects_strategy_winning_record_per_local_path_instead_of_alphabetical_url() {
        let mapper = SiteMapper::new("example.com").unwrap();
        let records = vec![
            CdxRecord {
                timestamp: "20070101000000".to_owned(),
                original: "http://example.com/page.htm".to_owned(),
                mimetype: "text/html".to_owned(),
                status_code: 200,
                digest: "OLD_HOST".to_owned(),
                length: Some(500),
            },
            CdxRecord {
                timestamp: "20120601000000".to_owned(),
                original: "http://www.example.com/page.htm".to_owned(),
                mimetype: "text/html".to_owned(),
                status_code: 200,
                digest: "NEW_HOST".to_owned(),
                length: Some(600),
            },
            CdxRecord {
                timestamp: "20050309024319".to_owned(),
                original: "http://www.example.com/forums/viewforum.php?f=21&sid=000a".to_owned(),
                mimetype: "application/octet-stream".to_owned(),
                status_code: 200,
                digest: "OLD_SID".to_owned(),
                length: Some(700),
            },
            CdxRecord {
                timestamp: "20101125010613".to_owned(),
                original: "http://www.example.com/forums/viewforum.php?f=21&sid=ffff".to_owned(),
                mimetype: "text/html".to_owned(),
                status_code: 200,
                digest: "NEW_SID".to_owned(),
                length: Some(800),
            },
        ];

        let (known_latest, candidates_latest, selected_latest) =
            index_records_by_local_path(&mapper, &records, SnapshotStrategy::Latest).unwrap();
        assert_eq!(
            selected_latest
                .get(Path::new("page.htm"))
                .map(|record| record.timestamp.as_str()),
            Some("20120601000000")
        );
        let forum_path = known_latest
            .get("http://www.example.com/forums/viewforum.php?f=21")
            .unwrap();
        assert!(forum_path.to_string_lossy().ends_with(".html"));
        assert_eq!(
            selected_latest
                .get(forum_path)
                .map(|record| record.timestamp.as_str()),
            Some("20101125010613")
        );
        assert_eq!(candidates_latest.get(Path::new("page.htm")).unwrap().len(), 2);

        let (_, _, selected_earliest) =
            index_records_by_local_path(&mapper, &records, SnapshotStrategy::Earliest).unwrap();
        assert_eq!(
            selected_earliest
                .get(Path::new("page.htm"))
                .map(|record| record.timestamp.as_str()),
            Some("20070101000000")
        );
    }

    /// When the newest capture for a forum path is an unusable error page and its exact
    /// URL (or stripped URL) has no other captures in Wayback, fallback search must also
    /// try earlier discovered captures that mapped to the same local path with a different `sid`.
    #[tokio::test]
    async fn unusable_capture_falls_back_to_discovered_session_variant_for_same_path() {
        use wiremock::matchers::{path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let newer_unusable_url =
            "http://www.smallrockets.com/forums/viewforum.php?f=21&sid=7afce7097ef2dc36c6e5da997c132d21";
        let older_usable_url =
            "http://www.smallrockets.com:80/forums/viewforum.php?f=21&sid=89fabdc30059268a44ec36a1f3fdde78";

        Mock::given(path("/cdx/search/cdx"))
            .and(query_param("url", newer_unusable_url))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                "20061125010613 {newer_unusable_url} text/html 200 DEADFORUM 400\n"
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path(
            "/web/20050309024319id_/http://www.smallrockets.com:80/forums/viewforum.php",
        ))
        .and(query_param("f", "21"))
        .and(query_param("sid", "89fabdc30059268a44ec36a1f3fdde78"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<html><head><title>Small Rockets Forums :: View Forum - Wakeboarding Unleashed</title></head><body>Wakeboarding Unleashed Featuring Shaun Murray</body></html>",
        ))
        .expect(1)
        .mount(&server)
        .await;

        let mapper = SiteMapper::new("smallrockets.com").unwrap();
        let current = CdxRecord {
            timestamp: "20061125010613".to_owned(),
            original: newer_unusable_url.to_owned(),
            mimetype: "text/html".to_owned(),
            status_code: 200,
            digest: "DEADFORUM".to_owned(),
            length: Some(400),
        };
        let older = CdxRecord {
            timestamp: "20050309024319".to_owned(),
            original: older_usable_url.to_owned(),
            mimetype: "text/html".to_owned(),
            status_code: 200,
            digest: "GOODFORUM".to_owned(),
            length: Some(500),
        };
        let local_path = mapper
            .local_path_for_url(&current.original, &current.mimetype)
            .unwrap();
        let (_, candidates_by_path, selected_records_by_path) = index_records_by_local_path(
            &mapper,
            &[older.clone(), current.clone()],
            SnapshotStrategy::Latest,
        )
        .unwrap();

        let client = build_client("fallback-test", Duration::from_secs(5), Vec::new()).unwrap();
        let fallback = FallbackOptions {
            from: None,
            to: None,
            strategy: SnapshotStrategy::Latest,
            recovery: Arc::new(RecoveryState::default()),
            originals_by_path: selected_records_by_path,
            candidates_by_path,
        };

        let (recovered_record, recovered_body) = find_usable_capture(
            &client,
            &Url::parse(&server.uri()).unwrap(),
            &fallback,
            &CancellationFlag::new(),
            &current,
            &local_path,
        )
        .await
        .unwrap()
        .expect("should recover older session variant");

        assert_eq!(recovered_record.timestamp, "20050309024319");
        assert_eq!(recovered_record.original, older_usable_url);
        assert!(
            String::from_utf8_lossy(&recovered_body.bytes)
                .contains("Wakeboarding Unleashed Featuring Shaun Murray")
        );
    }

    /// phpBB 2 `images/smiles/` references, custom phpBB `templates/<theme>/images/` icons,
    /// and subdomain `_hosts/<host>/<subpath>` shared static assets must alias from existing
    /// unambiguous local counterparts (`images/smilies/`, `templates/subSilver/images/`, and
    /// primary/sibling host `<subpath>`).
    #[tokio::test]
    async fn creates_static_asset_aliases_for_phpbb_smilies_templates_and_shared_hosts() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();

        std::fs::create_dir_all(root.join("forums/images/smilies")).unwrap();
        std::fs::write(root.join("forums/images/smilies/icon_smile.gif"), b"smile").unwrap();

        std::fs::create_dir_all(root.join("forums/templates/subSilver/images")).unwrap();
        std::fs::write(
            root.join("forums/templates/subSilver/images/icon_latest_reply.gif"),
            b"reply",
        )
        .unwrap();

        std::fs::create_dir_all(root.join("bimages")).unwrap();
        std::fs::write(root.join("bimages/ic_logo.gif"), b"logo").unwrap();

        std::fs::create_dir_all(root.join("_hosts/pc.example.com/shared")).unwrap();
        std::fs::write(
            root.join("_hosts/pc.example.com/shared/bt_home_off.gif"),
            b"home",
        )
        .unwrap();

        std::fs::create_dir_all(root.join("_hosts/corporate.example.com/forums")).unwrap();
        std::fs::write(
            root.join("forums/viewtopic.html"),
            r#"<img src="images/smiles/icon_smile.gif"><img src="templates/SmallRockets/images/icon_latest_reply.gif">"#,
        )
        .unwrap();
        std::fs::write(
            root.join("_hosts/corporate.example.com/index.html"),
            r#"<img src="bimages/ic_logo.gif"><img src="shared/bt_home_off.gif">"#,
        )
        .unwrap();

        let created = create_missing_static_asset_aliases(root, &CancellationFlag::new())
            .await
            .unwrap();

        assert_eq!(created, 4);
        assert_eq!(
            std::fs::read(root.join("forums/images/smiles/icon_smile.gif")).unwrap(),
            b"smile"
        );
        assert_eq!(
            std::fs::read(root.join("forums/templates/SmallRockets/images/icon_latest_reply.gif"))
                .unwrap(),
            b"reply"
        );
        assert_eq!(
            std::fs::read(root.join("_hosts/corporate.example.com/bimages/ic_logo.gif")).unwrap(),
            b"logo"
        );
        assert_eq!(
            std::fs::read(root.join("_hosts/corporate.example.com/shared/bt_home_off.gif"))
                .unwrap(),
            b"home"
        );

        let log = std::fs::read_to_string(root.join(".wayback-state/aliases.jsonl")).unwrap();
        let events: Vec<AliasEvent> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 4);
        assert!(events.iter().any(|event| {
            event.target_path == Path::new("forums/images/smiles/icon_smile.gif")
                && event.source_path == Path::new("forums/images/smilies/icon_smile.gif")
                && event.reason == "static_asset_smiley_dir"
        }));
        assert!(events.iter().any(|event| {
            event.target_path == Path::new("forums/templates/SmallRockets/images/icon_latest_reply.gif")
                && event.source_path == Path::new("forums/templates/subSilver/images/icon_latest_reply.gif")
                && event.reason == "static_asset_theme_icon"
        }));
        assert!(events.iter().any(|event| {
            event.target_path == Path::new("_hosts/corporate.example.com/bimages/ic_logo.gif")
                && event.source_path == Path::new("bimages/ic_logo.gif")
                && event.reason == "static_asset_cross_host"
        }));
    }

    /// Extra download CDX lookups must strip the referring page's volatile session ID
    /// (`sid=...`) before querying Wayback so session-suffixed links do not issue two
    /// redundant 5-second-spaced CDX queries, while preserving non-sorted multi-key order.
    #[test]
    fn extra_download_capture_targets_strip_volatile_session_params_without_duplicate_lookup() {
        assert_eq!(
            extra_download_capture_targets(
                "http://www.smallrockets.com/forums/viewtopic.php?p=22745&sid=901279ef577c8c5e3a42029699bbda020"
            ),
            vec!["http://www.smallrockets.com/forums/viewtopic.php?p=22745".to_owned()]
        );
        assert_eq!(
            extra_download_capture_targets(
                "http://example.com/forums/viewtopic.php?t=7&p=9&sid=abcdef"
            ),
            vec![
                "http://example.com/forums/viewtopic.php?t=7&p=9".to_owned(),
                "http://example.com/forums/viewtopic.php?p=9&t=7".to_owned(),
            ]
        );
    }

    /// Primary host bare/`www` and `http`/`https` variants map to the same Wayback SURT
    /// key and must deduplicate in extra-download lookups, while non-primary subdomains
    /// remain distinct.
    #[test]
    fn canonical_extra_reference_key_deduplicates_primary_host_variants() {
        let mapper = SiteMapper::new("smallrockets.com").unwrap();
        let bare = canonical_extra_reference_key(
            &mapper,
            "http://smallrockets.com/forums/viewtopic.php?p=18412&sid=aaa",
        );
        let www = canonical_extra_reference_key(
            &mapper,
            "https://www.smallrockets.com/forums/viewtopic.php?p=18412&sid=bbb",
        );
        let sub = canonical_extra_reference_key(
            &mapper,
            "http://highscores.smallrockets.com/forums/viewtopic.php?p=18412",
        );

        assert_eq!(bare, www);
        assert_ne!(bare, sub);
    }

    /// Exact Wayback recovery must run before local aliasing so a different topic sharing
    /// the same title is fetched authentically instead of being replaced by an inferred alias.
    #[tokio::test]
    async fn download_site_fetches_exact_shared_title_topic_before_aliasing() {
        use wiremock::matchers::{path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let output = tempfile::tempdir().unwrap();
        let timestamp = "20060101000000";
        let index_url = "http://www.example.com/forums/index.htm";
        let topic1_url = "http://www.example.com/forums/viewtopic.php?t=1";
        let topic2_url = "http://www.example.com/forums/viewtopic.php?t=2";
        let mapper = SiteMapper::new("example.com").unwrap();

        Mock::given(path("/cdx/search/cdx"))
            .and(query_param("matchType", "domain"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                "{timestamp} {index_url} text/html 200 IDX 100\n{timestamp} {topic1_url} text/html 200 TOP1 100\n"
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/cdx/search/cdx"))
            .and(query_param("matchType", "exact"))
            .and(query_param("url", topic2_url))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                "{timestamp} {topic2_url} text/html 200 TOP2 100\n"
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path(format!("/web/{timestamp}id_/{index_url}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<a href="viewtopic.php?t=1">Welcome</a><a href="viewtopic.php?t=2">Welcome</a>"#,
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path(format!(
            "/web/{timestamp}id_/http://www.example.com/forums/viewtopic.php"
        )))
        .and(query_param("t", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<title>Forum :: View topic - Welcome</title><p>Authentic topic 1 text</p>"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
        Mock::given(path(format!(
            "/web/{timestamp}id_/http://www.example.com/forums/viewtopic.php"
        )))
        .and(query_param("t", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<title>Forum :: View topic - Welcome</title><p>Authentic topic 2 text</p>"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

        let report = download_site(
            build_client("exact-before-alias-test", Duration::from_secs(5), Vec::new()).unwrap(),
            mapper.clone(),
            CdxQuery::new(
                "example.com".to_owned(),
                MatchType::Domain,
                SnapshotStrategy::Latest,
                Url::parse(&server.uri()).unwrap(),
            ),
            DownloadOptions {
                output_dir: output.path().to_owned(),
                no_clobber: true,
                recover_existing: false,
                rewrite_links: true,
                add_wayback_comment: true,
                extra_download_max_bytes: Some(u64::MAX),
                validate_links: true,
                cancellation: CancellationFlag::new(),
            },
        )
        .await
        .unwrap();

        assert_eq!(report.downloaded, 3);
        assert_eq!(report.extra_downloads, 1);
        assert_eq!(report.aliases_created, 0);
        assert_eq!(report.missing_local_links, 0);

        let topic1_path = mapper.local_path_for_url(topic1_url, "text/html").unwrap();
        let topic2_path = mapper.local_path_for_url(topic2_url, "text/html").unwrap();
        let saved1 = std::fs::read_to_string(output.path().join(topic1_path)).unwrap();
        let saved2 = std::fs::read_to_string(output.path().join(topic2_path)).unwrap();
        assert!(saved1.contains("Authentic topic 1 text"), "{saved1}");
        assert!(saved2.contains("Authentic topic 2 text"), "{saved2}");
        assert!(!saved2.contains("Authentic topic 1 text"), "{saved2}");
    }

    /// Exact Wayback static-asset recovery must run before local cross-host aliasing so a
    /// subdomain's own archived asset is fetched instead of being replaced by the primary host's file.
    #[tokio::test]
    async fn repair_output_dir_fetches_exact_static_asset_before_cross_host_aliasing() {
        use wiremock::matchers::{path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let output = tempfile::tempdir().unwrap();
        let timestamp = "20060101000000";
        let sub_logo = "http://sub.example.com/images/logo.gif";

        std::fs::create_dir_all(output.path().join("images")).unwrap();
        std::fs::write(output.path().join("images/logo.gif"), b"primary logo").unwrap();
        std::fs::create_dir_all(output.path().join("_hosts/sub.example.com")).unwrap();
        std::fs::write(
            output.path().join("_hosts/sub.example.com/index.html"),
            r#"<img src="images/logo.gif">"#,
        )
        .unwrap();

        Mock::given(path("/cdx/search/cdx"))
            .and(query_param("matchType", "domain"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                "{timestamp} {sub_logo} image/gif 200 SUBLOGO 14\n"
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path(format!("/web/{timestamp}id_/{sub_logo}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"subdomain logo".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let report = repair_output_dir(
            build_client("static-exact-before-alias", Duration::from_secs(5), Vec::new()).unwrap(),
            SiteMapper::new("example.com").unwrap(),
            RepairOptions {
                output_dir: output.path().to_owned(),
                archive_root: Url::parse(&server.uri()).unwrap(),
                match_type: MatchType::Domain,
                from: None,
                to: None,
                limit: None,
                strategy: SnapshotStrategy::Latest,
                extra_download_max_bytes: Some(u64::MAX),
                validate_links: true,
                cancellation: CancellationFlag::new(),
            },
        )
        .await
        .unwrap();

        assert_eq!(report.recovered_static_assets, 1);
        assert_eq!(report.static_asset_aliases_created, 0);
        assert_eq!(report.missing_local_links, 0);
        assert_eq!(
            std::fs::read(output.path().join("_hosts/sub.example.com/images/logo.gif")).unwrap(),
            b"subdomain logo"
        );
    }

    #[test]
    fn wayback_replay_url_uses_browseable_snapshot_path() {
        let root = Url::parse("https://web.archive.org").unwrap();
        let record = CdxRecord {
            timestamp: "20120805042739".into(),
            original: "http://www.example.com:80/forums/viewtopic.php?t=42&f=3".into(),
            mimetype: "text/html".into(),
            status_code: 200,
            digest: "ABC".into(),
            length: None,
        };

        let replay = wayback_replay_url(&root, &record).unwrap();
        assert_eq!(
            replay.as_str(),
            "https://web.archive.org/web/20120805042739/http://www.example.com:80/forums/viewtopic.php?t=42&f=3"
        );
    }

    #[test]
    fn insert_wayback_source_comment_places_after_doctype_and_xml_prologue() {
        let url = "https://web.archive.org/web/20120805042739/http://example.com/";
        let comment = format!("<!-- Archived from Wayback Machine: {url} -->");

        assert_eq!(
            insert_wayback_source_comment("<html><body>Hi</body></html>", url),
            format!("{comment}\n<html><body>Hi</body></html>")
        );
        assert_eq!(
            insert_wayback_source_comment("<!DOCTYPE html>\n<html><body>Hi</body></html>", url),
            format!("<!DOCTYPE html>\n{comment}\n<html><body>Hi</body></html>")
        );
        assert_eq!(
            insert_wayback_source_comment("<!DOCTYPE html>\r\n<html><body>Hi</body></html>", url),
            format!("<!DOCTYPE html>\r\n{comment}\r\n<html><body>Hi</body></html>")
        );
        assert_eq!(
            insert_wayback_source_comment("<!DOCTYPE html><html><body>Hi</body></html>", url),
            format!("<!DOCTYPE html>\n{comment}\n<html><body>Hi</body></html>")
        );
        assert_eq!(
            insert_wayback_source_comment(
                "  \n<!doctype HTML PUBLIC \"-//W3C//DTD HTML 4.01 Transitional//EN\">\n<html>",
                url
            ),
            format!(
                "  \n<!doctype HTML PUBLIC \"-//W3C//DTD HTML 4.01 Transitional//EN\">\n{comment}\n<html>"
            )
        );
        assert_eq!(
            insert_wayback_source_comment(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0 Strict//EN\" \"http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd\">\n<html>",
                url
            ),
            format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0 Strict//EN\" \"http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd\">\n{comment}\n<html>"
            )
        );
        assert_eq!(
            insert_wayback_source_comment(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<html>",
                url
            ),
            format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n{comment}\n<html>")
        );
        assert_eq!(
            insert_wayback_source_comment(
                "<!DOCTYPE html [ <!ENTITY foo \"bar\"> ]>\n<html>",
                url
            ),
            format!("<!DOCTYPE html [ <!ENTITY foo \"bar\"> ]>\n{comment}\n<html>")
        );
        assert_eq!(
            insert_wayback_source_comment("<p>\u{65e5}\u{672c} \u{1f680}</p>", url),
            format!("{comment}\n<p>\u{65e5}\u{672c} \u{1f680}</p>")
        );
        assert_eq!(
            insert_wayback_source_comment("\u{feff}<p>Hi</p>", url),
            format!("\u{feff}{comment}\n<p>Hi</p>")
        );
        assert_eq!(
            insert_wayback_source_comment("<!DOCTYPE html", url),
            format!("{comment}\n<!DOCTYPE html")
        );

        let once = insert_wayback_source_comment("<!DOCTYPE html>\n<html></html>", url);
        assert_eq!(insert_wayback_source_comment(&once, url), once);
    }

    #[test]
    fn insert_wayback_source_comment_sanitizes_comment_breaking_sequences() {
        let html = insert_wayback_source_comment(
            "<p>Page</p>",
            "https://web.archive.org/web/20120805042739/http://example.com/a--b---c<d>e\r\nf",
        );
        assert_eq!(
            html,
            "<!-- Archived from Wayback Machine: https://web.archive.org/web/20120805042739/http://example.com/a%2D%2Db%2D%2D-c%3Cd%3Ee%0D%0Af -->\n<p>Page</p>"
        );
    }

    /// Linked HTML pages archived only as 301/302 redirects should resolve their related-host
    /// 200 OK target, emit a static HTML redirect stub with provenance, and pass validation.
    #[tokio::test]
    async fn download_site_follows_archived_301_redirect_for_linked_html_page() {
        use wiremock::matchers::{path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let output = tempfile::tempdir().unwrap();
        let timestamp = "20060101000000";
        let index_url = "http://example.com/index.html";
        let old_url = "http://example.com/old-game.html";
        let new_url = "http://example.com/games/new-game.html";

        Mock::given(path("/cdx/search/cdx"))
            .and(query_param("matchType", "domain"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                "{timestamp} {index_url} text/html 200 IDX 100\n"
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/cdx/search/cdx"))
            .and(query_param("matchType", "exact"))
            .and(query_param("url", old_url))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                "{timestamp} {old_url} text/html 301 REDIR 0\n"
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/cdx/search/cdx"))
            .and(query_param("matchType", "exact"))
            .and(query_param("url", new_url))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                "{timestamp} {new_url} text/html 200 NEW 100\n"
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path(format!("/web/{timestamp}id_/{index_url}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"<a href="/old-game.html">Play</a>"#),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path(format!("/web/{timestamp}id_/{old_url}")))
            .respond_with(
                ResponseTemplate::new(301)
                    .insert_header("X-Archive-Orig-Location", "/games/new-game.html")
                    .insert_header(
                        "Location",
                        format!("{}/web/{timestamp}id_/{new_url}", server.uri()),
                    ),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path(format!("/web/{timestamp}id_/{new_url}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("<html><body><h1>New Game</h1></body></html>"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let report = download_site(
            build_client("redirect-html-test", Duration::from_secs(5), Vec::new()).unwrap(),
            SiteMapper::new("example.com").unwrap(),
            CdxQuery::new(
                "example.com".to_owned(),
                MatchType::Domain,
                SnapshotStrategy::Latest,
                Url::parse(&server.uri()).unwrap(),
            ),
            DownloadOptions {
                output_dir: output.path().to_owned(),
                no_clobber: true,
                recover_existing: false,
                rewrite_links: true,
                add_wayback_comment: true,
                extra_download_max_bytes: Some(u64::MAX),
                validate_links: true,
                cancellation: CancellationFlag::new(),
            },
        )
        .await
        .unwrap();

        assert_eq!(report.downloaded, 2);
        assert_eq!(report.extra_downloads, 1);
        assert_eq!(report.aliases_created, 1);
        assert_eq!(report.missing_local_links, 0);
        assert_eq!(report.unusable_html_files, 0);

        let saved_new =
            std::fs::read_to_string(output.path().join("games/new-game.html")).unwrap();
        assert!(saved_new.contains("<h1>New Game</h1>"), "{saved_new}");
        let saved_stub = std::fs::read_to_string(output.path().join("old-game.html")).unwrap();
        assert!(saved_stub.contains("data-webarchive-redirect=\"301\""), "{saved_stub}");
        assert!(
            saved_stub.contains("content=\"0; url=games/new-game.html\""),
            "{saved_stub}"
        );
        assert!(
            saved_stub.contains(&format!(
                "<!-- Archived HTTP 301 redirect from Wayback Machine: {}/web/{timestamp}/{old_url} -->",
                server.uri()
            )),
            "{saved_stub}"
        );

        let aliases_log =
            std::fs::read_to_string(output.path().join(".wayback-state/aliases.jsonl")).unwrap();
        let events: Vec<AliasEvent> = aliases_log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            events,
            vec![AliasEvent {
                target_path: PathBuf::from("old-game.html"),
                source_path: PathBuf::from("games/new-game.html"),
                reason: "archived_http_301_redirect".to_owned(),
                replaced_unusable: false,
                referred_from: None,
            }]
        );
    }

    /// A 200 OK capture must always win over a newer 301/302 redirect (such as a post-shutdown
    /// domain redirect), and cross-domain 3xx redirects must never be followed.
    #[tokio::test]
    async fn download_site_prefers_200_over_later_301_and_skips_external_301_and_resolves_binary_302() {
        use wiremock::matchers::{path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let output = tempfile::tempdir().unwrap();
        let index_url = "http://example.com/index.html";
        let page_url = "http://example.com/page.html";
        let external_url = "http://example.com/parked.html";
        let old_img = "http://example.com/images/old.gif";
        let new_img = "http://example.com/images/new.gif";

        Mock::given(path("/cdx/search/cdx"))
            .and(query_param("matchType", "domain"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                "20060101000000 {index_url} text/html 200 IDX 100\n"
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/cdx/search/cdx"))
            .and(query_param("matchType", "exact"))
            .and(query_param("url", page_url))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                "20130101000000 {page_url} text/html 301 LATER_REDIRECT 0\n20060101000000 {page_url} text/html 200 AUTHENTIC_200 100\n"
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/cdx/search/cdx"))
            .and(query_param("matchType", "exact"))
            .and(query_param("url", external_url))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                "20130101000000 {external_url} text/html 301 EXT_REDIR 0\n"
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/cdx/search/cdx"))
            .and(query_param("matchType", "exact"))
            .and(query_param("url", old_img))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                "20060101000000 {old_img} image/gif 302 IMG_REDIR 0\n"
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/cdx/search/cdx"))
            .and(query_param("matchType", "exact"))
            .and(query_param("url", new_img))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                "20060101000000 {new_img} image/gif 200 NEW_GIF 6\n"
            )))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(path(format!("/web/20060101000000id_/{index_url}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<a href="/page.html">Page</a><a href="/parked.html">Parked</a><img src="/images/old.gif">"#,
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path(format!("/web/20060101000000id_/{page_url}")))
            .respond_with(ResponseTemplate::new(200).set_body_string("<p>Authentic 200</p>"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path(format!("/web/20130101000000id_/{page_url}")))
            .respond_with(
                ResponseTemplate::new(301)
                    .insert_header("X-Archive-Orig-Location", "http://unrelated.invalid/"),
            )
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(path(format!("/web/20130101000000id_/{external_url}")))
            .respond_with(
                ResponseTemplate::new(301)
                    .insert_header("X-Archive-Orig-Location", "https://unrelated.invalid/parked"),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path(format!("/web/20060101000000id_/{old_img}")))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("X-Archive-Orig-Location", "/images/new.gif"),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path(format!("/web/20060101000000id_/{new_img}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"GIF89a".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let report = download_site(
            build_client("redirect-rules-test", Duration::from_secs(5), Vec::new()).unwrap(),
            SiteMapper::new("example.com").unwrap(),
            CdxQuery::new(
                "example.com".to_owned(),
                MatchType::Domain,
                SnapshotStrategy::Latest,
                Url::parse(&server.uri()).unwrap(),
            ),
            DownloadOptions {
                output_dir: output.path().to_owned(),
                no_clobber: true,
                recover_existing: false,
                rewrite_links: true,
                add_wayback_comment: true,
                extra_download_max_bytes: Some(u64::MAX),
                validate_links: true,
                cancellation: CancellationFlag::new(),
            },
        )
        .await
        .unwrap();

        assert_eq!(report.downloaded, 3);
        assert_eq!(report.extra_downloads, 2);
        assert_eq!(report.aliases_created, 1);
        assert_eq!(report.linked_files_unavailable, 1);
        assert_eq!(report.missing_local_links, 1);
        assert!(
            std::fs::read_to_string(output.path().join("page.html"))
                .unwrap()
                .contains("Authentic 200")
        );
        assert!(!output.path().join("parked.html").exists());
        assert_eq!(
            std::fs::read(output.path().join("images/new.gif")).unwrap(),
            b"GIF89a"
        );
        assert_eq!(
            std::fs::read(output.path().join("images/old.gif")).unwrap(),
            b"GIF89a"
        );
    }

    /// Distinct pages parameterized by generic keys such as `st=0` vs `st=20` must download
    /// into separate local files and preserve both pages' authentic content.
    #[tokio::test]
    async fn download_site_preserves_distinct_st_paginated_pages() {
        use wiremock::matchers::{path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let output = tempfile::tempdir().unwrap();
        let timestamp = "20060101000000";
        let mapper = SiteMapper::new("example.com").unwrap();
        let index_url = "http://example.com/index.html";
        let page0_url = "http://example.com/list.php?st=0";
        let page20_url = "http://example.com/list.php?st=20";

        Mock::given(path("/cdx/search/cdx"))
            .and(query_param("matchType", "domain"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                "{timestamp} {index_url} text/html 200 IDX 100\n{timestamp} {page0_url} text/html 200 P0 100\n{timestamp} {page20_url} text/html 200 P20 100\n"
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path(format!("/web/{timestamp}id_/{index_url}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<a href="list.php?st=0">Page 1</a><a href="list.php?st=20">Page 2</a>"#,
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path(format!("/web/{timestamp}id_/http://example.com/list.php")))
            .and(query_param("st", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<p>Items 1-20</p>"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path(format!("/web/{timestamp}id_/http://example.com/list.php")))
            .and(query_param("st", "20"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<p>Items 21-40</p>"))
            .expect(1)
            .mount(&server)
            .await;

        let report = download_site(
            build_client("st-pagination-test", Duration::from_secs(5), Vec::new()).unwrap(),
            mapper.clone(),
            CdxQuery::new(
                "example.com".to_owned(),
                MatchType::Domain,
                SnapshotStrategy::Latest,
                Url::parse(&server.uri()).unwrap(),
            ),
            DownloadOptions {
                output_dir: output.path().to_owned(),
                no_clobber: true,
                recover_existing: false,
                rewrite_links: true,
                add_wayback_comment: true,
                extra_download_max_bytes: Some(u64::MAX),
                validate_links: true,
                cancellation: CancellationFlag::new(),
            },
        )
        .await
        .unwrap();

        assert_eq!(report.downloaded, 3);
        assert_eq!(report.missing_local_links, 0);
        let path0 = mapper.local_path_for_url(page0_url, "text/html").unwrap();
        let path20 = mapper.local_path_for_url(page20_url, "text/html").unwrap();
        assert_ne!(path0, path20);
        assert!(
            std::fs::read_to_string(output.path().join(path0))
                .unwrap()
                .contains("Items 1-20")
        );
        assert!(
            std::fs::read_to_string(output.path().join(path20))
                .unwrap()
                .contains("Items 21-40")
        );
    }

    /// URLs containing `%20` must write decoded filenames with spaces on disk and emit
    /// `%20` relative URLs in rewritten HTML so HTTP servers and link validation resolve them.
    #[tokio::test]
    async fn download_site_decodes_percent_20_filenames_and_validates_cleanly() {
        use wiremock::matchers::{path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let output = tempfile::tempdir().unwrap();
        let timestamp = "20060101000000";
        let index_url = "http://example.com/index.html";
        let image_url = "http://example.com/my%20images/my%20photo.gif";

        Mock::given(path("/cdx/search/cdx"))
            .and(query_param("matchType", "domain"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                "{timestamp} {index_url} text/html 200 IDX 100\n{timestamp} {image_url} image/gif 200 IMG 6\n"
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path(format!("/web/{timestamp}id_/{index_url}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"<img src="/my%20images/my%20photo.gif">"#),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path(format!("/web/{timestamp}id_/{image_url}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"GIF89a".to_vec()))
            .expect(1)
            .mount(&server)
            .await;

        let report = download_site(
            build_client("percent20-test", Duration::from_secs(5), Vec::new()).unwrap(),
            SiteMapper::new("example.com").unwrap(),
            CdxQuery::new(
                "example.com".to_owned(),
                MatchType::Domain,
                SnapshotStrategy::Latest,
                Url::parse(&server.uri()).unwrap(),
            ),
            DownloadOptions {
                output_dir: output.path().to_owned(),
                no_clobber: true,
                recover_existing: false,
                rewrite_links: true,
                add_wayback_comment: true,
                extra_download_max_bytes: Some(u64::MAX),
                validate_links: true,
                cancellation: CancellationFlag::new(),
            },
        )
        .await
        .unwrap();

        assert_eq!(report.downloaded, 2);
        assert_eq!(report.missing_local_links, 0);
        assert!(output.path().join("my images/my photo.gif").is_file());
        assert!(!output.path().join("my%20images/my%20photo.gif").exists());
        let saved_index = std::fs::read_to_string(output.path().join("index.html")).unwrap();
        assert!(
            saved_index.contains(r#"src="my%20images/my%20photo.gif""#),
            "{saved_index}"
        );
    }
}
