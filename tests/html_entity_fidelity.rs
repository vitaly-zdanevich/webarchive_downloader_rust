//! HTML attribute entities must be decoded once, independently of CSS and JS raw text.

use std::collections::{HashMap, HashSet};
use std::process::Command;

use webarchive_downloader_rust::link_validation::validate_local_links;
use webarchive_downloader_rust::pathmap::SiteMapper;
use webarchive_downloader_rust::rewrite::{RewriteContext, rewrite_html};
use wiremock::matchers::{path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Every URL-bearing attribute must use its browser-decoded value and escape it on output.
#[test]
fn url_attributes_decode_entities_before_localization() {
	let paths = HashMap::new();
	let context = RewriteContext::new(
		"http://example.com/pages/page.html",
		"pages/page.html".into(),
		&paths,
	)
	.unwrap();
	for (input, expected) in [
		(
			r#"<img src="/img/a&#38;b.gif">"#,
			r#"<img src="../img/a&amp;b.gif">"#,
		),
		(
			r#"<source srcset="/img/a&#x26;b.gif 2x">"#,
			r#"<source srcset="../img/a&amp;b.gif 2x">"#,
		),
		(
			r#"<param value="/img/a&amp;b.gif">"#,
			r#"<param value="../img/a&amp;b.gif">"#,
		),
		(
			r#"<applet archive="/lib/a&#38;b.jar,/lib/c.jar">"#,
			r#"<applet archive="../lib/a&amp;b.jar, ../lib/c.jar">"#,
		),
		(
			r#"<meta http-equiv="refresh" content="0;url=/a&#38;b.htm">"#,
			r#"<meta http-equiv="refresh" content="0;url=../a&amp;b.htm">"#,
		),
		(
			r#"<i style="background:url('/img/a&amp;amp;name.gif')"></i>"#,
			r#"<i style="background:url('../img/a&amp;amp;name.gif')"></i>"#,
		),
		(
			r#"<img src="/img/literal&amp;copy;.gif">"#,
			r#"<img src="../img/literal&amp;copy;.gif">"#,
		),
	] {
		assert_eq!(rewrite_html(input, &context).unwrap(), expected, "{input}");
	}
}

/// Validation must resolve literal ampersands instead of looking for entity spellings on disk.
#[test]
fn validation_decodes_attributes_but_not_raw_script_or_stylesheet_text() {
	let root = tempfile::tempdir().unwrap();
	for name in ["a&b.gif", "literal&amp;name.gif", "generated&name.gif"] {
		std::fs::write(root.path().join(name), b"image").unwrap();
	}
	std::fs::write(root.path().join("index.html"), r#"<img src="a&amp;b.gif">
<source srcset="a&#38;b.gif 2x"><param value="a&#x26;b.gif">
<style>i { background: url('literal&amp;name.gif'); }</style>
<script>image.src = 'literal&amp;name.gif'; document.write('<img src="generated&amp;name.gif">');</script>"#).unwrap();
	let report = validate_local_links(root.path()).unwrap();
	assert!(report.missing.is_empty(), "{:?}", report.missing);
}

/// Fresh downloads and resumed scans must request exact decoded URLs, never entity spellings.
#[tokio::test]
async fn fresh_download_and_resume_preserve_entity_boundaries() {
	let server = MockServer::start().await;
	let root = tempfile::tempdir().unwrap();
	let timestamp = "20200101000000";
	Mock::given(path("/cdx/search/cdx"))
		.respond_with(ResponseTemplate::new(200))
		.with_priority(10)
		.mount(&server)
		.await;
	Mock::given(path("/cdx/search/cdx"))
		.and(query_param("url", "example.com"))
		.respond_with(ResponseTemplate::new(200).set_body_string(format!(
			"{timestamp} http://example.com/index.html text/html 200 PAGE 100\n"
		)))
		.expect(2)
		.mount(&server)
		.await;
	let page = r#"<img src="img/photo&#38;one.gif"><a href="details.php?a=1&#38;b=2">Details</a>
<script>image.src = 'img/literal&copy;.gif'; document.write('<a href="' + prefix + '/index.html">Link</a>');</script>"#;
	Mock::given(path(format!(
		"/web/{timestamp}id_/http://example.com/index.html"
	)))
	.respond_with(ResponseTemplate::new(200).set_body_string(page))
	.expect(1)
	.mount(&server)
	.await;
	let mapper = SiteMapper::new("example.com").unwrap();
	let mut destinations = Vec::new();
	let mut expected_lookups = HashSet::from(["example.com".to_owned()]);
	for (reference, mime, body) in [
		("img/photo&one.gif", "image/gif", "image one"),
		("img/literal&copy;.gif", "image/gif", "image two"),
		(
			"details.php?a=1&b=2",
			"text/html",
			"<p>Preserved details</p>",
		),
	] {
		let original = format!("http://example.com/{reference}");
		expected_lookups.insert(original.clone());
		Mock::given(path("/cdx/search/cdx"))
			.and(query_param("url", &original))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_string(format!("{timestamp} {original} {mime} 200 RESOURCE 100\n")),
			)
			.expect(2)
			.mount(&server)
			.await;
		let replay =
			url::Url::parse(&format!("{}/web/{timestamp}id_/{original}", server.uri())).unwrap();
		let mut mock = Mock::given(path(replay.path()));
		for (key, value) in replay.query_pairs() {
			mock = mock.and(query_param(key, value));
		}
		mock.respond_with(ResponseTemplate::new(200).set_body_string(body))
			.expect(1)
			.mount(&server)
			.await;
		destinations.push(mapper.local_path_for_url(&original, mime).unwrap());
	}
	for _ in 0..2 {
		let mut command = Command::new(env!("CARGO_BIN_EXE_webarchive-downloader-rust"));
		command
			.args(["example.com", "--archive-root", &server.uri(), "--output"])
			.arg(root.path())
			.args(["--timeout-seconds", "2", "--strict-validate-links"]);
		let result = tokio::task::spawn_blocking(move || command.output().unwrap())
			.await
			.unwrap();
		assert!(
			result.status.success(),
			"{}\n{}",
			String::from_utf8_lossy(&result.stdout),
			String::from_utf8_lossy(&result.stderr)
		);
	}
	for destination in destinations {
		assert!(root.path().join(destination).is_file());
	}
	let requests = server.received_requests().await.unwrap();
	assert!(
		requests.iter().all(|request| request
			.url
			.query_pairs()
			.filter(|(key, _)| key == "url")
			.all(|(_, value)| expected_lookups.contains(value.as_ref()))),
		"unexpected lookup: {requests:?}"
	);
}
