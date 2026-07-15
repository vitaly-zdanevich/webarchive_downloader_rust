use std::collections::HashMap;
use std::error::Error as StdError;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::{
    Response, StatusCode,
    header::{HeaderMap, RETRY_AFTER},
};
use tokio::time::sleep;
use url::Url;

use crate::wayback_client::WaybackClient;

pub const MAX_WAYBACK_RETRY_DELAY_SECONDS: u64 = 24 * 60 * 60;
const FIRST_VERBOSE_RETRY_ATTEMPTS: usize = 5;
const RETRY_LOG_EVERY_ATTEMPTS: usize = 10;
const FIRST_CONNECTIVITY_NOTICE_AFTER: Duration = Duration::from_secs(15 * 60);
const CONNECTIVITY_NOTICE_INTERVAL: Duration = Duration::from_secs(60 * 60);
const CDX_THROTTLE_DECAY_AFTER: Duration = Duration::from_secs(10 * 60);

static CDX_COOLDOWN: Mutex<CdxCooldown> = Mutex::new(CdxCooldown {
    next_allowed_at: None,
    throttle_score: 0,
    last_throttle_at: None,
    last_success_at: None,
});

struct CdxCooldown {
    next_allowed_at: Option<Instant>,
    throttle_score: usize,
    last_throttle_at: Option<Instant>,
    last_success_at: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CdxRetryPolicy {
    max_attempts: Option<usize>,
}

impl CdxRetryPolicy {
    pub const fn primary() -> Self {
        Self::unlimited()
    }

    pub const fn recovery() -> Self {
        Self::unlimited()
    }

    pub const fn unlimited() -> Self {
        Self { max_attempts: None }
    }

    #[cfg(test)]
    pub const fn limited(max_attempts: usize) -> Self {
        Self {
            max_attempts: Some(max_attempts),
        }
    }

    fn should_retry_after_attempt(self, attempt: usize) -> bool {
        self.max_attempts.is_none_or(|max| attempt + 1 < max)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CdxRecord {
    pub timestamp: String,
    pub original: String,
    pub mimetype: String,
    pub status_code: u16,
    pub digest: String,
    pub length: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
pub enum MatchType {
    Exact,
    Prefix,
    Host,
    Domain,
}

impl MatchType {
    pub fn as_cdx(self) -> &'static str {
        match self {
            MatchType::Exact => "exact",
            MatchType::Prefix => "prefix",
            MatchType::Host => "host",
            MatchType::Domain => "domain",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, clap::ValueEnum)]
pub enum SnapshotStrategy {
    Latest,
    Earliest,
}

#[derive(Clone, Debug)]
pub struct CdxQuery {
    pub target: String,
    pub match_type: MatchType,
    pub from: Option<String>,
    pub to: Option<String>,
    pub limit: Option<usize>,
    pub strategy: SnapshotStrategy,
    pub archive_root: Url,
}

impl CdxQuery {
    pub fn new(
        target: String,
        match_type: MatchType,
        strategy: SnapshotStrategy,
        archive_root: Url,
    ) -> Self {
        Self {
            target,
            match_type,
            from: None,
            to: None,
            limit: None,
            strategy,
            archive_root,
        }
    }

    pub fn search_url(&self) -> Result<Url> {
        let mut url = self
            .archive_root
            .join("/cdx/search/cdx")
            .context("archive root must be an absolute URL")?;

        {
            let mut pairs = url.query_pairs_mut();
            pairs
                .append_pair("url", &self.target)
                .append_pair("matchType", self.match_type.as_cdx())
                .append_pair("output", "txt")
                .append_pair("fl", "timestamp,original,mimetype,statuscode,digest,length")
                .append_pair("filter", "statuscode:200");

            if let Some(from) = &self.from {
                pairs.append_pair("from", from);
            }
            if let Some(to) = &self.to {
                pairs.append_pair("to", to);
            }
            if let Some(limit) = self.limit {
                pairs.append_pair("limit", &limit.to_string());
            }
        }

        Ok(url)
    }
}

pub async fn fetch_latest_records(
    client: &WaybackClient,
    query: &CdxQuery,
) -> Result<Vec<CdxRecord>> {
    let search_url = query.search_url()?;
    let response = send_cdx_request(client, search_url, CdxRetryPolicy::primary()).await?;

    let mut records_by_url: HashMap<String, CdxRecord> = HashMap::new();
    let mut stream = response.bytes_stream();
    let mut pending = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("failed while reading CDX response")?;
        pending.extend_from_slice(&chunk);

        while let Some(line_end) = pending.iter().position(|byte| *byte == b'\n') {
            let line = pending.drain(..=line_end).collect::<Vec<_>>();
            ingest_cdx_line(&line, query.strategy, &mut records_by_url)?;
        }
    }

    if !pending.is_empty() {
        ingest_cdx_line(&pending, query.strategy, &mut records_by_url)?;
    }

    let mut records = records_by_url.into_values().collect::<Vec<_>>();
    records.sort_by(|left, right| {
        left.original
            .cmp(&right.original)
            .then(left.timestamp.cmp(&right.timestamp))
    });
    Ok(records)
}

pub async fn fetch_all_records(client: &WaybackClient, query: &CdxQuery) -> Result<Vec<CdxRecord>> {
    fetch_all_records_with_policy(client, query, CdxRetryPolicy::primary()).await
}

pub async fn fetch_all_records_with_policy(
    client: &WaybackClient,
    query: &CdxQuery,
    retry_policy: CdxRetryPolicy,
) -> Result<Vec<CdxRecord>> {
    let search_url = query.search_url()?;
    let mut attempt = 0usize;
    let started_at = Instant::now();
    let mut suppressed_retry_messages = 0usize;

    loop {
        let response = send_cdx_request(client, search_url.clone(), retry_policy).await?;
        match read_all_cdx_records(response, query.strategy).await {
            Ok(records) => return Ok(records),
            Err(error)
                if retry_policy.should_retry_after_attempt(attempt)
                    && is_cdx_connectivity_error(&error) =>
            {
                let attempt_number = attempt + 1;
                let elapsed = started_at.elapsed();
                let mut delay = retry_after_delay(&HeaderMap::new(), attempt);
                if try_switch_wayback_route(
                    client,
                    &format!("CDX API response read failed: {}", error),
                ) {
                    attempt = 0;
                    suppressed_retry_messages = 0;
                    continue;
                }
                delay = remember_cdx_cooldown(delay, "CDX API response read failed");
                if should_log_retry_attempt(attempt_number) {
                    eprintln!(
                        "Wayback CDX API response read failed on attempt {} after {}{}: {}; retrying in {} seconds",
                        attempt_number,
                        format_retry_elapsed(elapsed),
                        format_suppressed_retries(suppressed_retry_messages),
                        error,
                        delay.as_secs()
                    );
                    suppressed_retry_messages = 0;
                } else {
                    suppressed_retry_messages = suppressed_retry_messages.saturating_add(1);
                }
                sleep(delay).await;
                attempt = attempt.saturating_add(1);
            }
            Err(error) => return Err(error),
        }
    }
}

async fn read_all_cdx_records(
    response: Response,
    strategy: SnapshotStrategy,
) -> Result<Vec<CdxRecord>> {
    let mut records = Vec::new();
    let mut stream = response.bytes_stream();
    let mut pending = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("failed while reading CDX response")?;
        pending.extend_from_slice(&chunk);

        while let Some(line_end) = pending.iter().position(|byte| *byte == b'\n') {
            let line = pending.drain(..=line_end).collect::<Vec<_>>();
            ingest_all_cdx_line(&line, &mut records)?;
        }
    }

    if !pending.is_empty() {
        ingest_all_cdx_line(&pending, &mut records)?;
    }

    sort_records_for_strategy(&mut records, strategy);
    Ok(records)
}

pub fn is_cdx_connectivity_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<reqwest::Error>().is_some_and(|error| {
            error.is_connect()
                || error.is_timeout()
                || error.status().is_some_and(|status| {
                    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
                })
        })
    })
}

async fn send_cdx_request(
    client: &WaybackClient,
    search_url: Url,
    retry_policy: CdxRetryPolicy,
) -> Result<Response> {
    let mut attempt = 0usize;
    let started_at = Instant::now();
    let mut suppressed_retry_messages = 0usize;
    let mut next_connectivity_notice_after = FIRST_CONNECTIVITY_NOTICE_AFTER;
    loop {
        wait_for_cdx_cooldown().await;

        let response = match client.get(search_url.clone()).send().await {
            Ok(response) => response,
            Err(error) if retry_policy.should_retry_after_attempt(attempt) => {
                let attempt_number = attempt + 1;
                let elapsed = started_at.elapsed();
                let mut delay = retry_after_delay(&HeaderMap::new(), attempt);
                if try_switch_wayback_route(
                    client,
                    &format!("CDX API request failed: {}", format_error_chain(&error)),
                ) {
                    attempt = 0;
                    suppressed_retry_messages = 0;
                    continue;
                }
                if is_reqwest_connectivity_error(&error) {
                    delay = remember_cdx_cooldown(delay, "CDX API connection failed");
                }
                if should_log_retry_attempt(attempt_number) {
                    eprintln!(
                        "Wayback CDX API request failed on attempt {} after {}{}: {}; retrying in {} seconds",
                        attempt_number,
                        format_retry_elapsed(elapsed),
                        format_suppressed_retries(suppressed_retry_messages),
                        format_error_chain(&error),
                        delay.as_secs()
                    );
                    suppressed_retry_messages = 0;
                } else {
                    suppressed_retry_messages = suppressed_retry_messages.saturating_add(1);
                }

                if is_reqwest_connectivity_error(&error)
                    && elapsed >= next_connectivity_notice_after
                {
                    eprintln!(
                        "Wayback CDX API has been unreachable at TCP connect level for {}; this is not HTTP throttling. The downloader will keep retrying. Check network, firewall, proxy, VPN, or route access to https://web.archive.org/.",
                        format_retry_elapsed(elapsed)
                    );
                    next_connectivity_notice_after = elapsed + CONNECTIVITY_NOTICE_INTERVAL;
                }

                sleep(delay).await;
                attempt = attempt.saturating_add(1);
                continue;
            }
            Err(error) => {
                return Err(error).context("failed to query the Wayback CDX API");
            }
        };

        if is_retryable_cdx_status(response.status())
            && retry_policy.should_retry_after_attempt(attempt)
        {
            let attempt_number = attempt + 1;
            let elapsed = started_at.elapsed();
            let status = response.status();
            let mut delay = retry_after_delay(response.headers(), attempt);
            let should_switch_route =
                !client.is_using_ssh() || matches!(status, StatusCode::TOO_MANY_REQUESTS);
            if should_switch_route
                && try_switch_wayback_route(client, &format!("CDX API returned {status}"))
            {
                attempt = 0;
                suppressed_retry_messages = 0;
                continue;
            }
            delay = remember_cdx_cooldown(delay, &format!("CDX API returned {status}"));
            if should_log_retry_attempt(attempt_number) {
                eprintln!(
                    "Wayback CDX API returned {status} on attempt {} after {}{}; retrying in {} seconds",
                    attempt_number,
                    format_retry_elapsed(elapsed),
                    format_suppressed_retries(suppressed_retry_messages),
                    delay.as_secs()
                );
                suppressed_retry_messages = 0;
            } else {
                suppressed_retry_messages = suppressed_retry_messages.saturating_add(1);
            }
            sleep(delay).await;
            attempt = attempt.saturating_add(1);
            continue;
        }

        if response.status() == StatusCode::FORBIDDEN
            && try_switch_wayback_route(client, "CDX API returned 403 Forbidden")
        {
            attempt = 0;
            suppressed_retry_messages = 0;
            continue;
        }

        let response = response
            .error_for_status()
            .context("Wayback CDX API returned an error")?;
        remember_cdx_success();
        return Ok(response);
    }
}

async fn wait_for_cdx_cooldown() {
    loop {
        let Some(remaining) = cdx_cooldown_remaining() else {
            return;
        };
        eprintln!(
            "Wayback CDX shared cooldown active; waiting {} before next CDX request",
            format_retry_elapsed(remaining)
        );
        sleep(remaining).await;
    }
}

fn remember_cdx_cooldown(delay: Duration, reason: &str) -> Duration {
    let mut cooldown = lock_unpoisoned(&CDX_COOLDOWN);
    let now = Instant::now();
    decay_cdx_throttle(&mut cooldown, now);

    let throttle_index = cooldown.throttle_score;
    cooldown.throttle_score = cooldown.throttle_score.saturating_add(1);
    cooldown.last_throttle_at = Some(now);

    let global_delay = retry_after_delay(&HeaderMap::new(), throttle_index);
    let delay = delay.max(global_delay);
    let next_allowed_at = now + delay;
    let effective_next_allowed_at = cooldown
        .next_allowed_at
        .filter(|current| *current > next_allowed_at)
        .unwrap_or(next_allowed_at);
    if cooldown
        .next_allowed_at
        .is_none_or(|current| next_allowed_at > current)
    {
        cooldown.next_allowed_at = Some(next_allowed_at);
        eprintln!(
            "Wayback CDX shared cooldown set to {} because {reason}",
            format_retry_elapsed(delay)
        );
    }
    effective_next_allowed_at
        .checked_duration_since(now)
        .unwrap_or(Duration::ZERO)
}

fn remember_cdx_success() {
    let now = Instant::now();
    let mut cooldown = lock_unpoisoned(&CDX_COOLDOWN);
    decay_cdx_throttle(&mut cooldown, now);
    cooldown.last_success_at = Some(now);
    if cooldown
        .next_allowed_at
        .is_some_and(|next_allowed_at| next_allowed_at <= now)
    {
        cooldown.next_allowed_at = None;
    }
}

fn decay_cdx_throttle(cooldown: &mut CdxCooldown, now: Instant) {
    let Some(last_success_at) = cooldown.last_success_at else {
        return;
    };
    if cooldown
        .last_throttle_at
        .is_some_and(|last_throttle_at| last_success_at <= last_throttle_at)
    {
        return;
    }
    if now
        .checked_duration_since(last_success_at)
        .is_some_and(|elapsed| elapsed >= CDX_THROTTLE_DECAY_AFTER)
    {
        cooldown.throttle_score = 0;
        cooldown.last_throttle_at = None;
        cooldown.last_success_at = None;
    }
}

fn cdx_cooldown_remaining() -> Option<Duration> {
    lock_unpoisoned(&CDX_COOLDOWN)
        .next_allowed_at
        .and_then(|next_allowed_at| next_allowed_at.checked_duration_since(Instant::now()))
        .filter(|remaining| !remaining.is_zero())
}

fn is_retryable_cdx_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn try_switch_wayback_route(client: &WaybackClient, reason: &str) -> bool {
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

fn is_reqwest_connectivity_error(error: &reqwest::Error) -> bool {
    error.is_connect() || error.is_timeout()
}

fn should_log_retry_attempt(attempt_number: usize) -> bool {
    attempt_number <= FIRST_VERBOSE_RETRY_ATTEMPTS
        || attempt_number.is_multiple_of(RETRY_LOG_EVERY_ATTEMPTS)
}

fn format_suppressed_retries(count: usize) -> String {
    match count {
        0 => String::new(),
        1 => " (1 similar retry message suppressed)".to_owned(),
        count => format!(" ({count} similar retry messages suppressed)"),
    }
}

fn format_error_chain(error: &dyn StdError) -> String {
    let mut message = error.to_string();
    let mut source = error.source();

    while let Some(error) = source {
        let part = error.to_string();
        if !message.contains(&part) {
            message.push_str(": ");
            message.push_str(&part);
        }
        source = error.source();
    }

    message
}

fn format_retry_elapsed(duration: Duration) -> String {
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

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn retry_after_delay(headers: &HeaderMap, attempt: usize) -> Duration {
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

fn sort_records_for_strategy(records: &mut [CdxRecord], strategy: SnapshotStrategy) {
    records.sort_by(|left, right| {
        let timestamp_order = match strategy {
            SnapshotStrategy::Latest => right.timestamp.cmp(&left.timestamp),
            SnapshotStrategy::Earliest => left.timestamp.cmp(&right.timestamp),
        };
        timestamp_order.then_with(|| left.original.cmp(&right.original))
    });
}

fn ingest_cdx_line(
    line: &[u8],
    strategy: SnapshotStrategy,
    records_by_url: &mut HashMap<String, CdxRecord>,
) -> Result<()> {
    let line = String::from_utf8_lossy(line);
    let line = line.trim();
    if line.is_empty() {
        return Ok(());
    }

    let record = parse_cdx_line(line)?;
    let key = canonical_original_key(&record.original);
    match records_by_url.get(&key) {
        Some(existing) if keep_existing(existing, &record, strategy) => {}
        _ => {
            records_by_url.insert(key, record);
        }
    }
    Ok(())
}

fn ingest_all_cdx_line(line: &[u8], records: &mut Vec<CdxRecord>) -> Result<()> {
    let line = String::from_utf8_lossy(line);
    let line = line.trim();
    if line.is_empty() {
        return Ok(());
    }

    records.push(parse_cdx_line(line)?);
    Ok(())
}

fn keep_existing(existing: &CdxRecord, incoming: &CdxRecord, strategy: SnapshotStrategy) -> bool {
    match strategy {
        SnapshotStrategy::Latest => existing.timestamp >= incoming.timestamp,
        SnapshotStrategy::Earliest => existing.timestamp <= incoming.timestamp,
    }
}

pub fn parse_cdx_line(line: &str) -> Result<CdxRecord> {
    let mut parts = line.split_whitespace();
    let Some(timestamp) = parts.next() else {
        bail!("CDX line is missing timestamp: {line}");
    };
    let Some(original) = parts.next() else {
        bail!("CDX line is missing original URL: {line}");
    };
    let Some(mimetype) = parts.next() else {
        bail!("CDX line is missing mimetype: {line}");
    };
    let Some(status_code) = parts.next() else {
        bail!("CDX line is missing status code: {line}");
    };
    let Some(digest) = parts.next() else {
        bail!("CDX line is missing digest: {line}");
    };
    let length = parts.next().and_then(|value| value.parse::<u64>().ok());

    Ok(CdxRecord {
        timestamp: timestamp.to_owned(),
        original: original.to_owned(),
        mimetype: mimetype.to_owned(),
        status_code: status_code
            .parse()
            .with_context(|| format!("invalid CDX status code: {status_code}"))?,
        digest: digest.to_owned(),
        length,
    })
}

pub fn canonical_original_key(original: &str) -> String {
    let Ok(mut url) = Url::parse(original) else {
        return original.to_owned();
    };
    url.set_fragment(None);
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    static CDX_COOLDOWN_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn parses_text_cdx_line() {
        let record =
            parse_cdx_line("20200102030405 http://example.com/ text/html 200 ABCDEFG 1234")
                .unwrap();

        assert_eq!(record.timestamp, "20200102030405");
        assert_eq!(record.original, "http://example.com/");
        assert_eq!(record.mimetype, "text/html");
        assert_eq!(record.status_code, 200);
        assert_eq!(record.digest, "ABCDEFG");
        assert_eq!(record.length, Some(1234));
    }

    #[test]
    fn builds_cdx_search_url() {
        let archive_root = Url::parse("https://web.archive.org").unwrap();
        let query = CdxQuery::new(
            "example.com".to_owned(),
            MatchType::Domain,
            SnapshotStrategy::Latest,
            archive_root,
        );

        let url = query.search_url().unwrap().to_string();
        assert!(url.starts_with("https://web.archive.org/cdx/search/cdx?"));
        assert!(url.contains("url=example.com"));
        assert!(url.contains("matchType=domain"));
        assert!(url.contains("fl=timestamp%2Coriginal%2Cmimetype%2Cstatuscode%2Cdigest%2Clength"));
        assert!(url.contains("filter=statuscode%3A200"));
    }

    #[test]
    fn uses_retry_after_seconds_for_cdx_429_delay() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("42"));

        assert_eq!(retry_after_delay(&headers, 0), Duration::from_secs(42));
    }

    #[test]
    fn caps_retry_after_seconds_for_cdx_429_delay() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("999999"));

        assert_eq!(
            retry_after_delay(&headers, 0),
            Duration::from_secs(MAX_WAYBACK_RETRY_DELAY_SECONDS)
        );
    }

    #[test]
    fn falls_back_to_exponential_cdx_429_delay() {
        let headers = HeaderMap::new();

        assert_eq!(retry_after_delay(&headers, 0), Duration::from_secs(5));
        assert_eq!(retry_after_delay(&headers, 2), Duration::from_secs(20));
        assert_eq!(retry_after_delay(&headers, 5), Duration::from_secs(160));
        assert_eq!(retry_after_delay(&headers, 7), Duration::from_secs(640));
        assert_eq!(retry_after_delay(&headers, 10), Duration::from_secs(5120));
        assert_eq!(retry_after_delay(&headers, 11), Duration::from_secs(10240));
        assert_eq!(retry_after_delay(&headers, 14), Duration::from_secs(81920));
        assert_eq!(
            retry_after_delay(&headers, 100),
            Duration::from_secs(MAX_WAYBACK_RETRY_DELAY_SECONDS)
        );
    }

    #[test]
    fn formats_retry_elapsed_time() {
        assert_eq!(format_retry_elapsed(Duration::from_secs(7)), "7s");
        assert_eq!(format_retry_elapsed(Duration::from_secs(67)), "1m 7s");
        assert_eq!(format_retry_elapsed(Duration::from_secs(3667)), "1h 1m 7s");
    }

    #[test]
    fn shared_cdx_cooldown_extends_but_does_not_shorten() {
        let _guard = CDX_COOLDOWN_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        reset_cdx_cooldown_for_test();

        remember_cdx_cooldown(Duration::from_secs(30), "test");
        let first_deadline = cdx_cooldown_deadline_for_test().unwrap();

        remember_cdx_cooldown(Duration::from_secs(5), "test");
        assert_eq!(cdx_cooldown_deadline_for_test(), Some(first_deadline));

        remember_cdx_cooldown(Duration::from_secs(60), "test");
        assert!(cdx_cooldown_deadline_for_test().unwrap() > first_deadline);

        reset_cdx_cooldown_for_test();
    }

    #[test]
    fn shared_cdx_cooldown_retry_delay_uses_existing_longer_deadline() {
        let _guard = CDX_COOLDOWN_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        reset_cdx_cooldown_for_test();

        assert_eq!(
            remember_cdx_cooldown(Duration::from_secs(60), "test"),
            Duration::from_secs(60)
        );
        let first_deadline = cdx_cooldown_deadline_for_test().unwrap();
        let retry_delay = remember_cdx_cooldown(Duration::from_secs(5), "test");

        assert!(Instant::now() + retry_delay >= first_deadline);

        reset_cdx_cooldown_for_test();
    }

    #[test]
    fn shared_cdx_cooldown_escalates_across_intermittent_successes() {
        let _guard = CDX_COOLDOWN_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        reset_cdx_cooldown_for_test();

        remember_cdx_cooldown(Duration::from_secs(5), "test");
        assert_eq!(cdx_cooldown_throttle_score_for_test(), 1);
        let first_remaining = cdx_cooldown_remaining().unwrap();
        assert!(first_remaining <= Duration::from_secs(5));

        remember_cdx_success();
        assert_eq!(cdx_cooldown_throttle_score_for_test(), 1);

        remember_cdx_cooldown(Duration::from_secs(5), "test");
        assert_eq!(cdx_cooldown_throttle_score_for_test(), 2);
        let second_remaining = cdx_cooldown_remaining().unwrap();
        assert!(second_remaining > first_remaining);

        remember_cdx_success();
        age_cdx_success_for_test(CDX_THROTTLE_DECAY_AFTER + Duration::from_secs(1));
        remember_cdx_success();
        assert_eq!(cdx_cooldown_throttle_score_for_test(), 0);

        reset_cdx_cooldown_for_test();
    }

    #[test]
    fn shared_cdx_cooldown_does_not_decay_just_because_backoff_elapsed() {
        let _guard = CDX_COOLDOWN_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        reset_cdx_cooldown_for_test();

        for _ in 0..8 {
            remember_cdx_cooldown(Duration::from_secs(5), "test");
        }
        assert_eq!(cdx_cooldown_throttle_score_for_test(), 8);

        age_cdx_throttle_for_test(CDX_THROTTLE_DECAY_AFTER + Duration::from_secs(1));
        let retry_delay = remember_cdx_cooldown(Duration::from_secs(5), "test");

        assert!(retry_delay >= Duration::from_secs(1280));

        reset_cdx_cooldown_for_test();
    }

    #[test]
    fn logs_first_retry_attempts_then_periodically() {
        assert!(should_log_retry_attempt(1));
        assert!(should_log_retry_attempt(FIRST_VERBOSE_RETRY_ATTEMPTS));
        assert!(!should_log_retry_attempt(FIRST_VERBOSE_RETRY_ATTEMPTS + 1));
        assert!(should_log_retry_attempt(RETRY_LOG_EVERY_ATTEMPTS));
        assert!(!should_log_retry_attempt(RETRY_LOG_EVERY_ATTEMPTS + 1));
    }

    #[test]
    fn formats_suppressed_retry_count() {
        assert_eq!(format_suppressed_retries(0), "");
        assert_eq!(
            format_suppressed_retries(1),
            " (1 similar retry message suppressed)"
        );
        assert_eq!(
            format_suppressed_retries(9),
            " (9 similar retry messages suppressed)"
        );
    }

    #[test]
    fn retries_primary_cdx_requests_indefinitely() {
        let policy = CdxRetryPolicy::primary();

        assert!(policy.should_retry_after_attempt(0));
        assert!(policy.should_retry_after_attempt(1_000_000));
    }

    #[test]
    fn recovery_cdx_requests_are_unbounded() {
        let policy = CdxRetryPolicy::recovery();

        assert!(policy.should_retry_after_attempt(0));
        assert!(policy.should_retry_after_attempt(1_000_000));
    }

    #[test]
    fn limited_policy_stops_after_configured_attempts() {
        let policy = CdxRetryPolicy::limited(3);

        assert!(policy.should_retry_after_attempt(0));
        assert!(policy.should_retry_after_attempt(1));
        assert!(!policy.should_retry_after_attempt(2));
    }

    #[test]
    fn retries_only_transient_cdx_statuses() {
        assert!(is_retryable_cdx_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_cdx_status(StatusCode::BAD_GATEWAY));
        assert!(!is_retryable_cdx_status(StatusCode::NOT_FOUND));
        assert!(!is_retryable_cdx_status(StatusCode::FORBIDDEN));
    }

    fn cdx_cooldown_deadline_for_test() -> Option<Instant> {
        lock_unpoisoned(&CDX_COOLDOWN).next_allowed_at
    }

    fn cdx_cooldown_throttle_score_for_test() -> usize {
        lock_unpoisoned(&CDX_COOLDOWN).throttle_score
    }

    fn age_cdx_throttle_for_test(age: Duration) {
        lock_unpoisoned(&CDX_COOLDOWN).last_throttle_at = Some(Instant::now() - age);
    }

    fn age_cdx_success_for_test(age: Duration) {
        let mut cooldown = lock_unpoisoned(&CDX_COOLDOWN);
        let success_at = Instant::now() - age;
        cooldown.last_success_at = Some(success_at);
        cooldown.last_throttle_at = Some(success_at - Duration::from_secs(1));
    }

    fn reset_cdx_cooldown_for_test() {
        let mut cooldown = lock_unpoisoned(&CDX_COOLDOWN);
        cooldown.next_allowed_at = None;
        cooldown.throttle_score = 0;
        cooldown.last_throttle_at = None;
        cooldown.last_success_at = None;
    }
}
