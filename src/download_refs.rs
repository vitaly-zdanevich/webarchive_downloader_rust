use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use lol_html::{HtmlRewriter, Settings, element};
use url::Url;

use crate::html_attrs::decoded_attribute;
use crate::link_validation::extract_document_references;
use crate::noise::is_archive_noise_reference;
use crate::pathmap::{SiteMapper, unwrap_wayback_url};
use crate::rewrite::extract_html_base_url;
use crate::text_encoding::TextDocumentKind;

/// Finds related archived pages and resources without restricting file extensions.
/// Only HTTP(S) URLs are returned; the caller queries Wayback, never the live site.
pub(crate) fn extract_related_references(
	input: &str,
	base_url: &Url,
	mapper: &SiteMapper,
	kind: TextDocumentKind,
) -> Result<Vec<String>> {
	let effective_base = match kind {
		TextDocumentKind::Html => {
			extract_html_base_url(input, base_url).unwrap_or_else(|| base_url.clone())
		}
		TextDocumentKind::Css | TextDocumentKind::JavaScript => base_url.clone(),
	};
	let mut references = Vec::new();
	for value in extract_document_references(input, kind)? {
		let value = value.trim();
		if value.is_empty() || value.starts_with('#') {
			continue;
		}
		let Ok(mut url) = effective_base.join(&unwrap_wayback_url(value)) else {
			continue;
		};
		url.set_fragment(None);
		if matches!(url.scheme(), "http" | "https")
			&& url.host_str().is_some_and(|host| mapper.is_related_host(host))
			&& !is_archive_noise_reference(url.as_str())
			&& !is_interactive_forum_extra_reference(&url)
		{
			references.push(url.to_string());
		}
	}
	references.sort();
	references.dedup();
	Ok(references)
}

/// Returns the HTML document's effective base URL (honoring `<base href>`) and all
/// resolved HTTP(S) `<script src="...">` URLs it references.
pub(crate) fn extract_html_script_references(input: &str, base_url: &Url) -> (Url, Vec<String>) {
	let effective_base =
		extract_html_base_url(input, base_url).unwrap_or_else(|| base_url.clone());
	let scripts = Rc::new(RefCell::new(Vec::<String>::new()));
	let mut parser = HtmlRewriter::new(
		Settings {
			element_content_handlers: vec![element!("script[src]", {
				let scripts = Rc::clone(&scripts);
				let effective_base = effective_base.clone();
				move |element| {
					if let Some(src) = decoded_attribute(element, "src") {
						let trimmed = src.trim();
						if !trimmed.is_empty()
							&& !trimmed.starts_with('#')
							&& let Ok(mut url) = effective_base.join(&unwrap_wayback_url(trimmed))
							&& matches!(url.scheme(), "http" | "https")
						{
							url.set_fragment(None);
							scripts.borrow_mut().push(url.to_string());
						}
					}
					Ok(())
				}
			})],
			..Settings::default()
		},
		|_: &[u8]| {},
	);
	let _ = parser.write(input.as_bytes()).and_then(|_| parser.end());
	let mut urls = scripts.borrow().clone();
	urls.sort();
	urls.dedup();
	(effective_base, urls)
}

/// Identifies interactive forum compose, quote, private-message, login, and moderation
/// endpoints that require an authenticated session and should not trigger per-URL
/// extra CDX lookups when absent from initial discovery.
pub(crate) fn is_interactive_forum_extra_reference(url: &Url) -> bool {
    let path = url.path().to_ascii_lowercase();
    if [
        "/posting.php",
        "/privmsg.php",
        "/mcp.php",
        "/modcp.php",
        "/report.php",
    ]
    .into_iter()
    .any(|suffix| path.ends_with(suffix))
    {
        return true;
    }

    if path.ends_with("/login.php") && url.query().is_some() {
        return true;
    }

    if path.ends_with("/ucp.php") {
        return url.query_pairs().any(|(key, value)| {
            let key = key.trim_start_matches("amp;");
            (key.eq_ignore_ascii_case("mode")
                && matches!(
                    value.to_ascii_lowercase().as_str(),
                    "login"
                        | "logout"
                        | "register"
                        | "sendpassword"
                        | "activate"
                        | "resend_act"
                        | "delete_cookies"
                        | "terms"
                        | "privacy"
                ))
                || (key.eq_ignore_ascii_case("i") && value.eq_ignore_ascii_case("pm"))
        });
    }

    if path.ends_with("/profile.php") || path.ends_with("/memberlist.php") {
        return url.query_pairs().any(|(key, value)| {
            let key = key.trim_start_matches("amp;");
            key.eq_ignore_ascii_case("mode")
                && matches!(
                    value.to_ascii_lowercase().as_str(),
                    "email"
                        | "register"
                        | "sendpassword"
                        | "activate"
                        | "editprofile"
                        | "contactadmin"
                )
        });
    }

    if path.ends_with("/viewtopic.php") {
        return url.query_pairs().any(|(key, value)| {
            let key = key.trim_start_matches("amp;");
            key.eq_ignore_ascii_case("view")
                && matches!(
                    value.to_ascii_lowercase().as_str(),
                    "next" | "previous" | "print" | "unread"
                )
        });
    }

    if path.ends_with("/search.php") {
        return url.query_pairs().any(|(key, _)| {
            let key = key.trim_start_matches("amp;");
            key.eq_ignore_ascii_case("search_author")
                || key.eq_ignore_ascii_case("search_keywords")
        });
    }

    false
}

pub fn is_downloadable_file_url(url: &Url) -> bool {
    let Some(extension) = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .and_then(extension_of)
    else {
        return false;
    };

    is_downloadable_extension(extension)
}

pub fn is_extra_file_url(url: &Url) -> bool {
    let Some(extension) = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .and_then(extension_of)
    else {
        return false;
    };

    is_downloadable_extension(extension) || is_static_resource_extension(extension)
}

fn is_downloadable_extension(extension: &str) -> bool {
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "7z" | "apk"
            | "bin"
            | "bz2"
            | "deb"
            | "dmg"
            | "exe"
            | "gz"
            | "iso"
            | "jar"
            | "msi"
            | "pkg"
            | "rar"
            | "rpm"
            | "sit"
            | "sitx"
            | "tar"
            | "tgz"
            | "xz"
            | "zip"
    )
}

fn is_static_resource_extension(extension: &str) -> bool {
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "atom"
            | "bmp"
            | "css"
            | "eot"
            | "gif"
            | "ico"
            | "jpe"
            | "jpeg"
            | "jpg"
            | "js"
            | "json"
            | "m4a"
            | "mid"
            | "midi"
            | "mjs"
            | "mov"
            | "mp3"
            | "mp4"
            | "ogg"
            | "otf"
            | "pdf"
            | "png"
            | "rss"
            | "svg"
            | "swf"
            | "ttf"
            | "txt"
            | "wav"
            | "webm"
            | "webp"
            | "woff"
            | "woff2"
            | "xml"
    )
}

fn extension_of(segment: &str) -> Option<&str> {
    segment.rsplit_once('.').map(|(_, extension)| extension)
}

#[cfg(test)]
mod tests {
    use super::*;

	/// HTML entities are syntax, whereas identical CSS/JS raw text is literal URL data.
	#[test]
	fn discovery_decodes_entities_at_html_attribute_boundaries_only() {
		let mapper = SiteMapper::new("example.com").unwrap();
		let base = Url::parse("http://example.com/").unwrap();
		let references = extract_related_references(r#"<a href="view.php?a=1&amp;b=2">Page</a>
<img src="img/a&#38;b.gif"><source srcset="img/c&#x26;d.gif 2x">
<style>i { background:url('img/literal&amp;name.gif'); }</style>
<script>image.src = 'img/literal&amp;name.gif'; document.write('<img src="img/generated&amp;name.gif">');</script>"#, &base, &mapper, TextDocumentKind::Html).unwrap();
		assert_eq!(references, vec![
			"http://example.com/img/a&b.gif",
			"http://example.com/img/c&d.gif",
			"http://example.com/img/generated&name.gif",
			"http://example.com/img/literal&amp;name.gif",
			"http://example.com/view.php?a=1&b=2",
		]);
	}

	/// Uses the same HTML/CSS parser as validation, including unquoted and wrapped URLs.
	#[test]
	fn discovers_pages_srcset_css_and_wrapped_queries() {
		let mapper = SiteMapper::new("example.com").unwrap();
		let base = Url::parse("http://example.com/pages/index.html").unwrap();
		let refs = extract_related_references(
			r#"<a href=next>Next</a><img srcset='//assets.example.com/a.gif 1x, /b.gif 2x'><a href='https://web.archive.org/web/20080101000000/http://example.com/viewtopic.php?t=7&amp;p=9'>Topic</a><a href='javascript:alert(1)'>JS</a><a href='https://unrelated.invalid/a.html'>External</a>"#,
			&base, &mapper, TextDocumentKind::Html).unwrap();
		assert!(refs.contains(&"http://example.com/pages/next".to_owned()));
		assert!(refs.contains(&"http://assets.example.com/a.gif".to_owned()));
		assert!(refs.contains(&"http://example.com/b.gif".to_owned()));
		assert!(refs.contains(&"http://example.com/viewtopic.php?t=7&p=9".to_owned()));
		assert_eq!(refs.len(), 4);
		assert_eq!(extract_related_references("@import 'theme.css'; body { background: url(../image.gif); }", &base, &mapper, TextDocumentKind::Css).unwrap(), vec!["http://example.com/image.gif", "http://example.com/pages/theme.css"]);
	}

	/// Discovers downloads on related hosts without including unrelated sites.
	#[test]
	fn extracts_related_download_links() {
		let mapper = SiteMapper::new("example.com").unwrap();
		let base = Url::parse("http://www.example.com/downloads.html").unwrap();
		let references = extract_related_references(
			r#"<a href="http://downloads.example.com/file.exe">Download</a><a href="http://other.test/file.exe">Other</a>"#,
			&base,
			&mapper,
			TextDocumentKind::Html,
		).unwrap();

		assert_eq!(
			references,
			vec!["http://downloads.example.com/file.exe".to_owned()]
		);
	}

	/// Discovers static assets on related hosts without including unrelated sites.
	#[test]
	fn extracts_related_static_asset_links() {
		let mapper = SiteMapper::new("example.com").unwrap();
		let base = Url::parse("http://www.example.com/forums/topic.html").unwrap();
		let references = extract_related_references(
			r#"<img src="http://downloads.example.com/files/preview/shot.jpg"><img src="http://other.test/shot.jpg">"#,
			&base,
			&mapper,
			TextDocumentKind::Html,
		).unwrap();

		assert_eq!(
			references,
			vec!["http://downloads.example.com/files/preview/shot.jpg".to_owned()]
		);
	}

	/// Per-post quote, reply, private-message compose, email, and login redirect actions
	/// must not be queued for per-URL extra CDX lookups, while public profiles and topics are kept.
	#[test]
	fn skips_interactive_forum_compose_and_login_extra_references() {
		let mapper = SiteMapper::new("example.com").unwrap();
		let base = Url::parse("http://www.example.com/forums/viewtopic.php?t=1").unwrap();
		let references = extract_related_references(
			r#"<a href="posting.php?mode=quote&amp;p=100">Quote</a>
			   <a href="posting.php?mode=reply&amp;t=1">Reply</a>
			   <a href="privmsg.php?mode=post&amp;u=2">PM</a>
			   <a href="ucp.php?i=pm&amp;mode=compose&amp;action=quotepost&amp;p=100">PM Quote</a>
			   <a href="ucp.php?mode=login&amp;redirect=viewtopic.php">Login</a>
			   <a href="login.php?redirect=posting.php&amp;mode=quote&amp;p=100">Login2</a>
			   <a href="profile.php?mode=email&amp;u=2">Email</a>
			   <a href="viewtopic.php?t=1&amp;view=next">Next</a>
			   <a href="viewtopic.php?t=1&amp;view=previous">Prev</a>
			   <a href="viewtopic.php?t=1&amp;view=print">Print</a>
			   <a href="search.php?search_author=Bullfrog">Author posts</a>
			   <a href="search.php?search_id=active_topics">Active topics</a>
			   <a href="memberlist.php?mode=viewprofile&amp;u=2">Profile</a>
			   <a href="viewtopic.php?t=2">Next Topic</a>"#,
			&base,
			&mapper,
			TextDocumentKind::Html,
		)
		.unwrap();

		assert_eq!(
			references,
			vec![
				"http://www.example.com/forums/memberlist.php?mode=viewprofile&u=2".to_owned(),
				"http://www.example.com/forums/search.php?search_id=active_topics".to_owned(),
				"http://www.example.com/forums/viewtopic.php?t=2".to_owned(),
			]
		);
	}

	/// Relative references in HTML documents with `<base href>` must resolve against the
	/// `<base href>` URL rather than the document URL during extra-download discovery.
	#[test]
	fn discovery_resolves_relative_references_against_base_href() {
		let mapper = SiteMapper::new("example.com").unwrap();
		let base = Url::parse("http://example.com/archive/old.html").unwrap();
		let references = extract_related_references(
			r#"<html><head><base href="http://example.com/sub/dir/"></head><body><img src="logo.gif"><a href="next.html">Next</a></body></html>"#,
			&base,
			&mapper,
			TextDocumentKind::Html,
		)
		.unwrap();

		assert_eq!(
			references,
			vec![
				"http://example.com/sub/dir/logo.gif".to_owned(),
				"http://example.com/sub/dir/next.html".to_owned(),
			]
		);
	}

	/// Standalone JavaScript files must yield static URL literals and embedded HTML fragments
	/// while ignoring comments, string concatenations, and bare path delimiters like `"/"`.
	#[test]
	fn extracts_dependencies_from_standalone_javascript() {
		let mapper = SiteMapper::new("example.com").unwrap();
		let base = Url::parse("http://example.com/index.html").unwrap();
		let js = r#"
			// '/img/comment.gif'
			var sep = "/";
			var dir = "/img/";
			var dyn = "/img/" + name + ".gif";
			var hero = "images/hero.gif";
			var photo = "/photo";
			document.write('<img src="/img/written.png">');
		"#;
		let references =
			extract_related_references(js, &base, &mapper, TextDocumentKind::JavaScript).unwrap();
		assert_eq!(
			references,
			vec![
				"http://example.com/images/hero.gif".to_owned(),
				"http://example.com/img/written.png".to_owned(),
				"http://example.com/photo".to_owned(),
			]
		);

		let (effective_base, scripts) = extract_html_script_references(
			r#"<head><base href="http://example.com/app/"><script src="js/menu.js"></script><script src="/web/20060101/http://example.com/shared.js#v1"></script></head>"#,
			&Url::parse("http://example.com/old/page.html").unwrap(),
		);
		assert_eq!(effective_base.as_str(), "http://example.com/app/");
		assert_eq!(
			scripts,
			vec![
				"http://example.com/app/js/menu.js".to_owned(),
				"http://example.com/shared.js".to_owned(),
			]
		);
	}
}
