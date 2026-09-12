//! Exercises preservation behavior against a local mock Wayback server.

use std::path::Path;
use std::time::Duration;

use url::Url;
use webarchive_downloader_rust::cdx::{CdxQuery, MatchType, SnapshotStrategy};
use webarchive_downloader_rust::downloader::{
	CancellationFlag, DownloadOptions, RepairOptions, build_client, download_site,
	repair_output_dir,
};
use webarchive_downloader_rust::pathmap::SiteMapper;
use wiremock::matchers::{path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Supplies deterministic CDX results without contacting the Internet Archive.
async fn mock_cdx(server: &MockServer, target: &str, match_type: &str, records: &str) {
	Mock::given(path("/cdx/search/cdx"))
		.and(query_param("url", target))
		.and(query_param("matchType", match_type))
		.respond_with(ResponseTemplate::new(200).set_body_string(records))
		.mount(server)
		.await;
}

/// Serves one archived response from the identity replay endpoint.
async fn mock_capture(
	server: &MockServer,
	timestamp: &str,
	original: &str,
	status: u16,
	body: &str,
) {
	let replay = Url::parse(&format!("{}/web/{timestamp}id_/{original}", server.uri())).unwrap();
	let mut mock = Mock::given(path(replay.path()));
	for (key, value) in replay.query_pairs() {
		mock = mock.and(query_param(key, value));
	}
	mock.respond_with(ResponseTemplate::new(status).set_body_string(body))
		.mount(server)
		.await;
}

/// Enables ordinary rewriting and validation with an isolated output directory.
fn options(output_dir: &Path, max_bytes: Option<u64>) -> DownloadOptions {
	DownloadOptions {
		output_dir: output_dir.to_owned(),
		no_clobber: true,
		rewrite_links: true,
		extra_download_max_bytes: max_bytes,
		validate_links: true,
		cancellation: CancellationFlag::new(),
	}
}

#[tokio::test]
async fn download_and_repair_keep_unresolved_references() {
	let server = MockServer::start().await;
	let root = Url::parse(&server.uri()).unwrap();
	let output = tempfile::tempdir().unwrap();
	let client = build_client("preservation-test", Duration::from_secs(5), Vec::new()).unwrap();
	mock_cdx(
		&server,
		"example.com",
		"host",
		"20260911000000 http://example.com/index.html text/html 200 DIGEST 300\n",
	)
	.await;
	mock_capture(&server, "20260911000000", "http://example.com/index.html", 200,
        r#"<html><body><a class='topictitle' href='forums/viewtopic.php?t=14887'>[RAS2] Diary Entry #20</a><a class='username-coloured' href='forums/memberlist.php?mode=viewprofile&amp;u=102'>jonathan</a><img src='images/unavailable.gif'><a href='https://unrelated.invalid/game.zip'>Download</a></body></html>"#
    ).await;

	let query = CdxQuery::new(
		"example.com".to_owned(),
		MatchType::Host,
		SnapshotStrategy::Latest,
		root.clone(),
	);
	let report = download_site(
		client.clone(),
		SiteMapper::new("example.com").unwrap(),
		query,
		options(output.path(), None),
	)
	.await
	.unwrap();
	assert_eq!(report.downloaded, 1);
	assert_eq!(report.failed, 0);
	assert_eq!(report.missing_image_sources, 0);
	assert!(report.missing_local_links >= 3);
	let before_repair = std::fs::read_to_string(output.path().join("index.html")).unwrap();
	assert_eq!(before_repair.matches("href=").count(), 3);
	assert!(
		before_repair.contains("src='images/unavailable.gif'")
			|| before_repair.contains("src=\"images/unavailable.gif\"")
	);
	assert!(before_repair.contains("https://unrelated.invalid/game.zip"));

	let repair = repair_output_dir(
		client,
		SiteMapper::new("example.com").unwrap(),
		RepairOptions {
			output_dir: output.path().to_owned(),
			archive_root: root,
			match_type: MatchType::Host,
			from: None,
			to: None,
			limit: None,
			strategy: SnapshotStrategy::Latest,
			extra_download_max_bytes: None,
			validate_links: true,
			cancellation: CancellationFlag::new(),
		},
	)
	.await
	.unwrap();
	assert_eq!(repair.missing_image_sources, 0);
	assert_eq!(
		std::fs::read_to_string(output.path().join("index.html")).unwrap(),
		before_repair
	);
}

#[tokio::test]
async fn linked_download_with_unknown_length_respects_only_explicit_cap() {
	let server = MockServer::start().await;
	let root = Url::parse(&server.uri()).unwrap();
	mock_cdx(
		&server,
		"example.com",
		"host",
		"20260911000000 http://example.com/index.html text/html 200 PAGE 100\n",
	)
	.await;
	mock_cdx(
        &server,
        "http://downloads.example.com/game.zip",
        "exact",
        "20260911000000 http://downloads.example.com/game.zip application/zip 200 FILE -\n20260910000000 http://downloads.example.com/game.zip application/zip 200 FILE -\n",
    )
    .await;
	mock_capture(
		&server,
		"20260911000000",
		"http://example.com/index.html",
		200,
		"<html><body><a href='http://downloads.example.com/game.zip'>Download</a></body></html>",
	)
	.await;
	mock_capture(
		&server,
		"20260911000000",
		"http://downloads.example.com/game.zip",
		404,
		"Not found",
	)
	.await;
	mock_capture(
		&server,
		"20260910000000",
		"http://downloads.example.com/game.zip",
		200,
		"mock archive payload",
	)
	.await;

	for (cap, expected_downloads) in [(8, 0), (u64::MAX, 1)] {
		let output = tempfile::tempdir().unwrap();
		let client = build_client("preservation-test", Duration::from_secs(5), Vec::new()).unwrap();
		let query = CdxQuery::new(
			"example.com".to_owned(),
			MatchType::Host,
			SnapshotStrategy::Latest,
			root.clone(),
		);
		let report = download_site(
			client,
			SiteMapper::new("example.com").unwrap(),
			query,
			options(output.path(), Some(cap)),
		)
		.await
		.unwrap();
		assert_eq!(report.failed, 0);
		assert_eq!(report.extra_downloads, expected_downloads);
		let destination = output.path().join("_hosts/downloads.example.com/game.zip");
		assert_eq!(destination.exists(), expected_downloads == 1);
		if expected_downloads == 1 {
			assert_eq!(std::fs::read(destination).unwrap(), b"mock archive payload");
		}
	}
}

#[tokio::test]
async fn missing_selected_replays_fall_back_for_pages_and_binaries() {
	let server = MockServer::start().await;
	let root = Url::parse(&server.uri()).unwrap();
	let output = tempfile::tempdir().unwrap();
	let client = build_client("preservation-test", Duration::from_secs(5), Vec::new()).unwrap();
	mock_cdx(&server, "example.com", "host", "20260911000000 http://example.com/index.html text/html 200 PAGE 100\n20260911000000 http://example.com/game.zip application/zip 200 FILE 100\n").await;
	for (name, mime, body) in [
		(
			"index.html",
			"text/html",
			"<html><body>Archived content</body></html>",
		),
		("game.zip", "application/zip", "mock archive payload"),
	] {
		let original = format!("http://example.com/{name}");
		mock_cdx(
			&server,
			&original,
			"exact",
			&format!(
				"20260911000000 {original} {mime} 200 SAME 100\n20260910000000 {original} {mime} 200 SAME 100\n"
			),
		)
		.await;
		mock_capture(&server, "20260911000000", &original, 404, "Not found").await;
		mock_capture(&server, "20260910000000", &original, 200, body).await;
	}
	let query = CdxQuery::new(
		"example.com".to_owned(),
		MatchType::Host,
		SnapshotStrategy::Latest,
		root,
	);
	let report = download_site(
		client,
		SiteMapper::new("example.com").unwrap(),
		query,
		options(output.path(), None),
	)
	.await
	.unwrap();
	assert_eq!(report.downloaded, 2);
	assert_eq!(report.failed, 0);
	assert_eq!(report.unavailable_snapshots, 0);
	assert!(
		std::fs::read_to_string(output.path().join("index.html"))
			.unwrap()
			.contains("Archived content")
	);
	assert_eq!(
		std::fs::read(output.path().join("game.zip")).unwrap(),
		b"mock archive payload"
	);
}

#[tokio::test]
async fn failed_alternate_lookup_is_not_reported_as_missing_content() {
	let server = MockServer::start().await;
	let root = Url::parse(&server.uri()).unwrap();
	let output = tempfile::tempdir().unwrap();
	let client = build_client("preservation-test", Duration::from_secs(5), Vec::new()).unwrap();
	mock_cdx(
		&server,
		"example.com",
		"host",
		"20260911000000 http://example.com/index.html text/html 200 PAGE 100\n",
	)
	.await;
	mock_capture(
		&server,
		"20260911000000",
		"http://example.com/index.html",
		404,
		"Not found",
	)
	.await;
	Mock::given(path("/cdx/search/cdx"))
		.and(query_param("matchType", "exact"))
		.respond_with(ResponseTemplate::new(404))
		.mount(&server)
		.await;
	let query = CdxQuery::new(
		"example.com".to_owned(),
		MatchType::Host,
		SnapshotStrategy::Latest,
		root,
	);
	let report = download_site(
		client,
		SiteMapper::new("example.com").unwrap(),
		query,
		options(output.path(), None),
	)
	.await
	.unwrap();
	assert_eq!(report.downloaded, 0);
	assert_eq!(report.unavailable_snapshots, 0);
	assert_eq!(report.failed, 1);
	assert!(!output.path().join("index.html").exists());
}

#[tokio::test]
async fn missing_replay_search_checks_more_than_twenty_captures() {
	let server = MockServer::start().await;
	let root = Url::parse(&server.uri()).unwrap();
	let output = tempfile::tempdir().unwrap();
	let client = build_client("preservation-test", Duration::from_secs(5), Vec::new()).unwrap();
	let original = "http://example.com/index.html";
	let body = "<html><body>Oldest surviving capture</body></html>";
	let mut records = String::new();
	for day in 1..=22 {
		let timestamp = format!("202608{day:02}000000");
		records.push_str(&format!("{timestamp} {original} text/html 200 SAME 100\n"));
		mock_capture(
			&server,
			&timestamp,
			original,
			if day == 1 { 200 } else { 404 },
			body,
		)
		.await;
	}
	mock_cdx(&server, "example.com", "host", &records).await;
	mock_cdx(&server, original, "exact", &records).await;
	let query = CdxQuery::new(
		"example.com".to_owned(),
		MatchType::Host,
		SnapshotStrategy::Latest,
		root,
	);
	// Parallel cases share five-second CDX pacing, so allow time for the whole fixture suite.
	let report = tokio::time::timeout(
		Duration::from_secs(600),
		download_site(
			client,
			SiteMapper::new("example.com").unwrap(),
			query,
			options(output.path(), None),
		),
	)
	.await
	.expect("alternate-capture recovery exceeded the shared pacing test budget")
	.unwrap();
	assert_eq!(report.downloaded, 1);
	assert_eq!(report.unavailable_snapshots, 0);
	assert_eq!(report.failed, 0);
	assert_eq!(
		std::fs::read_to_string(output.path().join("index.html")).unwrap(),
		body
	);
}

/// New and resumed runs must follow archived page and stylesheet dependencies recursively.
#[tokio::test]
async fn discovers_linked_pages_and_nested_assets_on_new_and_resumed_runs() {
	let server = MockServer::start().await;
	let root = Url::parse(&server.uri()).unwrap();
	let index = "<html><body><a href='forums/viewtopic.php?t=7&amp;p=9'>Topic</a><a href='https://unrelated.invalid/page'>External</a></body></html>";
	let topic = "<html><head><link rel=stylesheet href=//assets.example.com/site.css></head><body><a href='../index.html'>Home</a><a href='next'>Next</a><img srcset='//assets.example.com/a.gif 1x, //assets.example.com/b.gif 2x'></body></html>";
	let captures = [
		("http://example.com/index.html", "text/html", index),
		(
			"http://example.com/forums/viewtopic.php?p=9&t=7",
			"text/html",
			topic,
		),
		(
			"http://example.com/forums/next",
			"text/html",
			"<html><body><a href='viewtopic.php?p=9&amp;t=7'>Back</a></body></html>",
		),
		(
			"http://assets.example.com/site.css",
			"text/css",
			"@import 'theme.css'; body { background: url(nested.gif); }",
		),
		(
			"http://assets.example.com/theme.css",
			"text/css",
			"@font-face { font-family: archive; src: url(font.woff2); }",
		),
		(
			"http://assets.example.com/font.woff2",
			"font/woff2",
			"mock font",
		),
		("http://assets.example.com/a.gif", "image/gif", "mock a"),
		("http://assets.example.com/b.gif", "image/gif", "mock b"),
		(
			"http://assets.example.com/nested.gif",
			"image/gif",
			"mock nested",
		),
	];
	mock_cdx(
		&server,
		"example.com",
		"host",
		"20260911000000 http://example.com/index.html text/html 200 INDEX 100\n",
	)
	.await;
	for (original, mime, body) in captures {
		mock_cdx(
			&server,
			original,
			"exact",
			&format!("20260911000000 {original} {mime} 200 CONTENT 100\n"),
		)
		.await;
		mock_capture(&server, "20260911000000", original, 200, body).await;
	}
	Mock::given(path("/cdx/search/cdx"))
		.and(query_param(
			"url",
			"http://example.com/forums/viewtopic.php?t=7&amp;p=9",
		))
		.respond_with(ResponseTemplate::new(200).set_body_string(
			"20260911000000 http://example.com/forums/viewtopic.php?p=9&t=7 text/html 200 CONTENT 100\n",
		))
		.mount(&server)
		.await;
	for resume in [false, true] {
		let output = tempfile::tempdir().unwrap();
		if resume {
			std::fs::write(
				output.path().join("index.html"),
				"existing local page must not be overwritten",
			)
			.unwrap();
		}
		let client = build_client("preservation-test", Duration::from_secs(5), Vec::new()).unwrap();
		let query = CdxQuery::new(
			"example.com".to_owned(),
			MatchType::Host,
			SnapshotStrategy::Latest,
			root.clone(),
		);
		let report = download_site(
			client,
			SiteMapper::new("example.com").unwrap(),
			query,
			options(output.path(), Some(u64::MAX)),
		)
		.await
		.unwrap();
		assert_eq!(report.extra_downloads, 8);
		assert_eq!(report.failed, 0);
		assert_eq!(report.missing_local_links, 0);
		assert_eq!(
			std::fs::read_to_string(output.path().join("_hosts/assets.example.com/nested.gif"))
				.unwrap(),
			"mock nested"
		);
		assert!(output.path().join("forums/next/index.html").exists());
		assert_eq!(
			std::fs::read(output.path().join("_hosts/assets.example.com/font.woff2")).unwrap(),
			b"mock font"
		);
		if resume {
			assert_eq!(
				std::fs::read_to_string(output.path().join("index.html")).unwrap(),
				"existing local page must not be overwritten"
			);
		}
	}
	for request in server.received_requests().await.unwrap() {
		assert!(
			!request
				.url
				.query()
				.unwrap_or_default()
				.contains("unrelated.invalid")
		);
	}
}
