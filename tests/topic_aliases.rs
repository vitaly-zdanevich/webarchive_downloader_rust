//! Regression coverage for forum-scoped topic aliases and their relative links.

use std::fs;
use std::path::Path;

use webarchive_downloader_rust::alias_repair::create_missing_topic_aliases;
use webarchive_downloader_rust::link_validation::validate_local_links;

const PRIMARY: &str = "forums/viewtopic.php__q_primary.html";
const OTHER: &str = "_hosts/other.example.com/forums/viewtopic.php__q_other.html";
const ALIAS: &str = "_hosts/other.example.com/forums/viewtopic.php__q_alias.html";

/// Writes mock archive files, creating their parent directories as necessary.
fn write(root: &Path, name: &str, bytes: impl AsRef<[u8]>) {
	let path = root.join(name);
	fs::create_dir_all(path.parent().unwrap()).unwrap();
	fs::write(path, bytes).unwrap();
}

/// Numeric post IDs and titles must be looked up in the destination forum.
#[test]
fn alias_matching_uses_destination_forum() {
	for fragment in ["#p42", ""] {
		let root = tempfile::tempdir().unwrap();
		let primary = "<title>Forum :: View topic - Same title</title><p id='p42'>PRIMARY POST</p>";
		write(root.path(), PRIMARY, primary);
		write(root.path(), OTHER, "<title>Forum :: View topic - Same title</title><p id='p42'>UNRELATED POST</p>");
		write(root.path(), "forums/index.html", format!("<a href='viewtopic.php__q_missing.html{fragment}'>Same title</a>"));
		assert_eq!(create_missing_topic_aliases(root.path()).unwrap().created, 1);
		assert_eq!(fs::read_to_string(root.path().join("forums/viewtopic.php__q_missing.html")).unwrap(), primary);
	}
}

/// The referring page's own anchors/title cannot justify an alias in another forum.
#[test]
fn self_matches_cannot_cross_forum_boundaries() {
	for (text, fragment) in [("Same title", ""), ("Print view", ""), ("Post", "#p42")] {
		let root = tempfile::tempdir().unwrap();
		write(root.path(), PRIMARY, format!("<title>Forum :: View topic - Same title</title><p id='p42'>Primary</p><a href='../{ALIAS}{fragment}'>{text}</a>"));
		assert_eq!(create_missing_topic_aliases(root.path()).unwrap().created, 0);
		assert!(!root.path().join(ALIAS).exists());
	}
}

/// A latest-post link in a row cannot import an unrelated forum by title or anchor.
#[test]
fn sibling_matches_cannot_cross_forum_boundaries() {
	for sibling in [OTHER, "_hosts/other.example.com/forums/viewtopic.php__q_absent.html"] {
		let root = tempfile::tempdir().unwrap();
		write(root.path(), OTHER, "<title>Forum :: View topic - Same title</title><p id='p42'>Unrelated</p>");
		write(root.path(), "forums/index.html", format!("<li><a href='viewtopic.php__q_missing.html'>Same title</a><a href='../{sibling}#p42'>Latest</a></li>"));
		create_missing_topic_aliases(root.path()).unwrap();
		assert!(!root.path().join("forums/viewtopic.php__q_missing.html").exists());
	}
}

/// Common layout anchors such as 'top' do not identify a post or discussion.
#[test]
fn layout_anchors_are_not_post_identity() {
	for anchor in ["top", "content", "p"] {
		let root = tempfile::tempdir().unwrap();
		write(root.path(), PRIMARY, format!("<title>Forum :: View topic - Different</title><p id='{anchor}'>Wrong</p>"));
		write(root.path(), "forums/index.html", format!("<a href='viewtopic.php__q_missing.html#{anchor}'>Unknown</a>"));
		assert_eq!(create_missing_topic_aliases(root.path()).unwrap().created, 0);
	}
}

/// Forums installed in different directories on one host also have independent post IDs.
#[test]
fn separate_forum_directories_do_not_share_post_ids() {
	let root = tempfile::tempdir().unwrap();
	write(root.path(), PRIMARY, "<title>Forum :: View topic - Topic</title><p id='p42'>Wrong forum</p>");
	write(root.path(), "support/forums/index.html", "<a href='viewtopic.php__q_missing.html#p42'>Topic</a>");
	assert_eq!(create_missing_topic_aliases(root.path()).unwrap().created, 0);
	assert!(!root.path().join("support/forums/viewtopic.php__q_missing.html").exists());
}

/// Cross-forum links are resolved in the destination namespace, not the referring page's.
#[test]
fn explicit_cross_forum_link_uses_destination_topic() {
	let root = tempfile::tempdir().unwrap();
	let other = "<title>Forum :: View topic - Topic</title><p id='p42'>Correct destination</p>";
	write(root.path(), PRIMARY, format!("<title>Forum :: View topic - Topic</title><p id='p42'>Referring post</p><a href='../{ALIAS}#p42'>Topic</a>"));
	write(root.path(), OTHER, other);
	assert_eq!(create_missing_topic_aliases(root.path()).unwrap().created, 1);
	assert_eq!(fs::read_to_string(root.path().join(ALIAS)).unwrap(), other);
}

/// Keeping copies beside their source preserves images, styles, scripts and navigation.
#[test]
fn aliases_preserve_relative_links_and_existing_files() {
	let root = tempfile::tempdir().unwrap();
	let source = "<title>Forum :: View topic - Topic</title><link href='style.css'><script src='script.js'></script><img src='images/logo.gif'><p id='p42'>Topic</p><a href='viewtopic.php__q_missing.html#p42'>Post</a>";
	write(root.path(), PRIMARY, source);
	for resource in ["forums/style.css", "forums/script.js", "forums/images/logo.gif"] {
		write(root.path(), resource, "");
	}
	write(root.path(), "forums/viewtopic.php__q_existing.html", "<p>Keep existing content</p>");
	write(root.path(), "forums/index.html", "<a href='viewtopic.php__q_existing.html#p42'>Post</a>");
	assert_eq!(create_missing_topic_aliases(root.path()).unwrap().created, 1);
	assert!(validate_local_links(root.path()).unwrap().missing.is_empty());
	assert_eq!(fs::read_to_string(root.path().join("forums/viewtopic.php__q_missing.html")).unwrap(), source);
	assert_eq!(fs::read_to_string(root.path().join(PRIMARY)).unwrap(), source);
	assert_eq!(fs::read_to_string(root.path().join("forums/viewtopic.php__q_existing.html")).unwrap(), "<p>Keep existing content</p>");
	assert_eq!(create_missing_topic_aliases(root.path()).unwrap().created, 0);
}

/// Symlinked forums outside the output cannot be used to create topic aliases.
#[cfg(unix)]
#[test]
fn topic_aliases_do_not_follow_directory_symlinks() {
	let root = tempfile::tempdir().unwrap();
	let outside = tempfile::tempdir().unwrap();
	write(outside.path(), "viewtopic.php__q_original.html", "<title>Forum :: View topic - Topic</title><a href='viewtopic.php__q_missing.html'>Topic</a>");
	std::os::unix::fs::symlink(outside.path(), root.path().join("forums")).unwrap();
	assert_eq!(create_missing_topic_aliases(root.path()).unwrap().created, 0);
	assert!(!outside.path().join("viewtopic.php__q_missing.html").exists());
}

/// A dangling destination symlink must not be followed or replaced by alias creation.
#[cfg(unix)]
#[test]
fn topic_aliases_preserve_dangling_symlinks() {
	let root = tempfile::tempdir().unwrap();
	let outside = tempfile::tempdir().unwrap();
	write(root.path(), PRIMARY, "<title>Forum :: View topic - Topic</title><a href='viewtopic.php__q_missing.html'>Topic</a>");
	let alias = root.path().join("forums/viewtopic.php__q_missing.html");
	let target = outside.path().join("outside.html");
	std::os::unix::fs::symlink(&target, &alias).unwrap();
	assert_eq!(create_missing_topic_aliases(root.path()).unwrap().created, 0);
	assert_eq!(fs::read_link(alias).unwrap(), target);
	assert!(!target.exists());
}

/// New copies must retain source permissions, as the original copy operation did.
#[cfg(unix)]
#[test]
fn topic_aliases_preserve_source_permissions() {
	use std::os::unix::fs::PermissionsExt;
	let root = tempfile::tempdir().unwrap();
	write(root.path(), PRIMARY, "<title>Forum :: View topic - Topic</title><a href='viewtopic.php__q_missing.html'>Topic</a>");
	fs::set_permissions(root.path().join(PRIMARY), fs::Permissions::from_mode(0o600)).unwrap();
	assert_eq!(create_missing_topic_aliases(root.path()).unwrap().created, 1);
	let metadata = fs::metadata(root.path().join("forums/viewtopic.php__q_missing.html")).unwrap();
	assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
}
