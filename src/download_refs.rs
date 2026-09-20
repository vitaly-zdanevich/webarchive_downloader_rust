use anyhow::Result;
use url::Url;

use crate::link_validation::extract_document_references;
use crate::noise::is_archive_noise_reference;
use crate::pathmap::{SiteMapper, unwrap_wayback_url};

/// Finds related archived pages and resources without restricting file extensions.
/// Only HTTP(S) URLs are returned; the caller queries Wayback, never the live site.
pub(crate) fn extract_related_references(
	input: &str,
	base_url: &Url,
	mapper: &SiteMapper,
	is_css: bool,
) -> Result<Vec<String>> {
	let mut references = Vec::new();
	for value in extract_document_references(input, is_css)? {
		let value = value.trim();
		if value.is_empty() || value.starts_with('#') {
			continue;
		}
		let Ok(mut url) = base_url.join(&unwrap_wayback_url(value)) else {
			continue;
		};
		url.set_fragment(None);
		if matches!(url.scheme(), "http" | "https")
			&& url.host_str().is_some_and(|host| mapper.is_related_host(host))
			&& !is_archive_noise_reference(url.as_str())
		{
			references.push(url.to_string());
		}
	}
	references.sort();
	references.dedup();
	Ok(references)
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

	/// Uses the same HTML/CSS parser as validation, including unquoted and wrapped URLs.
	#[test]
	fn discovers_pages_srcset_css_and_wrapped_queries() {
		let mapper = SiteMapper::new("example.com").unwrap();
		let base = Url::parse("http://example.com/pages/index.html").unwrap();
		let refs = extract_related_references(
			r#"<a href=next>Next</a><img srcset='//assets.example.com/a.gif 1x, /b.gif 2x'><a href='https://web.archive.org/web/20080101000000/http://example.com/viewtopic.php?t=7&amp;p=9'>Topic</a><a href='javascript:alert(1)'>JS</a><a href='https://unrelated.invalid/a.html'>External</a>"#,
			&base, &mapper, false).unwrap();
		assert!(refs.contains(&"http://example.com/pages/next".to_owned()));
		assert!(refs.contains(&"http://assets.example.com/a.gif".to_owned()));
		assert!(refs.contains(&"http://example.com/b.gif".to_owned()));
		assert!(refs.iter().any(|url| url.contains("viewtopic.php?t=7") && url.contains("p=9")));
		assert_eq!(refs.len(), 4);
		assert_eq!(extract_related_references("@import 'theme.css'; body { background: url(../image.gif); }", &base, &mapper, true).unwrap(), vec!["http://example.com/image.gif", "http://example.com/pages/theme.css"]);
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
			false,
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
			false,
		).unwrap();

		assert_eq!(
			references,
			vec!["http://downloads.example.com/files/preview/shot.jpg".to_owned()]
		);
	}
}
