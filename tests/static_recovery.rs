//! Checks static-asset recovery and its final CLI summary against a mock archive.

use std::path::Path;
use std::process::{Command, Output};

use wiremock::matchers::{path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TIMESTAMP: &str = "20020401084637";
const HOST: &str = "corporate.example.com";
const LOCAL_DIR: &str = "_hosts/corporate.example.com/shared";

/// Runs the real CLI with static recovery enabled and no external requests.
async fn run_cli(server: &MockServer, output: &Path, arguments: &[&str]) -> Output {
	let mut command = Command::new(env!("CARGO_BIN_EXE_webarchive-downloader-rust"));
	command
		.args(["example.com", "--archive-root", &server.uri(), "--output"])
		.arg(output)
		.args(["--timeout-seconds", "2"])
		.args(arguments);
	tokio::task::spawn_blocking(move || command.output().unwrap())
		.await
		.unwrap()
}

/// Supplies a CDX response for either domain discovery or an exact asset lookup.
async fn mock_cdx(server: &MockServer, target: &str, scope: &str, status: u16, body: &str) {
	Mock::given(path("/cdx/search/cdx"))
		.and(query_param("url", target))
		.and(query_param("matchType", scope))
		.respond_with(ResponseTemplate::new(status).set_body_string(body))
		.expect(1)
		.mount(server)
		.await;
}

/// Describes one indexed image whose replay may no longer be available.
fn asset_record(name: &str) -> String {
	format!("{TIMESTAMP} http://{HOST}/shared/{name} image/gif 200 {name} 6\n")
}

/// Permanent replay failures must not prevent later recovery, validation, or summaries.
async fn check_completion(repair: bool, strict: bool) {
	let server = MockServer::start().await;
	let output = tempfile::tempdir().unwrap();
	let mut html = String::new();
	let mut records = String::new();
	for (name, status) in [
		("hd_products.gif", 404),
		("missing.gif", 410),
		("restricted.gif", 451),
		("z-good.gif", 200),
	] {
		html.push_str(&format!("<img src='{LOCAL_DIR}/{name}'>"));
		let record = asset_record(name);
		records.push_str(&record);
		// Repair uses the discovery index directly for the successful asset.
		if !repair || status != 200 {
			mock_cdx(
				&server,
				&format!("http://{HOST}/shared/{name}"),
				"exact",
				200,
				&record,
			)
			.await;
		}
		Mock::given(path(format!(
			"/web/{TIMESTAMP}id_/http://{HOST}/shared/{name}"
		)))
		.respond_with(ResponseTemplate::new(status).set_body_string("GIF89a"))
		.expect(1)
		.mount(&server)
		.await;
	}
	// An existing page can reference assets absent from the initial discovery results.
	mock_cdx(
		&server,
		"example.com",
		"domain",
		200,
		if repair { &records } else { "" },
	)
	.await;
	std::fs::write(output.path().join("index.html"), &html).unwrap();
	let mut arguments = Vec::new();
	if repair {
		arguments.push("--repair-output");
	}
	if strict {
		arguments.push("--strict-validate-links");
	}
	let result = run_cli(&server, output.path(), &arguments).await;
	let stdout = String::from_utf8_lossy(&result.stdout);
	let stderr = String::from_utf8_lossy(&result.stderr);
	assert_eq!(
		result.status.code(),
		Some(if strict { 2 } else { 0 }),
		"{stdout}\n{stderr}"
	);
	for line in [
		if repair { "repair done:" } else { "done:" },
		"  recovered static assets: 1",
		"  unavailable static assets: 3",
		"  missing links: 3",
		"  unique missing targets: 3",
		"output size:",
		"10 biggest files:",
		"elapsed time:",
		"Finished at ",
	] {
		assert!(stdout.contains(line), "missing {line:?}: {stdout}");
	}
	assert_eq!(
		stderr.matches("static asset snapshot unavailable:").count(),
		3,
		"{stderr}"
	);
	assert!(!stderr.contains("Error:"), "{stderr}");
	assert_eq!(
		std::fs::read_to_string(output.path().join("index.html")).unwrap(),
		html
	);
	assert_eq!(
		std::fs::read(output.path().join(LOCAL_DIR).join("z-good.gif")).unwrap(),
		b"GIF89a"
	);
	for name in ["hd_products.gif", "missing.gif", "restricted.gif"] {
		assert!(!output.path().join(LOCAL_DIR).join(name).exists());
	}
}

/// Ordinary downloads finish even when final static-asset lookups find missing replays.
#[tokio::test]
async fn download_completes_after_unavailable_static_snapshots() {
	check_completion(false, false).await;
}

/// Repair also continues past indexed assets whose replay has disappeared.
#[tokio::test]
async fn repair_completes_after_unavailable_static_snapshots() {
	check_completion(true, false).await;
}

/// Continuing recovery must not hide missing assets from strict validation.
#[tokio::test]
async fn strict_validation_still_fails_after_static_recovery() {
	check_completion(false, true).await;
	check_completion(true, true).await;
}

/// A CDX 404 during alternate lookup is a lookup failure, not proof of missing content.
#[tokio::test]
async fn static_recovery_propagates_failed_alternate_lookup() {
	let server = MockServer::start().await;
	let output = tempfile::tempdir().unwrap();
	let name = "hd_products.gif";
	let html = format!("<img src='{LOCAL_DIR}/{name}'>");
	std::fs::write(output.path().join("index.html"), &html).unwrap();
	mock_cdx(&server, "example.com", "domain", 200, &asset_record(name)).await;
	mock_cdx(
		&server,
		&format!("http://{HOST}/shared/{name}"),
		"exact",
		404,
		"CDX unavailable",
	)
	.await;
	Mock::given(path(format!(
		"/web/{TIMESTAMP}id_/http://{HOST}/shared/{name}"
	)))
	.respond_with(ResponseTemplate::new(404))
	.expect(1)
	.mount(&server)
	.await;
	let result = run_cli(&server, output.path(), &["--repair-output"]).await;
	let stdout = String::from_utf8_lossy(&result.stdout);
	let stderr = String::from_utf8_lossy(&result.stderr);
	assert_eq!(result.status.code(), Some(1), "{stdout}\n{stderr}");
	assert!(stderr.contains("Error: repair failed:"), "{stderr}");
	assert!(
		!stderr.contains("static asset snapshot unavailable:"),
		"{stderr}"
	);
	assert!(!stdout.contains("repair done:"), "{stdout}");
	assert_eq!(
		std::fs::read_to_string(output.path().join("index.html")).unwrap(),
		html
	);
}

/// Missing alias source replays must leave the original reference intact and finish repair.
#[tokio::test]
async fn repair_completes_after_unavailable_static_alias_source() {
	let server = MockServer::start().await;
	let output = tempfile::tempdir().unwrap();
	let html = format!("<img src='{LOCAL_DIR}/screen1.gif'>");
	std::fs::write(output.path().join("index.html"), &html).unwrap();
	let mut records =
		format!("{TIMESTAMP} http://example.com/index.html text/html 200 CURRENT 100\n");
	for name in ["screen1.gif", "screenshot1.gif"] {
		let record = asset_record(name);
		records.push_str(&record);
		mock_cdx(
			&server,
			&format!("http://{HOST}/shared/{name}"),
			"exact",
			200,
			&record,
		)
		.await;
		Mock::given(path(format!(
			"/web/{TIMESTAMP}id_/http://{HOST}/shared/{name}"
		)))
		.respond_with(ResponseTemplate::new(404))
		.expect(1)
		.mount(&server)
		.await;
	}
	mock_cdx(&server, "example.com", "domain", 200, &records).await;
	mock_cdx(
		&server,
		"http://example.com/index.html",
		"exact",
		200,
		"20010401000000 http://example.com/index.html text/html 200 OLDER 100\n",
	)
	.await;
	Mock::given(path("/web/20010401000000id_/http://example.com/index.html"))
		.respond_with(
			ResponseTemplate::new(200)
				.set_body_string(format!("<img src='{LOCAL_DIR}/screenshot1.gif'>")),
		)
		.expect(1)
		.mount(&server)
		.await;
	let result = run_cli(&server, output.path(), &["--repair-output"]).await;
	let stdout = String::from_utf8_lossy(&result.stdout);
	let stderr = String::from_utf8_lossy(&result.stderr);
	assert!(result.status.success(), "{stdout}\n{stderr}");
	for line in [
		"repair done:",
		"  static asset aliases: 0",
		"  unavailable static assets: 1",
		"  missing links: 1",
		"output size:",
	] {
		assert!(stdout.contains(line), "missing {line:?}: {stdout}");
	}
	assert!(
		stderr.contains("static asset alias source unavailable:"),
		"{stderr}"
	);
	assert_eq!(
		std::fs::read_to_string(output.path().join("index.html")).unwrap(),
		html
	);
	for name in ["screen1.gif", "screenshot1.gif"] {
		assert!(!output.path().join(LOCAL_DIR).join(name).exists());
	}
}

/// Local write failures must still fail recovery rather than count as archive omissions.
#[tokio::test]
async fn static_recovery_propagates_local_write_errors() {
	let server = MockServer::start().await;
	let output = tempfile::tempdir().unwrap();
	let name = "hd_products.gif";
	std::fs::write(
		output.path().join("index.html"),
		format!("<img src='{LOCAL_DIR}/{name}'>"),
	)
	.unwrap();
	let temp_path = output
		.path()
		.join(LOCAL_DIR)
		.join(format!(".{name}.webarchive-downloader-rust.tmp"));
	std::fs::create_dir_all(&temp_path).unwrap();
	mock_cdx(&server, "example.com", "domain", 200, &asset_record(name)).await;
	Mock::given(path(format!(
		"/web/{TIMESTAMP}id_/http://{HOST}/shared/{name}"
	)))
	.respond_with(ResponseTemplate::new(200).set_body_string("GIF89a"))
	.expect(1)
	.mount(&server)
	.await;
	let result = run_cli(&server, output.path(), &["--repair-output"]).await;
	let stderr = String::from_utf8_lossy(&result.stderr);
	assert_eq!(result.status.code(), Some(1), "{stderr}");
	assert!(
		stderr.contains("Error: repair failed: failed to create"),
		"{stderr}"
	);
	assert!(
		!stderr.contains("static asset snapshot unavailable:"),
		"{stderr}"
	);
	assert!(temp_path.is_dir());
}
