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

use crate::alias_repair::create_missing_topic_aliases;
use crate::cdx::{
    CdxQuery, CdxRecord, CdxRetryPolicy, MAX_WAYBACK_RETRY_DELAY_SECONDS, MatchType,
    SnapshotStrategy, fetch_all_records, fetch_all_records_with_policy, fetch_latest_records,
    is_cdx_connectivity_error,
};
use crate::download_refs::{extract_downloadable_references, remove_missing_download_links};
use crate::link_validation::{
    remove_missing_local_href_links, remove_missing_local_resource_references, validate_local_links,
};
use crate::noise::is_archive_noise_record;
use crate::pathmap::{
    SiteMapper, is_css_mimetype, is_html_mimetype, normalize_lookup_url, relative_link,
};
use crate::rewrite::{RewriteContext, rewrite_css, rewrite_html};
use crate::soft_redirect::is_unusable_html_capture;
use crate::wayback_client::WaybackClient;

#[derive(Clone, Debug)]
pub struct DownloadOptions {
    pub output_dir: PathBuf,
    pub no_clobber: bool,
    pub rewrite_links: bool,
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
    pub aliases_created: usize,
    pub extra_downloads: usize,
    pub linked_files_unavailable: usize,
    pub linked_files_deferred: usize,
    pub recovered_static_assets: usize,
    pub static_asset_aliases_created: usize,
    pub unavailable_static_assets: usize,
    pub deferred_static_assets: usize,
    pub download_links_removed: usize,
    pub local_hrefs_removed: usize,
    pub local_resources_removed: usize,
    pub local_links_checked: usize,
    pub missing_local_links: usize,
    pub missing_image_sources: usize,
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
    pub deferred_static_assets: usize,
    pub download_links_removed: usize,
    pub local_hrefs_removed: usize,
    pub local_resources_removed: usize,
    pub local_links_checked: usize,
    pub missing_local_links: usize,
    pub missing_image_sources: usize,
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
}

#[derive(Clone, Debug)]
struct FallbackOptions {
    from: Option<String>,
    to: Option<String>,
    strategy: SnapshotStrategy,
}

const MAX_ALTERNATE_PAGE_CAPTURE_CHECKS: usize = 20;
const MAX_EXTRA_CDX_CONNECTIVITY_FAILURES: usize = 3;
const MAX_STATIC_ASSET_CDX_CONNECTIVITY_FAILURES: usize = 3;
const MAX_ALTERNATE_CDX_CONNECTIVITY_FAILURES: usize = 3;
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
    let mut known_paths = mapper.records_to_paths(&records)?;
    let recovery_records_by_path = recoverable_static_records_by_path(
        &mapper,
        &records,
        options.extra_download_max_bytes.unwrap_or(u64::MAX),
    )?;
    let selected_records_by_path = records_by_local_path(&mapper, &records)?;

    fs::create_dir_all(&options.output_dir)
        .await
        .with_context(|| {
            format!(
                "failed to create output directory {}",
                options.output_dir.display()
            )
        })?;

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
            jobs.push(DownloadJob { record, local_path });
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
    };
    let archive_root = query.archive_root;

    let mut report = DownloadReport {
        discovered,
        skipped: duplicate_paths,
        output_dir: options.output_dir.clone(),
        ..DownloadReport::default()
    };
    let mut extra_download_refs = HashSet::new();

    for job in jobs {
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
            }) => {
                report.downloaded += 1;
                extra_download_refs.extend(refs);
            }
            Ok(DownloadStatus::Skipped) => report.skipped += 1,
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
    }

    if options.rewrite_links && report.cancelled == 0 {
        if let Some(max_bytes) = options.extra_download_max_bytes {
            let extra_resolution = resolve_extra_download_jobs(
                &client,
                &archive_root,
                &mapper,
                &fallback_options,
                &extra_download_refs,
                &mut known_paths,
                &mut seen_paths,
                max_bytes,
            )
            .await?;
            if !extra_resolution.jobs.is_empty() {
                println!(
                    "selected {} linked files under {} bytes",
                    extra_resolution.jobs.len(),
                    max_bytes
                );
            }
            if extra_resolution.unavailable > 0 {
                println!(
                    "linked files unavailable in Wayback or over size limit: {}",
                    extra_resolution.unavailable
                );
            }
            if extra_resolution.deferred > 0 {
                println!(
                    "linked files deferred because Wayback CDX was unreachable: {}",
                    extra_resolution.deferred
                );
            }
            report.linked_files_unavailable = extra_resolution.unavailable;
            report.linked_files_deferred = extra_resolution.deferred;

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
                    Ok(DownloadStatus::Downloaded { .. }) => {
                        report.downloaded += 1;
                        report.extra_downloads += 1;
                    }
                    Ok(DownloadStatus::Skipped) => report.skipped += 1,
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
        }

        let alias_report = create_missing_topic_aliases(&options.output_dir)?;
        if alias_report.created > 0 {
            println!(
                "created {} local alias files for missing topic links",
                alias_report.created
            );
        }
        report.aliases_created = alias_report.created;

        if let Some(max_bytes) = options.extra_download_max_bytes {
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
            report.deferred_static_assets = recovery_report.deferred;
        }

        let alternate_page_aliases = create_missing_static_asset_aliases_from_alternate_pages(
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
        if alternate_page_aliases > 0 {
            println!("created {alternate_page_aliases} local aliases from alternate page captures");
        }

        let local_static_asset_aliases =
            create_missing_static_asset_aliases(&options.output_dir, &options.cancellation).await?;
        if local_static_asset_aliases > 0 {
            println!(
                "created {local_static_asset_aliases} local aliases for missing static assets"
            );
        }
        report.static_asset_aliases_created = alternate_page_aliases + local_static_asset_aliases;
        report.unavailable_static_assets = report
            .unavailable_static_assets
            .saturating_sub(report.static_asset_aliases_created);
        if report.unavailable_static_assets > 0 {
            println!(
                "missing static assets unavailable in Wayback or over size limit: {}",
                report.unavailable_static_assets
            );
        }
        if report.deferred_static_assets > 0 {
            println!(
                "missing static assets deferred because Wayback CDX was unreachable: {}",
                report.deferred_static_assets
            );
        } else if options.extra_download_max_bytes.is_some() {
            let removed_resources = remove_missing_local_resource_references(&options.output_dir)?;
            if removed_resources > 0 {
                println!(
                    "removed {removed_resources} local resource references with no captured file"
                );
            }
            report.local_resources_removed = removed_resources;
        }

        if options.cancellation.is_cancelled() {
            report.cancelled += 1;
            return Ok(report);
        }

        let removed_download_links = remove_missing_download_links(&options.output_dir)?;
        if removed_download_links > 0 {
            println!("removed {removed_download_links} local download links with no captured file");
        }
        report.download_links_removed = removed_download_links;

        let removed_local_hrefs = remove_missing_local_href_links(&options.output_dir)?;
        if removed_local_hrefs > 0 {
            println!("removed {removed_local_hrefs} local hrefs with no captured file");
        }
        report.local_hrefs_removed = removed_local_hrefs;
    }

    if options.validate_links && report.cancelled == 0 {
        let validation_report = validate_local_links(&options.output_dir)?;
        report.local_links_checked = validation_report.checked;
        report.missing_local_links = validation_report.missing.len();
        report.missing_image_sources = validation_report.missing_image_sources.len();
        println!(
            "validated {} local links; missing {}; images without source {}",
            validation_report.checked,
            validation_report.missing.len(),
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

    let mut report = RepairReport {
        output_dir: options.output_dir.clone(),
        ..RepairReport::default()
    };

    let alias_report = create_missing_topic_aliases(&options.output_dir)?;
    if alias_report.created > 0 {
        println!(
            "created {} local alias files for missing topic links",
            alias_report.created
        );
    }
    report.aliases_created = alias_report.created;

    if let Some(max_bytes) = options.extra_download_max_bytes {
        let records = fetch_recovery_records(&client, &mapper, &options).await?;
        let selected_records_by_path = records_by_local_path(&mapper, &records)?;
        let recovery_records_by_path =
            recoverable_static_records_by_path(&mapper, &records, max_bytes)?;
        let fallback_options = FallbackOptions {
            from: options.from.clone(),
            to: options.to.clone(),
            strategy: options.strategy,
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
        report.deferred_static_assets = recovery_report.deferred;

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
            println!(
                "missing static assets unavailable in Wayback or over size limit: {}",
                report.unavailable_static_assets
            );
        }
        if report.deferred_static_assets > 0 {
            println!(
                "missing static assets deferred because Wayback CDX was unreachable: {}",
                report.deferred_static_assets
            );
        } else if options.extra_download_max_bytes.is_some() {
            let removed_resources = remove_missing_local_resource_references(&options.output_dir)?;
            if removed_resources > 0 {
                println!(
                    "removed {removed_resources} local resource references with no captured file"
                );
            }
            report.local_resources_removed = removed_resources;
        }

        let removed_download_links = remove_missing_download_links(&options.output_dir)?;
        if removed_download_links > 0 {
            println!("removed {removed_download_links} local download links with no captured file");
        }
        report.download_links_removed = removed_download_links;

        let removed_local_hrefs = remove_missing_local_href_links(&options.output_dir)?;
        if removed_local_hrefs > 0 {
            println!("removed {removed_local_hrefs} local hrefs with no captured file");
        }
        report.local_hrefs_removed = removed_local_hrefs;
    }

    if options.validate_links && !options.cancellation.is_cancelled() {
        let validation_report = validate_local_links(&options.output_dir)?;
        report.local_links_checked = validation_report.checked;
        report.missing_local_links = validation_report.missing.len();
        report.missing_image_sources = validation_report.missing_image_sources.len();
        println!(
            "validated {} local links; missing {}; images without source {}",
            validation_report.checked,
            validation_report.missing.len(),
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
    max_bytes: u64,
) -> Result<HashMap<PathBuf, CdxRecord>> {
    let mut records_by_path = HashMap::new();

    for record in records {
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

fn records_by_local_path(
    mapper: &SiteMapper,
    records: &[CdxRecord],
) -> Result<HashMap<PathBuf, CdxRecord>> {
    let mut records_by_path = HashMap::new();

    for record in records {
        let local_path = mapper.local_path_for_url(&record.original, &record.mimetype)?;
        records_by_path
            .entry(local_path)
            .or_insert_with(|| record.clone());
    }

    Ok(records_by_path)
}

fn filter_archive_noise(records: &mut Vec<CdxRecord>) -> usize {
    let before = records.len();
    records.retain(|record| !is_archive_noise_record(record));
    before - records.len()
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DownloadStatus {
    Downloaded { extra_download_refs: Vec<String> },
    Skipped,
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
    if options.cancellation.is_cancelled() {
        return Ok(DownloadStatus::Cancelled);
    }

    let destination = options.output_dir.join(&job.local_path);
    if options.no_clobber && has_non_empty_file(&destination).await? {
        return Ok(DownloadStatus::Skipped);
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }

    println!("{}", job.record.original);

    if should_buffer_response(&job.record, &job.local_path, options.rewrite_links) {
        let bytes =
            fetch_record_bytes(client, archive_root, &job.record, &options.cancellation).await?;
        let (record, bytes) = maybe_replace_unusable_capture(
            client,
            archive_root,
            fallback_options,
            &options.cancellation,
            &job.record,
            bytes,
            &job.local_path,
        )
        .await?;

        if options.cancellation.is_cancelled() {
            return Ok(DownloadStatus::Cancelled);
        }

        if options.rewrite_links && should_rewrite_as_text(&record, &job.local_path) {
            let text = String::from_utf8_lossy(&bytes);
            let extra_download_refs =
                extract_downloadable_references(&text, &Url::parse(&record.original)?, mapper);
            let context = RewriteContext::new_with_mapper(
                &record.original,
                job.local_path.clone(),
                known_paths,
                mapper,
            )?;
            let rewritten = if is_css_mimetype(&record.mimetype) {
                rewrite_css(&text, &context)
            } else {
                rewrite_html(&text, &context)?
            };
            write_bytes_atomic(&destination, rewritten.as_bytes()).await?;
            return Ok(DownloadStatus::Downloaded {
                extra_download_refs,
            });
        } else {
            write_bytes_atomic(&destination, &bytes).await?;
        }
    } else {
        download_record_to_file(
            client,
            archive_root,
            &job.record,
            &destination,
            &options.cancellation,
        )
        .await?;
    }

    Ok(DownloadStatus::Downloaded {
        extra_download_refs: Vec::new(),
    })
}

#[derive(Clone, Debug, Default)]
struct ExtraDownloadResolution {
    jobs: Vec<DownloadJob>,
    unavailable: usize,
    deferred: usize,
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
) -> Result<ExtraDownloadResolution> {
    let mut resolution = ExtraDownloadResolution::default();
    let mut references = references.iter().cloned().collect::<Vec<_>>();
    references.sort();
    let references_len = references.len();
    let mut consecutive_connectivity_failures = 0;

    for (index, reference) in references.into_iter().enumerate() {
        let lookup_key = normalize_lookup_url(&reference);
        if known_paths.contains_key(&lookup_key) {
            continue;
        }

        let lookup = find_extra_download_record(
            client,
            archive_root,
            fallback_options,
            &reference,
            max_bytes,
        )
        .await?;
        match lookup.record {
            Some(record) => {
                consecutive_connectivity_failures = 0;
                let local_path = mapper.local_path_for_url(&record.original, &record.mimetype)?;
                known_paths.insert(lookup_key, local_path.clone());
                known_paths.insert(normalize_lookup_url(&record.original), local_path.clone());
                if seen_paths.insert(local_path.clone()) {
                    resolution.jobs.push(DownloadJob { record, local_path });
                }
            }
            None => {
                if lookup.connectivity_failed && !lookup.cdx_query_succeeded {
                    resolution.deferred += 1;
                    consecutive_connectivity_failures += 1;
                    if consecutive_connectivity_failures >= MAX_EXTRA_CDX_CONNECTIVITY_FAILURES {
                        let remaining = references_len.saturating_sub(index + 1);
                        resolution.deferred += remaining;
                        eprintln!(
                            "stopping linked-file CDX lookups after {consecutive_connectivity_failures} consecutive network failures; deferred {remaining} remaining linked files"
                        );
                        break;
                    }
                } else {
                    resolution.unavailable += 1;
                    consecutive_connectivity_failures = 0;
                }
            }
        }
    }

    Ok(resolution)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct StaticAssetRecoveryReport {
    recovered: usize,
    unavailable: usize,
    deferred: usize,
}

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
    let targets_len = targets.len();
    let mut consecutive_connectivity_failures = 0;
    for (index, target) in targets.into_iter().enumerate() {
        if cancellation.is_cancelled() {
            break;
        }

        let destination = output_dir.join(&target);
        if has_non_empty_file(&destination).await? {
            continue;
        }

        let record = if let Some(record) = records_by_path.get(&target) {
            consecutive_connectivity_failures = 0;
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
                Some(record) => {
                    consecutive_connectivity_failures = 0;
                    record
                }
                None => {
                    if lookup.connectivity_failed && !lookup.cdx_query_succeeded {
                        report.deferred += 1;
                        consecutive_connectivity_failures += 1;
                        if consecutive_connectivity_failures
                            >= MAX_STATIC_ASSET_CDX_CONNECTIVITY_FAILURES
                        {
                            let remaining = targets_len.saturating_sub(index + 1);
                            report.deferred += remaining;
                            eprintln!(
                                "stopping static-asset CDX lookups after {consecutive_connectivity_failures} consecutive network failures; deferred {remaining} remaining static assets"
                            );
                            break;
                        }
                    } else {
                        report.unavailable += 1;
                        consecutive_connectivity_failures = 0;
                    }
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
        if download_record_to_file_limited(
            client,
            archive_root,
            &record,
            &destination,
            max_bytes,
            cancellation,
        )
        .await?
        {
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

        let Some(source) =
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
            let text = String::from_utf8_lossy(&bytes);

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
                    recovery_records_by_path,
                    &output_dir,
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

    for target in fallback_capture_targets(&source_record.original) {
        let mut query = CdxQuery::new(
            target,
            MatchType::Exact,
            fallback_options.strategy,
            archive_root.clone(),
        );
        query.from = fallback_options.from.clone();
        query.to = fallback_options.to.clone();

        let records =
            fetch_all_records_with_policy(client, &query, CdxRetryPolicy::recovery()).await?;
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
            if alternate_records.len() >= MAX_ALTERNATE_PAGE_CAPTURE_CHECKS {
                return Ok(alternate_records);
            }
        }
    }

    Ok(alternate_records)
}

async fn create_static_asset_alias_from_candidate(
    client: &WaybackClient,
    archive_root: &Url,
    recovery_records_by_path: &HashMap<PathBuf, CdxRecord>,
    output_dir: &Path,
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

    Ok(true)
}

async fn ensure_static_asset_alias_source(
    client: &WaybackClient,
    archive_root: &Url,
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
    if download_record_to_file_limited(
        client,
        archive_root,
        record,
        &source,
        max_bytes,
        cancellation,
    )
    .await?
    {
        Ok(Some(source))
    } else {
        Ok(None)
    }
}

async fn existing_static_asset_alias_source(
    output_dir: &Path,
    source: &Path,
    target: &Path,
) -> Result<Option<PathBuf>> {
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

    Ok(if matches.len() == 1 {
        matches.into_iter().next()
    } else {
        None
    })
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
    cdx_query_succeeded: bool,
    connectivity_failed: bool,
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
                match fetch_all_records_with_policy(client, &query, CdxRetryPolicy::recovery())
                    .await
                {
                    Ok(records) => {
                        lookup.cdx_query_succeeded = true;
                        records
                    }
                    Err(error) => {
                        if is_cdx_connectivity_error(&error) {
                            lookup.connectivity_failed = true;
                        }
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

async fn find_extra_download_record(
    client: &WaybackClient,
    archive_root: &Url,
    fallback_options: &FallbackOptions,
    reference: &str,
    max_bytes: u64,
) -> Result<ExtraDownloadLookup> {
    let mut lookup = ExtraDownloadLookup::default();

    for target in fallback_capture_targets(reference) {
        let mut query = CdxQuery::new(
            target.clone(),
            MatchType::Exact,
            fallback_options.strategy,
            archive_root.clone(),
        );
        query.from = fallback_options.from.clone();
        query.to = fallback_options.to.clone();

        let records =
            match fetch_all_records_with_policy(client, &query, CdxRetryPolicy::recovery()).await {
                Ok(records) => {
                    lookup.cdx_query_succeeded = true;
                    records
                }
                Err(error) => {
                    if is_cdx_connectivity_error(&error) {
                        lookup.connectivity_failed = true;
                    }
                    eprintln!("extra download CDX lookup failed for {target}: {error:#}");
                    continue;
                }
            };

        for record in records {
            if record.length.is_some_and(|length| length <= max_bytes) {
                lookup.record = Some(record);
                return Ok(lookup);
            }
        }
    }

    Ok(lookup)
}

fn should_buffer_response(record: &CdxRecord, local_path: &Path, rewrite_links: bool) -> bool {
    should_detect_soft_redirect(record, local_path)
        || (rewrite_links && should_rewrite_as_text(record, local_path))
}

async fn fetch_record_bytes(
    client: &WaybackClient,
    archive_root: &Url,
    record: &CdxRecord,
    cancellation: &CancellationFlag,
) -> Result<Vec<u8>> {
    let snapshot_url = snapshot_url(archive_root, record)?;
    let mut attempt = 0usize;
    let started_at = Instant::now();
    let mut suppressed_retry_messages = 0usize;

    loop {
        match fetch_snapshot_response_once(client, &snapshot_url, record).await {
            Ok(SnapshotResponseAttempt::Ready(response)) => {
                match response
                    .bytes()
                    .await
                    .with_context(|| format!("failed to read {}", record.original))
                {
                    Ok(bytes) => return Ok(bytes.to_vec()),
                    Err(error) if is_retryable_snapshot_error(&error) => {
                        if try_activate_ssh_for_snapshot_error(client, record, &error) {
                            attempt = 0;
                            suppressed_retry_messages = 0;
                            continue;
                        }
                        retry_snapshot_after_error(
                            record,
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
                if try_activate_ssh_for_snapshot_status(client, record, status) {
                    attempt = 0;
                    suppressed_retry_messages = 0;
                    continue;
                }
                retry_snapshot_after_status(
                    record,
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
                if try_activate_ssh_for_snapshot_status(client, record, status) {
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
    cancellation: &CancellationFlag,
) -> Result<()> {
    let snapshot_url = snapshot_url(archive_root, record)?;
    let mut attempt = 0usize;
    let started_at = Instant::now();
    let mut suppressed_retry_messages = 0usize;

    loop {
        match fetch_snapshot_response_once(client, &snapshot_url, record).await {
            Ok(SnapshotResponseAttempt::Ready(response)) => {
                match stream_response_to_file(response, destination).await {
                    Ok(()) => return Ok(()),
                    Err(error) if is_retryable_snapshot_error(&error) => {
                        if try_activate_ssh_for_snapshot_error(client, record, &error) {
                            attempt = 0;
                            suppressed_retry_messages = 0;
                            continue;
                        }
                        retry_snapshot_after_error(
                            record,
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
                if try_activate_ssh_for_snapshot_status(client, record, status) {
                    attempt = 0;
                    suppressed_retry_messages = 0;
                    continue;
                }
                retry_snapshot_after_status(
                    record,
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
                if try_activate_ssh_for_snapshot_status(client, record, status) {
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

async fn download_record_to_file_limited(
    client: &WaybackClient,
    archive_root: &Url,
    record: &CdxRecord,
    destination: &Path,
    max_bytes: u64,
    cancellation: &CancellationFlag,
) -> Result<bool> {
    let snapshot_url = snapshot_url(archive_root, record)?;
    let mut attempt = 0usize;
    let started_at = Instant::now();
    let mut suppressed_retry_messages = 0usize;

    loop {
        match fetch_snapshot_response_once(client, &snapshot_url, record).await {
            Ok(SnapshotResponseAttempt::Ready(response)) => {
                match stream_response_to_file_limited(response, destination, max_bytes).await {
                    Ok(downloaded) => return Ok(downloaded),
                    Err(error) if is_retryable_snapshot_error(&error) => {
                        if try_activate_ssh_for_snapshot_error(client, record, &error) {
                            attempt = 0;
                            suppressed_retry_messages = 0;
                            continue;
                        }
                        retry_snapshot_after_error(
                            record,
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
                if try_activate_ssh_for_snapshot_status(client, record, status) {
                    attempt = 0;
                    suppressed_retry_messages = 0;
                    continue;
                }
                retry_snapshot_after_status(
                    record,
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
                if try_activate_ssh_for_snapshot_status(client, record, status) {
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
        .with_context(|| format!("Wayback returned an error for {}", record.original))?;
    Ok(SnapshotResponseAttempt::Ready(response))
}

async fn retry_snapshot_after_status(
    record: &CdxRecord,
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
            "Wayback snapshot for {} returned {status} on attempt {} after {}{}; retrying in {} seconds",
            record.original,
            attempt_number,
            format_snapshot_retry_elapsed(started_at.elapsed()),
            format_snapshot_suppressed_retries(*suppressed_retry_messages),
            delay.as_secs()
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
            "Wayback snapshot for {} failed on attempt {} after {}{}: {}; retrying in {} seconds",
            record.original,
            attempt_number,
            format_snapshot_retry_elapsed(started_at.elapsed()),
            format_snapshot_suppressed_retries(*suppressed_retry_messages),
            format_anyhow_error_chain(error),
            delay.as_secs()
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

fn is_unavailable_snapshot_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .and_then(reqwest::Error::status)
            .is_some_and(is_unavailable_snapshot_status)
    })
}

fn try_activate_ssh_for_snapshot_status(
    client: &WaybackClient,
    record: &CdxRecord,
    status: StatusCode,
) -> bool {
    if client.is_using_ssh()
        && !matches!(
            status,
            StatusCode::TOO_MANY_REQUESTS | StatusCode::FORBIDDEN
        )
    {
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
                || error.status().is_some_and(is_retryable_snapshot_status)
        })
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
    current_bytes: Vec<u8>,
    local_path: &Path,
) -> Result<(CdxRecord, Vec<u8>)> {
    if !should_detect_soft_redirect(current_record, local_path) {
        return Ok((current_record.clone(), current_bytes));
    }

    let text = String::from_utf8_lossy(&current_bytes);
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
) -> Result<Option<(CdxRecord, Vec<u8>)>> {
    let identity_terms = site_identity_terms(&current_record.original);
    let mut seen_digests = HashSet::new();
    remember_digest(&mut seen_digests, current_record);
    for target in fallback_capture_targets(&current_record.original) {
        let mut query = CdxQuery::new(
            target,
            MatchType::Exact,
            fallback_options.strategy,
            archive_root.clone(),
        );
        query.from = fallback_options.from.clone();
        query.to = fallback_options.to.clone();

        let records = fetch_all_records(client, &query).await?;
        for candidate in records {
            if cancellation.is_cancelled() {
                return Ok(None);
            }
            if candidate.timestamp == current_record.timestamp
                && candidate.original == current_record.original
            {
                continue;
            }
            if !remember_digest(&mut seen_digests, &candidate) {
                continue;
            }
            if !should_detect_soft_redirect(&candidate, local_path) {
                continue;
            }

            println!("{}", candidate.original);
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
            let text = String::from_utf8_lossy(&bytes);
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

async fn write_bytes_atomic(destination: &Path, bytes: &[u8]) -> Result<()> {
    let temp_path = temp_path_for(destination);
    let result = async {
        let mut file = fs::File::create(&temp_path)
            .await
            .with_context(|| format!("failed to create {}", temp_path.display()))?;
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

        let records_by_path = recoverable_static_records_by_path(&mapper, &records, 1024).unwrap();

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
}
