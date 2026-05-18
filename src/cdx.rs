use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::Client;
use url::Url;

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

pub async fn fetch_latest_records(client: &Client, query: &CdxQuery) -> Result<Vec<CdxRecord>> {
    let search_url = query.search_url()?;
    let response = client
        .get(search_url)
        .send()
        .await
        .context("failed to query the Wayback CDX API")?
        .error_for_status()
        .context("Wayback CDX API returned an error")?;

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
}
