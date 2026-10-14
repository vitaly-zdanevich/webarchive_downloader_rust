//! Checks JavaScript rewriting and dependency discovery without site-specific fixtures.

use std::collections::HashMap;
use std::process::Command;

use webarchive_downloader_rust::link_validation::validate_local_links;
use webarchive_downloader_rust::rewrite::{RewriteContext, rewrite_html};
use wiremock::matchers::{path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Rewrites a nested document using predictable local resource paths.
fn rewrite(input: &str) -> String {
	let paths = HashMap::new();
	let context = RewriteContext::new(
		"http://example.com/pages/page.html",
		"pages/page.html".into(),
		&paths,
	)
	.unwrap();
	rewrite_html(input, &context).unwrap()
}

/// Escaped double quotes must be decoded before HTML parsing and escaped on insertion.
#[test]
fn double_quoted_html_string_retains_valid_quotes() {
	let input =
		r#"<script>document.write("<IMG SRC=\"/img/spacer.gif\" WIDTH=1 HEIGHT=5>");</script>"#;
	assert_eq!(
		rewrite(input),
		input.replace("/img/spacer.gif", "../img/spacer.gif")
	);
}

/// HTML parsing can introduce double quotes inside a formerly unquoted attribute.
#[test]
fn unquoted_fragment_attributes_do_not_close_the_javascript_string() {
	assert_eq!(
		rewrite(r#"<script>document.write("<img src=/img/spacer.gif>");</script>"#),
		r#"<script>document.write("<img src=\"../img/spacer.gif\">");</script>"#,
	);
}

/// Browser-decoded handler values must be rewritten and entity-encoded only once.
#[test]
fn handler_entities_and_escaped_quotes_are_preserved() {
	let input = r#"<a onclick="document.write(&quot;<img src=\&quot;/img/spacer.gif\&quot; alt=\&quot;A &amp;amp; B\&quot;>&quot;); return false">Open</a>"#;
	let expected = input
		.replace("/img/spacer.gif", "../img/spacer.gif")
		.replace("<img", "&lt;img")
		.replace(">&quot;", "&gt;&quot;");
	assert_eq!(rewrite(input), expected);
}

/// Hexadecimal, Unicode and escaped slash references use their runtime string values.
#[test]
fn escaped_url_literals_are_localized() {
	for reference in [
		r"\/img\/logo.gif",
		r"\x2fimg/logo.gif",
		r"\u002fimg/logo.gif",
		r"\057img/logo.gif",
	] {
		assert_eq!(
			rewrite(&format!("<script>image.src = '{reference}';</script>")),
			"<script>image.src = '../img/logo.gif';</script>",
		);
	}
}

/// Non-code text must not be treated as quoted JavaScript URL references.
#[test]
fn comments_regular_expressions_and_template_text_stay_unchanged() {
	let input = r#"<script>// '/img/comment.gif'
/* "<img src=/img/comment.gif>" */
const pattern = /'\/img\/regex.gif'/;
const template = `"/img/template.gif" ${image.src = '/img/live.gif'}`;
</script>"#;
	assert_eq!(
		rewrite(input),
		input.replace("/img/live.gif", "../img/live.gif")
	);
}

/// Invalid archived code and non-JavaScript data blocks must not be guessed at.
#[test]
fn invalid_scripts_and_data_blocks_stay_unchanged() {
	for input in [
		r#"<script>var = '/img/logo.gif';</script>"#,
		r#"<script type="application/ld+json">{"image":"/img/logo.gif"}</script>"#,
		r#"<script type="text/template"><img src="/img/logo.gif"></script>"#,
	] {
		assert_eq!(rewrite(input), input);
	}
}

/// Escaped end tags and line breaks must not become HTML terminators or literal JS newlines.
#[test]
fn rewritten_html_keeps_script_terminators_and_line_breaks_safe() {
	let input = r#"<script>document.write("<img src=\"/img/logo.gif\">\n<script>void 0;<\/script>");</script>"#;
	let output = rewrite(input);
	assert!(output.contains(r#"src=\"../img/logo.gif\""#), "{output}");
	assert!(output.contains(r"\n"), "{output}");
	assert_eq!(output.matches("</script>").count(), 1, "{output}");
}

/// Validation and discovery must inspect decoded markup rather than bogus quoted paths.
#[test]
fn validation_discovers_escaped_fragment_assets_but_not_comments() {
	let root = tempfile::tempdir().unwrap();
	std::fs::write(
		root.path().join("page.html"),
		r#"<script>// '/img/comment.gif'
document.write("<img src=\"img/spacer.gif\">");
image.src = '\x69mg/logo.gif';</script>"#,
	)
	.unwrap();
	let report = validate_local_links(root.path()).unwrap();
	let mut references = report
		.missing
		.into_iter()
		.map(|link| link.href)
		.collect::<Vec<_>>();
	references.sort();
	assert_eq!(references, vec!["img/logo.gif", "img/spacer.gif"]);
}

/// A fresh CLI download must fetch dependencies embedded in escaped HTML and URL strings.
#[tokio::test]
async fn fresh_download_fetches_decoded_script_dependencies() {
	let server = MockServer::start().await;
	let root = tempfile::tempdir().unwrap();
	let timestamp = "20200101000000";
	let page = r#"<script>// '/img/comment.gif'
document.write("<img src=\"/img/spacer.gif\">");
image.src = '\x2fimg/logo.gif';</script>
<button onclick="document.write(&quot;<img src=\&quot;/img/handler.gif\&quot;>&quot;)">Open</button>"#;
	Mock::given(path("/cdx/search/cdx"))
		.and(query_param("url", "example.com"))
		.respond_with(ResponseTemplate::new(200).set_body_string(format!(
			"{timestamp} http://example.com/index.html text/html 200 PAGE 100\n"
		)))
		.expect(1)
		.mount(&server)
		.await;
	Mock::given(path(format!(
		"/web/{timestamp}id_/http://example.com/index.html"
	)))
	.respond_with(ResponseTemplate::new(200).set_body_string(page))
	.expect(1)
	.mount(&server)
	.await;
	for name in ["spacer", "logo", "handler"] {
		let original = format!("http://example.com/img/{name}.gif");
		Mock::given(path("/cdx/search/cdx"))
			.and(query_param("url", &original))
			.respond_with(
				ResponseTemplate::new(200)
					.set_body_string(format!("{timestamp} {original} image/gif 200 {name} 10\n")),
			)
			.expect(1)
			.mount(&server)
			.await;
		Mock::given(path(format!("/web/{timestamp}id_/{original}")))
			.respond_with(ResponseTemplate::new(200).set_body_bytes([0, 1, 2, 255]))
			.expect(1)
			.mount(&server)
			.await;
	}
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
	for name in ["spacer", "logo", "handler"] {
		assert_eq!(
			std::fs::read(root.path().join(format!("img/{name}.gif"))).unwrap(),
			[0, 1, 2, 255]
		);
	}
	let saved = std::fs::read_to_string(root.path().join("index.html")).unwrap();
	assert!(saved.contains(r#"src=\"img/spacer.gif\""#), "{saved}");
	assert!(saved.contains("image.src = 'img/logo.gif'"), "{saved}");
	assert!(
		saved.contains(r"src=\&quot;img/handler.gif\&quot;"),
		"{saved}"
	);
	assert!(
		validate_local_links(root.path())
			.unwrap()
			.missing
			.is_empty()
	);
	let events =
		std::fs::read_to_string(root.path().join(".wayback-state/captures.jsonl")).unwrap();
	assert!(
		!events.contains("comment.gif"),
		"comments must not cause archive requests"
	);
}
