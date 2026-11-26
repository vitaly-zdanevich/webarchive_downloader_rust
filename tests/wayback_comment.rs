//! Verifies Wayback Machine source comments in rewritten HTML output and opt-out CLI flags.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use wiremock::matchers::{path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn run_cli(server: &MockServer, output: &Path, extra: &[&str]) -> Output {
	let mut command = Command::new(env!("CARGO_BIN_EXE_webarchive-downloader-rust"));
	command
		.args(["example.com", "--archive-root", &server.uri(), "--output"])
		.arg(output)
		.args([
			"--max-extra-download-size-mib",
			"0",
			"--timeout-seconds",
			"2",
		])
		.args(extra);
	tokio::task::spawn_blocking(move || command.output().unwrap())
		.await
		.unwrap()
}

fn assert_success(result: &Output) {
	assert!(
		result.status.success(),
		"{}\n{}",
		String::from_utf8_lossy(&result.stdout),
		String::from_utf8_lossy(&result.stderr),
	);
}

/// Default downloads insert a non-visible Wayback comment on rewritten HTML pages only,
/// while `--no-wayback-comment` disables it without disabling link rewriting and
/// `--no-rewrite` preserves original archived bytes untouched.
#[tokio::test]
async fn wayback_comment_default_and_opt_out_flags() {
	let server = MockServer::start().await;
	let timestamp = "20120805042739";
	let index_body = "<!DOCTYPE html>\n<html><head><link rel=\"stylesheet\" href=\"/style.css\"></head><body><a href=\"/about.html\">About</a><img src=\"/pixel.gif\"></body></html>";
	let about_body = "<p>About \u{2014} archived</p>";
	let css_body = "body { background: url(/pixel.gif); }";
	let untyped_css_body = "p { color: black; }";
	let binary_body = b"GIF89a";

	Mock::given(path("/cdx/search/cdx"))
		.and(query_param("url", "example.com"))
		.and(query_param("matchType", "domain"))
		.respond_with(ResponseTemplate::new(200).set_body_string(format!(
			concat!(
				"{ts} http://example.com/ text/html 200 INDEX 100\n",
				"{ts} http://example.com/about.html text/html 200 ABOUT 100\n",
				"{ts} http://example.com/style.css text/css 200 CSS 100\n",
				"{ts} http://example.com/legacy.css unk 200 UNKCSS 100\n",
				"{ts} http://example.com/pixel.gif image/gif 200 GIF 6\n",
			),
			ts = timestamp
		)))
		.mount(&server)
		.await;

	for (original, body) in [
		("http://example.com/", index_body.as_bytes()),
		("http://example.com/about.html", about_body.as_bytes()),
		("http://example.com/style.css", css_body.as_bytes()),
		("http://example.com/legacy.css", untyped_css_body.as_bytes()),
		("http://example.com/pixel.gif", binary_body.as_slice()),
	] {
		Mock::given(path(format!("/web/{timestamp}id_/{original}")))
			.respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
			.mount(&server)
			.await;
	}

	let default_out = tempfile::tempdir().unwrap();
	assert_success(&run_cli(&server, default_out.path(), &["--strict-validate-links"]).await);

	let saved_index = fs::read_to_string(default_out.path().join("index.html")).unwrap();
	assert_eq!(
		saved_index,
		format!(
			"<!DOCTYPE html>\n<!-- Archived from Wayback Machine: {}/web/{timestamp}/http://example.com/ -->\n<html><head><link rel=\"stylesheet\" href=\"style.css\"></head><body><a href=\"about.html\">About</a><img src=\"pixel.gif\"></body></html>",
			server.uri()
		)
	);
	assert!(!saved_index.starts_with('\u{feff}'), "ASCII HTML must not gain a BOM");

	let saved_about = fs::read_to_string(default_out.path().join("about.html")).unwrap();
	assert_eq!(
		saved_about,
		format!(
			"\u{feff}<!-- Archived from Wayback Machine: {}/web/{timestamp}/http://example.com/about.html -->\n{about_body}",
			server.uri()
		)
	);

	assert_eq!(
		fs::read_to_string(default_out.path().join("style.css")).unwrap(),
		"body { background: url(pixel.gif); }"
	);
	assert_eq!(
		fs::read_to_string(default_out.path().join("legacy.css")).unwrap(),
		untyped_css_body
	);
	assert_eq!(fs::read(default_out.path().join("pixel.gif")).unwrap(), binary_body);

	let no_comment_out = tempfile::tempdir().unwrap();
	assert_success(
		&run_cli(
			&server,
			no_comment_out.path(),
			&["--no-wayback-comment", "--strict-validate-links"],
		)
		.await,
	);
	assert_eq!(
		fs::read_to_string(no_comment_out.path().join("index.html")).unwrap(),
		"<!DOCTYPE html>\n<html><head><link rel=\"stylesheet\" href=\"style.css\"></head><body><a href=\"about.html\">About</a><img src=\"pixel.gif\"></body></html>"
	);
	assert_eq!(
		fs::read_to_string(no_comment_out.path().join("about.html")).unwrap(),
		format!("\u{feff}{about_body}")
	);

	let no_rewrite_out = tempfile::tempdir().unwrap();
	assert_success(
		&run_cli(
			&server,
			no_rewrite_out.path(),
			&["--no-rewrite", "--no-validate-links"],
		)
		.await,
	);
	assert_eq!(
		fs::read_to_string(no_rewrite_out.path().join("index.html")).unwrap(),
		index_body
	);
	assert_eq!(
		fs::read_to_string(no_rewrite_out.path().join("about.html")).unwrap(),
		about_body
	);
}

/// Strict validation must still flag retained unusable HTML files that contain a Wayback comment.
#[tokio::test]
async fn strict_validation_still_flags_unusable_html_with_wayback_comment() {
	let server = MockServer::start().await;
	let timestamp = "20120805042739";
	let original = "http://example.com/redirect.html";
	let redirect = "<!DOCTYPE html>\n<html><head><meta http-equiv=\"refresh\" content=\"0;url=/other.html\"></head><body></body></html>";
	let record = format!("{timestamp} {original} text/html 200 REDIRECT 100\n");

	Mock::given(path("/cdx/search/cdx"))
		.and(query_param("url", "example.com"))
		.respond_with(ResponseTemplate::new(200).set_body_string(record.clone()))
		.mount(&server)
		.await;
	Mock::given(path("/cdx/search/cdx"))
		.and(query_param("url", original))
		.respond_with(ResponseTemplate::new(200).set_body_string(record))
		.mount(&server)
		.await;
	Mock::given(path(format!("/web/{timestamp}id_/{original}")))
		.respond_with(ResponseTemplate::new(200).set_body_string(redirect))
		.mount(&server)
		.await;

	let output = tempfile::tempdir().unwrap();
	let result = run_cli(&server, output.path(), &["--strict-validate-links"]).await;
	assert_eq!(result.status.code(), Some(2));

	let saved = fs::read_to_string(output.path().join("redirect.html")).unwrap();
	assert!(
		saved.starts_with(&format!(
			"<!DOCTYPE html>\n<!-- Archived from Wayback Machine: {}/web/{timestamp}/{original} -->\n",
			server.uri()
		)),
		"{saved}"
	);
	let stdout = String::from_utf8_lossy(&result.stdout);
	assert!(stdout.contains("unusable HTML files: 1"), "{stdout}");
}
