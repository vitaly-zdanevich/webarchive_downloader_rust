//! Exercises offline navigation repair without contacting Wayback.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

/// Runs the real offline CLI against an unreachable archive endpoint.
fn repair(root: &Path, extra: &[&str]) -> Output {
	Command::new(env!("CARGO_BIN_EXE_webarchive-downloader-rust"))
		.args(["--repair-local-links", "--archive-root", "http://127.0.0.1:1", "--output"])
		.arg(root)
		.args(extra)
		.output()
		.unwrap()
}

/// Repair needs only existing files, preserves anchors/bytes, and is idempotent.
#[test]
fn repairs_existing_session_destinations_without_network() {
	let output = tempfile::tempdir().unwrap();
	let forums = output.path().join("forums");
	fs::create_dir(&forums).unwrap();
	fs::write(forums.join("faq.php.html"), "<p id='help'>FAQ</p>").unwrap();
	let original = b"<p>caf\xe9</p><a href='./faq.php?sid=abc#help'>FAQ</a><area href='/forums/faq.php?PHPSESSID=def'><a href='faq.php'>Again</a>";
	fs::write(forums.join("topic.html"), original).unwrap();
	let result = repair(output.path(), &["--strict-validate-links"]);
	assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
	let stdout = String::from_utf8_lossy(&result.stdout);
	assert!(stdout.contains("repaired 3 local navigation links in 1 files"), "{stdout}");
	assert!(stdout.contains("missing 0"), "{stdout}");
	let repaired = fs::read(forums.join("topic.html")).unwrap();
	assert!(repaired.starts_with(b"<p>caf\xe9</p>"));
	let text = String::from_utf8_lossy(&repaired);
	assert!(text.contains("faq.php.html#help"), "{text}");
	assert!(!text.contains("sid=abc"));
	assert_eq!(fs::read_to_string(forums.join("faq.php.html")).unwrap(), "<p id='help'>FAQ</p>");
	let again = repair(output.path(), &[]);
	assert!(again.status.success());
	assert!(String::from_utf8_lossy(&again.stdout).contains("repaired 0 local navigation links in 0 files"));
	assert_eq!(fs::read(forums.join("topic.html")).unwrap(), repaired);
}

/// Unknown pages, meaningful queries, forms, external links and existing targets stay intact.
#[test]
fn leaves_unproven_navigation_and_interactive_targets_unchanged() {
	let output = tempfile::tempdir().unwrap();
	fs::write(output.path().join("faq.php.html"), "FAQ").unwrap();
	fs::write(output.path().join("existing.php"), "original").unwrap();
	fs::write(output.path().join("existing.php.html"), "alternative").unwrap();
	fs::write(output.path().join("empty.php.html"), "").unwrap();
	let original = r#"<a href='faq.php?id=2&amp;sid=abc'>Page</a><a href='faq.php?postorder=desc'>Order</a><a href='missing.php?sid=abc'>Missing</a><a href='https://elsewhere.invalid/faq.php?sid=abc'>External</a><a href='//example.com/faq.php'>Protocol</a><a href='existing.php'>Existing</a><a href='empty.php'>Empty</a><form action='faq.php?sid=abc'></form><img src='faq.php'><a href='../faq.php'>Outside</a>"#;
	fs::write(output.path().join("index.html"), original).unwrap();
	let result = repair(output.path(), &["--strict-validate-links"]);
	assert_eq!(result.status.code(), Some(2), "{}", String::from_utf8_lossy(&result.stderr));
	assert!(String::from_utf8_lossy(&result.stdout).contains("repaired 0 local navigation links in 0 files"));
	assert_eq!(fs::read_to_string(output.path().join("index.html")).unwrap(), original);
}

/// A base element changes link resolution, so metadata-free repair must skip that page.
#[test]
fn does_not_repair_pages_with_a_base_url() {
	let output = tempfile::tempdir().unwrap();
	fs::write(output.path().join("faq.php.html"), "FAQ").unwrap();
	let original = "<a href='faq.php?sid=abc'>FAQ</a><base href='https://elsewhere.invalid/'>";
	fs::write(output.path().join("index.html"), original).unwrap();
	let result = repair(output.path(), &[]);
	assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
	assert_eq!(fs::read_to_string(output.path().join("index.html")).unwrap(), original);
}

/// Contradictory modes must fail before any file can be rewritten.
#[test]
fn offline_repair_rejects_other_modes() {
	let output = tempfile::tempdir().unwrap();
	for flag in ["--validate-only", "--repair-output", "--list", "--overwrite", "--no-rewrite"] {
		let result = repair(output.path(), &[flag]);
		assert_eq!(result.status.code(), Some(2));
		assert!(String::from_utf8_lossy(&result.stderr).contains("cannot be used with"));
	}
}

/// Encoded filenames and fragments must remain URLs rather than literal filesystem paths.
#[test]
fn preserves_url_encoding_in_repaired_links() {
	let output = tempfile::tempdir().unwrap();
	fs::write(output.path().join("page one.php.html"), "FAQ").unwrap();
	fs::write(output.path().join("index.html"), "<a href='page%20one.php?sid=a&amp;session_id=b#some%20id'>FAQ</a>").unwrap();
	let result = repair(output.path(), &["--strict-validate-links"]);
	assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
	assert!(fs::read_to_string(output.path().join("index.html")).unwrap().contains("page%20one.php.html#some%20id"));
}

/// Offline repair must not traverse source symlinks or use out-of-tree destinations.
#[cfg(unix)]
#[test]
fn respects_archive_boundaries_and_file_permissions() {
	use std::os::unix::fs::{PermissionsExt, symlink};
	let output = tempfile::tempdir().unwrap();
	let outside = tempfile::tempdir().unwrap();
	fs::write(outside.path().join("outside.html"), "<a href='faq.php'>FAQ</a>").unwrap();
	symlink(outside.path().join("outside.html"), output.path().join("symlink.html")).unwrap();
	symlink(outside.path(), output.path().join("external")).unwrap();
	symlink(outside.path().join("outside.html"), output.path().join("missing.php.html")).unwrap();
	fs::write(output.path().join("faq.php.html"), "FAQ").unwrap();
	let page = output.path().join("index.html");
	fs::write(&page, "<a href='faq.php'>FAQ</a><a href='missing.php'>Outside</a>").unwrap();
	fs::set_permissions(&page, fs::Permissions::from_mode(0o600)).unwrap();
	let result = repair(output.path(), &[]);
	assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
	assert_eq!(fs::metadata(&page).unwrap().permissions().mode() & 0o777, 0o600);
	let text = fs::read_to_string(&page).unwrap();
	assert!(text.contains("faq.php.html"));
	assert!(text.contains("href='missing.php'"));
	assert_eq!(fs::read_to_string(outside.path().join("outside.html")).unwrap(), "<a href='faq.php'>FAQ</a>");
}
