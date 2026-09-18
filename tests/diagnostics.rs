//! Checks retry routing and completion diagnostics against mock archive responses.

use std::path::Path;
use std::process::{Command, Output};
use std::sync::{
	Arc,
	atomic::{AtomicUsize, Ordering},
};

use wiremock::matchers::{path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// A saved forum error remains a preservation defect even when all files exist.
#[test]
fn strict_validation_reports_unusable_html_without_network() {
	let output = tempfile::tempdir().unwrap();
	std::fs::write(output.path().join("topic.html"), "<table><tr><td align='center'><span class='gen'>The topic or post you requested does not exist</span></td></tr></table>").unwrap();
	let result = Command::new(env!("CARGO_BIN_EXE_webarchive-downloader-rust"))
		.args(["--validate-only", "--strict-validate-links", "--output"]).arg(output.path())
		.args(["--archive-root", "http://127.0.0.1:1"]).output().unwrap();
	assert_eq!(result.status.code(), Some(2));
	let stdout = String::from_utf8_lossy(&result.stdout);
	assert!(stdout.contains("unusable HTML files: 1"), "{stdout}");
	assert!(stdout.contains("topic.html"), "{stdout}");
}

/// Runs the real CLI with isolated output and optional fake SSH on PATH.
async fn run_cli(server: &MockServer, output: &Path, extra: &[&str], ssh: Option<&Path>) -> Output {
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
	if let Some(directory) = ssh {
		let mut paths = vec![directory.to_owned()];
		paths.extend(std::env::split_paths(
			&std::env::var_os("PATH").unwrap_or_default(),
		));
		command
			.env("PATH", std::env::join_paths(paths).unwrap())
			.args(["--ssh", "mock@unused.invalid"]);
	}
	tokio::task::spawn_blocking(move || command.output().unwrap())
		.await
		.unwrap()
}

/// Returns controlled failures followed by a successful response body.
async fn sequence(server: &MockServer, endpoint: &str, failures: Vec<u16>, body: String) {
	let count = failures.len() + 1;
	let attempts = Arc::new(AtomicUsize::new(0));
	Mock::given(path(endpoint))
		.respond_with(move |_: &Request| {
			let attempt = attempts.fetch_add(1, Ordering::SeqCst);
			ResponseTemplate::new(failures.get(attempt).copied().unwrap_or(200))
				.insert_header("Retry-After", "1")
				.set_body_string(body.clone())
		})
		.expect(count as u64)
		.mount(server)
		.await;
}

/// Installs a deterministic failing SSH executable without making connections.
#[cfg(unix)]
fn fake_ssh() -> tempfile::TempDir {
	use std::os::unix::fs::PermissionsExt;
	let directory = tempfile::tempdir().unwrap();
	let executable = directory.path().join("ssh");
	std::fs::copy(
		Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ssh-unavailable.sh"),
		&executable,
	)
	.unwrap();
	std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o755)).unwrap();
	directory
}

/// Short CDX outages should recover directly; persistent outages still try SSH.
#[cfg(unix)]
#[tokio::test]
async fn cdx_retries_server_errors_before_starting_ssh() {
	for (failures, ssh_attempts) in [
		(vec![503, 504], 0),
		(vec![503, 503, 503], 1),
		(vec![429], 1),
	] {
		let server = MockServer::start().await;
		let output = tempfile::tempdir().unwrap();
		let ssh = fake_ssh();
		sequence(&server, "/cdx/search/cdx", failures.clone(), String::new()).await;
		let result = run_cli(
			&server,
			output.path(),
			&["--no-rewrite", "--no-validate-links"],
			Some(ssh.path()),
		)
		.await;
		let stderr = String::from_utf8_lossy(&result.stderr);
		assert!(result.status.success(), "{stderr}");
		assert_eq!(
			stderr.matches("mock SSH startup failed").count(),
			ssh_attempts,
			"{failures:?}: {stderr}"
		);
	}
}

/// Buffered and streamed snapshots use the same grace period as CDX requests.
#[cfg(unix)]
#[tokio::test]
async fn snapshot_retries_server_errors_before_starting_ssh() {
	for (name, mime) in [("page.html", "text/html"), ("game.zip", "application/zip")] {
		let server = MockServer::start().await;
		let output = tempfile::tempdir().unwrap();
		let ssh = fake_ssh();
		sequence(
			&server,
			"/cdx/search/cdx",
			vec![],
			format!("20260911000000 http://example.com/{name} {mime} 200 DATA 100\n"),
		)
		.await;
		sequence(
			&server,
			&format!("/web/20260911000000id_/http://example.com/{name}"),
			vec![503, 504],
			"example archived content".to_owned(),
		)
		.await;
		let result = run_cli(
			&server,
			output.path(),
			&["--no-validate-links"],
			Some(ssh.path()),
		)
		.await;
		let stderr = String::from_utf8_lossy(&result.stderr);
		assert!(result.status.success(), "{stderr}");
		assert!(!stderr.contains("mock SSH startup failed"), "{stderr}");
		assert_eq!(
			std::fs::read_to_string(output.path().join(name)).unwrap(),
			"example archived content"
		);
	}
}

/// One failed SSH startup should announce its cooldown only once across requests.
#[cfg(unix)]
#[tokio::test]
async fn ssh_cooldown_does_not_repeat_for_each_snapshot_failure() {
	let server = MockServer::start().await;
	let output = tempfile::tempdir().unwrap();
	let ssh = fake_ssh();
	let mut records = String::new();
	for name in ["a.zip", "b.zip", "c.zip"] {
		records.push_str(&format!(
			"20260911000000 http://example.com/{name} application/zip 200 DATA 100\n"
		));
		Mock::given(path(format!(
			"/web/20260911000000id_/http://example.com/{name}"
		)))
		.respond_with(ResponseTemplate::new(403))
		.expect(1)
		.mount(&server)
		.await;
	}
	sequence(&server, "/cdx/search/cdx", vec![], records).await;
	let result = run_cli(
		&server,
		output.path(),
		&["--no-rewrite", "--no-validate-links"],
		Some(ssh.path()),
	)
	.await;
	let stderr = String::from_utf8_lossy(&result.stderr);
	assert_eq!(
		stderr.matches("mock SSH startup failed").count(),
		1,
		"{stderr}"
	);
	assert_eq!(stderr.matches("eligible again in").count(), 1, "{stderr}");
	assert!(!stderr.contains("is cooling down"), "{stderr}");
}

/// Preserved access-denied pages are counted separately from usable replacements.
#[tokio::test]
async fn completion_reports_retained_unusable_captures() {
	let server = MockServer::start().await;
	let output = tempfile::tempdir().unwrap();
	let blocked = "<html><body>You are not authorised to read this forum.</body></html>";
	let usable = "<html><body>example forum content</body></html>";
	let mut discovery = String::new();
	for name in ["blocked.html", "recovered.html"] {
		let original = format!("http://example.com/{name}");
		let record = format!("20260911000000 {original} text/html 200 BLOCKED 100\n");
		discovery.push_str(&record);
		let mut candidates = record;
		if name == "recovered.html" {
			candidates.push_str(&format!(
				"20260910000000 {original} text/html 200 USABLE 100\n"
			));
			sequence(
				&server,
				&format!("/web/20260910000000id_/{original}"),
				vec![],
				usable.to_owned(),
			)
			.await;
		}
		Mock::given(path("/cdx/search/cdx"))
			.and(query_param("url", &original))
			.respond_with(ResponseTemplate::new(200).set_body_string(candidates))
			.expect(1)
			.mount(&server)
			.await;
		sequence(
			&server,
			&format!("/web/20260911000000id_/{original}"),
			vec![],
			blocked.to_owned(),
		)
		.await;
	}
	Mock::given(path("/cdx/search/cdx"))
		.and(query_param("url", "example.com"))
		.respond_with(ResponseTemplate::new(200).set_body_string(discovery))
		.expect(1)
		.mount(&server)
		.await;
	let result = run_cli(&server, output.path(), &[], None).await;
	let stdout = String::from_utf8_lossy(&result.stdout);
	assert!(
		result.status.success(),
		"{}",
		String::from_utf8_lossy(&result.stderr)
	);
	assert!(stdout.contains("downloaded: 2"), "{stdout}");
	assert!(stdout.contains("retained unusable captures: 1"), "{stdout}");
	assert_eq!(
		std::fs::read_to_string(output.path().join("blocked.html")).unwrap(),
		blocked
	);
	assert_eq!(
		std::fs::read_to_string(output.path().join("recovered.html")).unwrap(),
		usable
	);
}

/// Validation counts one missing target even when multiple pages link to it.
#[test]
fn validation_reports_unique_missing_targets() {
	let output = tempfile::tempdir().unwrap();
	for name in ["a.html", "b.html"] {
		std::fs::write(output.path().join(name), "<img src='missing.gif'>").unwrap();
	}
	let result = Command::new(env!("CARGO_BIN_EXE_webarchive-downloader-rust"))
		.args(["--validate-only", "--strict-validate-links", "--output"])
		.arg(output.path())
		.output()
		.unwrap();
	let stdout = String::from_utf8_lossy(&result.stdout);
	assert_eq!(result.status.code(), Some(2));
	assert!(stdout.contains("missing 2"), "{stdout}");
	assert!(stdout.contains("unique missing targets 1"), "{stdout}");
}
