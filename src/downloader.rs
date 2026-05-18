use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use reqwest::Client;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use url::Url;

use crate::cdx::{CdxQuery, CdxRecord, fetch_latest_records};
use crate::pathmap::{SiteMapper, is_css_mimetype, is_html_mimetype, normalize_lookup_url};
use crate::rewrite::{RewriteContext, rewrite_css, rewrite_html};

#[derive(Clone, Debug)]
pub struct DownloadOptions {
    pub output_dir: PathBuf,
    pub no_clobber: bool,
    pub rewrite_links: bool,
    pub cancellation: CancellationFlag,
}

#[derive(Clone, Debug, Default)]
pub struct DownloadReport {
    pub discovered: usize,
    pub downloaded: usize,
    pub skipped: usize,
    pub cancelled: usize,
    pub failed: usize,
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

pub async fn download_site(
    client: Client,
    mapper: SiteMapper,
    query: CdxQuery,
    options: DownloadOptions,
) -> Result<DownloadReport> {
    let records = fetch_latest_records(&client, &query).await?;
    let discovered = records.len();
    let known_paths = mapper.records_to_paths(&records)?;

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
        "discovered {discovered} archived URLs; downloading {} unique output paths",
        jobs.len()
    );
    if duplicate_paths > 0 {
        println!("skipping {duplicate_paths} duplicate local path mappings");
    }

    let archive_root = query.archive_root;

    let mut report = DownloadReport {
        discovered,
        skipped: duplicate_paths,
        output_dir: options.output_dir.clone(),
        ..DownloadReport::default()
    };

    for job in jobs {
        let result = download_one(&client, &archive_root, &options, &known_paths, job).await;
        match result {
            Ok(DownloadStatus::Downloaded) => report.downloaded += 1,
            Ok(DownloadStatus::Skipped) => report.skipped += 1,
            Ok(DownloadStatus::Cancelled) => report.cancelled += 1,
            Err(error) => {
                report.failed += 1;
                eprintln!("download failed: {error:#}");
            }
        }
    }

    Ok(report)
}

pub async fn list_records(client: &Client, query: &CdxQuery) -> Result<Vec<CdxRecord>> {
    fetch_latest_records(client, query).await
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DownloadStatus {
    Downloaded,
    Skipped,
    Cancelled,
}

async fn download_one(
    client: &Client,
    archive_root: &Url,
    options: &DownloadOptions,
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

    let snapshot_url = snapshot_url(archive_root, &job.record)?;
    println!(
        "downloading {} -> {}",
        job.record.original,
        job.local_path.display()
    );
    let response = client
        .get(snapshot_url)
        .send()
        .await
        .with_context(|| format!("failed to request {}", job.record.original))?
        .error_for_status()
        .with_context(|| format!("Wayback returned an error for {}", job.record.original))?;

    if options.rewrite_links && should_rewrite_as_text(&job.record, &job.local_path) {
        let bytes = response
            .bytes()
            .await
            .with_context(|| format!("failed to read {}", job.record.original))?;
        let text = String::from_utf8_lossy(&bytes);
        let context =
            RewriteContext::new(&job.record.original, job.local_path.clone(), known_paths)?;
        let rewritten = if is_css_mimetype(&job.record.mimetype) {
            rewrite_css(&text, &context)
        } else {
            rewrite_html(&text, &context)?
        };
        write_bytes_atomic(&destination, rewritten.as_bytes()).await?;
    } else {
        stream_response_to_file(response, &destination).await?;
    }

    println!("saved {}", job.local_path.display());
    Ok(DownloadStatus::Downloaded)
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

pub fn snapshot_url(archive_root: &Url, record: &CdxRecord) -> Result<Url> {
    archive_root
        .join(&format!("/web/{}id_/{}", record.timestamp, record.original))
        .context("failed to build Wayback snapshot URL")
}

pub fn build_client(user_agent: &str, timeout: Duration) -> Result<Client> {
    Client::builder()
        .user_agent(user_agent)
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .context("failed to build HTTP client")
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
