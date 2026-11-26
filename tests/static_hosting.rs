//! Verifies GitHub Pages markers through the real download, repair, and read-only CLI modes.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use wiremock::matchers::{path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Serves a primary page and an unlinked subdomain page without contacting Wayback.
async fn mock_archive() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(path("/cdx/search/cdx"))
        .and(query_param("url", "example.com"))
        .and(query_param("matchType", "domain"))
        .respond_with(ResponseTemplate::new(200).set_body_string(concat!(
            "20060101000000 http://example.com/ text/html 200 MAIN 100\n",
            "20060101000000 http://archive.example.com/history.html text/html 200 HISTORY 100\n",
        )))
        .mount(&server)
        .await;
    for (original, body) in [
        ("http://example.com/", "<p>Main site</p>"),
        ("http://archive.example.com/history.html", "<p>History</p>"),
    ] {
        Mock::given(path(format!("/web/20060101000000id_/{original}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
    }
    server
}

/// Runs the CLI with linked network recovery disabled to isolate output preparation.
async fn run_cli(root: &Path, archive: &str, arguments: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_webarchive-downloader-rust"));
    command
        .args(["example.com", "--output"])
        .arg(root)
        .args([
            "--archive-root",
            archive,
            "--max-extra-download-size-mib",
            "0",
            "--timeout-seconds",
            "2",
            "--strict-validate-links",
        ])
        .args(arguments);
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

/// Makes CLI failures show their complete diagnostics in the regression output.
fn assert_success(result: &Output) {
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr),
    );
}

/// Fresh downloads and resume create the marker; overwrite preserves an existing marker.
#[tokio::test]
async fn download_and_resume_prepare_github_pages_without_clobbering_marker() {
    let server = mock_archive().await;
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("nested/public");
    let marker = output.join(".nojekyll");

    assert_success(&run_cli(&output, &server.uri(), &[]).await);
    assert_eq!(fs::read(&marker).unwrap(), b"");
    let history = output.join("_hosts/archive.example.com/history.html");
    assert_eq!(fs::read_to_string(&history).unwrap(), "<p>History</p>");

    fs::remove_file(&marker).unwrap();
    assert_success(&run_cli(&output, &server.uri(), &[]).await);
    assert_eq!(fs::read(&marker).unwrap(), b"");
    assert_eq!(fs::read_to_string(&history).unwrap(), "<p>History</p>");

    fs::write(&marker, b"existing deployment marker\n").unwrap();
    assert_success(&run_cli(&output, &server.uri(), &["--overwrite"]).await);
    assert_eq!(fs::read(&marker).unwrap(), b"existing deployment marker\n");
}

/// Both repair modes prepare an existing archive without fetching or altering its content.
#[tokio::test]
async fn repairs_create_nojekyll_and_preserve_existing_marker() {
    for mode in ["--repair-output", "--repair-local-links"] {
        let output = tempfile::tempdir().unwrap();
        let history = output
            .path()
            .join("_hosts/archive.example.com/history.html");
        fs::create_dir_all(history.parent().unwrap()).unwrap();
        fs::write(&history, "<p>Preserved history</p>").unwrap();
        let marker = output.path().join(".nojekyll");

        assert_success(&run_cli(output.path(), "http://127.0.0.1:1", &[mode]).await);
        assert_eq!(fs::read(&marker).unwrap(), b"");
        assert_eq!(
            fs::read_to_string(&history).unwrap(),
            "<p>Preserved history</p>"
        );

        fs::write(&marker, b"existing marker\n").unwrap();
        assert_success(&run_cli(output.path(), "http://127.0.0.1:1", &[mode]).await);
        assert_eq!(fs::read(&marker).unwrap(), b"existing marker\n");
        assert_eq!(
            fs::read_to_string(&history).unwrap(),
            "<p>Preserved history</p>"
        );
    }
}

/// Validation and capture listing must not create deployment metadata or output directories.
#[tokio::test]
async fn read_only_modes_do_not_create_nojekyll() {
    let server = mock_archive().await;
    let output = tempfile::tempdir().unwrap();
    fs::write(output.path().join("index.html"), "<p>Existing archive</p>").unwrap();
    for mode in ["--validate-only", "--list"] {
        assert_success(&run_cli(output.path(), &server.uri(), &[mode]).await);
        assert!(!output.path().join(".nojekyll").exists());
        assert_eq!(fs::read_dir(output.path()).unwrap().count(), 1);
    }
    let absent = output.path().join("absent");
    assert_success(&run_cli(&absent, &server.uri(), &["--list"]).await);
    assert!(!absent.exists());
}

/// A directory named .nojekyll is an error, not a successfully prepared Pages marker.
#[tokio::test]
async fn marker_path_conflicts_fail_without_removing_existing_entries() {
    let server = mock_archive().await;
    for arguments in [
        vec![],
        vec!["--repair-output"],
        vec!["--repair-local-links"],
    ] {
        let output = tempfile::tempdir().unwrap();
        let marker = output.path().join(".nojekyll");
        fs::create_dir(&marker).unwrap();
        fs::write(marker.join("keep.txt"), "existing content").unwrap();

        let result = run_cli(output.path(), &server.uri(), &arguments).await;
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains(".nojekyll"));
        assert_eq!(
            fs::read_to_string(marker.join("keep.txt")).unwrap(),
            "existing content"
        );
    }
}
