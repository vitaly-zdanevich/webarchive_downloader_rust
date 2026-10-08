//! Regression coverage for archived text decoding and raw script/style rewriting.

use std::collections::HashMap;
use std::process::{Command, Output};

use webarchive_downloader_rust::rewrite::{RewriteContext, rewrite_html};
use wiremock::matchers::{path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Downloads a single mocked capture through the real CLI, without external traffic.
async fn run_text_capture(
	body: &[u8],
	headers: &[(&str, &str)],
	css: bool,
	rewrite: bool,
) -> (tempfile::TempDir, Output, &'static str) {
	let server = MockServer::start().await;
	let output = tempfile::tempdir().unwrap();
	let (name, mime) = if css {
		("style.css", "text/css")
	} else {
		("page.html", "text/html")
	};
	Mock::given(path("/cdx/search/cdx"))
		.and(query_param("url", "example.com"))
		.respond_with(ResponseTemplate::new(200).set_body_string(format!(
			"20010709070258 http://example.com/{name} {mime} 200 TEXT 100\n"
		)))
		.expect(1)
		.mount(&server)
		.await;
	let mut response = ResponseTemplate::new(200).set_body_bytes(body.to_vec());
	for (name, value) in headers {
		response = response.insert_header(*name, *value);
	}
	Mock::given(path(format!(
		"/web/20010709070258id_/http://example.com/{name}"
	)))
	.respond_with(response)
	.expect(1)
	.mount(&server)
	.await;
	let mut command = Command::new(env!("CARGO_BIN_EXE_webarchive-downloader-rust"));
	command
		.args(["example.com", "--archive-root", &server.uri(), "--output"])
		.arg(output.path())
		.args([
			"--max-extra-download-size-mib",
			"0",
			"--timeout-seconds",
			"2",
		]);
	if !rewrite {
		command.arg("--no-rewrite");
	}
	let result = tokio::task::spawn_blocking(move || command.output().unwrap())
		.await
		.unwrap();
	(output, result, name)
}

/// Reads a successful CLI capture while retaining diagnostics on failure.
async fn download_text(body: &[u8], headers: &[(&str, &str)], css: bool, rewrite: bool) -> Vec<u8> {
	let (output, result, name) = run_text_capture(body, headers, css, rewrite).await;
	assert!(
		result.status.success(),
		"{}\n{}",
		String::from_utf8_lossy(&result.stdout),
		String::from_utf8_lossy(&result.stderr)
	);
	std::fs::read(output.path().join(name)).unwrap()
}

/// HTML raw-text elements must retain operators, markup strings, and comment delimiters.
#[test]
fn scripts_and_styles_preserve_raw_text_while_rewriting_urls() {
	let paths = HashMap::new();
	let context = RewriteContext::new(
		"http://example.com/pages/page.html",
		"pages/page.html".into(),
		&paths,
	)
	.unwrap();
	let html = r#"<script><!--
if (items > 0 && count < 2) { document.write('<b>A & B</b>'); }
preload.src = '/img/logo.gif';
//--></script><style><!--
p > a { background: url('/img/logo.gif'); }
a::before { content: '<>&'; }
--></style>"#;
	let expected = html.replace("/img/logo.gif", "../img/logo.gif");
	assert_eq!(rewrite_html(html, &context).unwrap(), expected);
}

/// Windows-1252 punctuation without a declaration must not become replacement characters.
#[tokio::test]
async fn undeclared_legacy_html_preserves_punctuation() {
	let saved = download_text(b"<p>\x91Small Rockets\x92 \xa9 2001</p>", &[], false, true).await;
	let text = String::from_utf8(saved).unwrap();
	assert!(
		text.contains("\u{2018}Small Rockets\u{2019} \u{a9} 2001"),
		"{text}"
	);
	assert!(
		text.starts_with('\u{feff}'),
		"non-ASCII output needs a local UTF-8 signature"
	);
}

/// Actual HTML metadata must select the decoder and then describe the UTF-8 output.
#[tokio::test]
async fn html_meta_charset_preserves_cyrillic() {
	let saved = download_text(b"<html><head><meta charset='windows-1251'></head><body>\xcf\xf0\xe8\xe2\xe5\xf2</body></html>", &[], false, true).await;
	let text = String::from_utf8(saved).unwrap();
	assert!(
		text.contains("\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}"),
		"{text}"
	);
	assert!(text.contains("charset=\"utf-8\""), "{text}");
	assert!(!text.contains("windows-1251"), "{text}");
}

/// Original HTTP charset metadata overrides conflicting in-document declarations.
#[tokio::test]
async fn original_http_charset_takes_precedence() {
	let saved = download_text(
		b"<meta charset='windows-1252'><p>\xcf\xf0\xe8\xe2\xe5\xf2</p>",
		&[
			("content-type", "text/html; charset=UTF-8"),
			(
				"x-archive-orig-content-type",
				"text/html; charset=windows-1251",
			),
		],
		false,
		true,
	)
	.await;
	assert!(
		String::from_utf8(saved)
			.unwrap()
			.contains("\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}")
	);
}

/// A byte-order mark wins over HTTP and HTML declarations.
#[tokio::test]
async fn utf16_bom_overrides_declared_charset() {
	let mut bytes = vec![0xff, 0xfe];
	for word in "<p>\u{65e5}\u{672c}</p>".encode_utf16() {
		bytes.extend(word.to_le_bytes());
	}
	let saved = download_text(
		&bytes,
		&[("content-type", "text/html; charset=windows-1252")],
		false,
		true,
	)
	.await;
	assert!(
		String::from_utf8(saved)
			.unwrap()
			.contains("<p>\u{65e5}\u{672c}</p>")
	);
}

/// CSS encoding declarations must be decoded and updated along with their contents.
#[tokio::test]
async fn css_charset_preserves_non_ascii_content() {
	let saved = download_text(
		b"@charset \"windows-1252\"; p::before { content: '\x93hello\x94'; }",
		&[],
		true,
		true,
	)
	.await;
	let text = String::from_utf8(saved).unwrap();
	assert!(text.contains("@charset \"UTF-8\";"), "{text}");
	assert!(text.contains("\u{201c}hello\u{201d}"), "{text}");
}

/// Already-UTF-8 text must not be decoded as a legacy code page by default.
#[tokio::test]
async fn undeclared_utf8_remains_utf8() {
	let text = "<p>\u{65e5}\u{672c} \u{1f680}</p>";
	let saved = download_text(text.as_bytes(), &[], false, true).await;
	assert!(String::from_utf8(saved).unwrap().contains(text));
}

/// Disabling rewriting preserves the archived bytes and charset declaration exactly.
#[tokio::test]
async fn no_rewrite_preserves_original_legacy_bytes() {
	let bytes = b"<meta charset='windows-1252'><p>\x91Small Rockets\x92</p>";
	assert_eq!(download_text(bytes, &[], false, false).await, bytes);
}

/// Separate nodes and an unclosed final script must not lose or combine buffered text.
#[test]
fn raw_text_buffers_flush_at_node_boundaries_and_eof() {
	let paths = HashMap::new();
	let context =
		RewriteContext::new("http://example.com/page.html", "page.html".into(), &paths).unwrap();
	let html = "<script>if (a < b) f('&amp;');</script><script></script><style>a > b { color: red; }</style><script>const literal = '<i>text</i>';";
	assert_eq!(rewrite_html(html, &context).unwrap(), html);
}

/// Charset conversion, metadata rewriting, and raw-text rewriting must work together.
#[tokio::test]
async fn legacy_document_keeps_scripts_and_http_equiv_metadata() {
	let bytes = b"<head><meta http-equiv='Content-Type' content='text/html; charset=iso-8859-1'></head><p>\x91Small Rockets\x92</p><script>if (count > 0 && count < 2) document.write('<b>A & B</b>');</script>";
	let saved = String::from_utf8(download_text(bytes, &[], false, true).await).unwrap();
	assert!(saved.contains("text/html; charset=utf-8"), "{saved}");
	assert!(saved.contains("\u{2018}Small Rockets\u{2019}"), "{saved}");
	assert!(
		saved.contains(
			"<script>if (count > 0 && count < 2) document.write('<b>A & B</b>');</script>"
		),
		"{saved}"
	);
}

/// Declared malformed text is reported instead of being saved with replacement characters.
#[tokio::test]
async fn malformed_declared_text_is_not_saved_lossily() {
	let bytes = b"<p>\xff</p>";
	let headers = [("content-type", "text/html; charset=utf-8")];
	let (output, result, name) = run_text_capture(bytes, &headers, false, true).await;
	assert!(String::from_utf8_lossy(&result.stdout).contains("  failed: 1"));
	assert!(!output.path().join(name).exists());
	assert!(String::from_utf8_lossy(&result.stderr).contains("refusing a lossy rewrite"));
	assert_eq!(download_text(bytes, &headers, false, false).await, bytes);
}

/// A non-UTF-8 stylesheet remains byte-for-byte original when rewriting is disabled.
#[tokio::test]
async fn no_rewrite_preserves_original_css_bytes() {
	let bytes = b"@charset \"windows-1252\"; p::before { content: '\x93hello\x94'; }";
	assert_eq!(download_text(bytes, &[], true, false).await, bytes);
}

/// An alternate replay must carry its own encoding rather than the failed capture's metadata.
#[tokio::test]
async fn alternate_capture_keeps_its_transport_charset() {
	let server = MockServer::start().await;
	let output = tempfile::tempdir().unwrap();
	let original = "http://example.com/page.html";
	let newest = format!("20020101000000 {original} text/html 200 NEW 100\n");
	let older = format!("20010101000000 {original} text/html 200 OLD 100\n");
	for (url, records) in [
		("example.com", newest.clone()),
		(original, format!("{newest}{older}")),
	] {
		Mock::given(path("/cdx/search/cdx"))
			.and(query_param("url", url))
			.respond_with(ResponseTemplate::new(200).set_body_string(records))
			.mount(&server)
			.await;
	}
	Mock::given(path("/web/20020101000000id_/http://example.com/page.html"))
		.respond_with(
			ResponseTemplate::new(404).insert_header("content-type", "text/html; charset=UTF-8"),
		)
		.expect(1)
		.mount(&server)
		.await;
	Mock::given(path("/web/20010101000000id_/http://example.com/page.html"))
		.respond_with(
			ResponseTemplate::new(200)
				.set_body_bytes(b"<p>example.com \xcf\xf0\xe8\xe2\xe5\xf2</p>".to_vec())
				.insert_header("content-type", "text/html; charset=windows-1251"),
		)
		.expect(1)
		.mount(&server)
		.await;
	let mut command = Command::new(env!("CARGO_BIN_EXE_webarchive-downloader-rust"));
	command
		.args(["example.com", "--archive-root", &server.uri(), "--output"])
		.arg(output.path())
		.args([
			"--max-extra-download-size-mib",
			"0",
			"--timeout-seconds",
			"2",
		]);
	let result = tokio::task::spawn_blocking(move || command.output().unwrap())
		.await
		.unwrap();
	assert!(
		result.status.success(),
		"{}",
		String::from_utf8_lossy(&result.stderr)
	);
	let saved = std::fs::read_to_string(output.path().join("page.html")).unwrap();
	assert!(
		saved.contains("\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}"),
		"{saved}"
	);
}
