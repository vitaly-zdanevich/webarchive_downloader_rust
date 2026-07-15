use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lol_html::{HtmlRewriter, Settings, element};
use url::Url;

use crate::pathmap::SiteMapper;

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

pub fn extract_downloadable_references(
    input: &str,
    base_url: &Url,
    mapper: &SiteMapper,
) -> Vec<String> {
    let mut references = Vec::new();
    for value in extract_quoted_url_attrs(input) {
        let Some(url) = resolve_reference(base_url, &value) else {
            continue;
        };
        let Some(host) = url.host_str() else {
            continue;
        };
        if mapper.is_related_host(host) && is_extra_file_url(&url) {
            references.push(url.to_string());
        }
    }
    references.sort();
    references.dedup();
    references
}

pub fn remove_missing_download_links(root: &Path) -> Result<usize> {
    let html_files = collect_html_files(root)?;
    let mut removed = 0;

    for file in html_files {
        let input = read_lossy(&file)?;
        let mut output = Vec::with_capacity(input.len());
        let mut removed_in_file = 0;
        let settings = Settings {
            element_content_handlers: vec![element!("a[href]", |element| {
                let Some(href) = element.get_attribute("href") else {
                    return Ok(());
                };
                if should_remove_download_href(root, &file, &href) {
                    element.remove_attribute("href");
                    removed_in_file += 1;
                }
                Ok(())
            })],
            ..Settings::default()
        };

        let mut rewriter =
            HtmlRewriter::new(settings, |chunk: &[u8]| output.extend_from_slice(chunk));
        rewriter
            .write(input.as_bytes())
            .with_context(|| format!("failed to rewrite {}", file.display()))?;
        rewriter
            .end()
            .with_context(|| format!("failed to finish rewriting {}", file.display()))?;

        if removed_in_file > 0 {
            fs::write(&file, output)
                .with_context(|| format!("failed to write {}", file.display()))?;
            removed += removed_in_file;
        }
    }

    Ok(removed)
}

fn should_remove_download_href(root: &Path, from_file: &Path, href: &str) -> bool {
    let trimmed = href.trim();
    if trimmed.is_empty() || should_skip_reference(trimmed) {
        return false;
    }

    if let Ok(url) = Url::parse(trimmed) {
        return is_downloadable_file_url(&url);
    }

    let path = trimmed
        .split_once('#')
        .map_or(trimmed, |(path, _)| path)
        .split_once('?')
        .map_or_else(
            || trimmed.split_once('#').map_or(trimmed, |(path, _)| path),
            |(path, _)| path,
        );
    if !is_downloadable_path(path) {
        return false;
    }

    let target = resolve_local_reference(root, from_file, path);
    target.starts_with(&normalize_path(root)) && !target.exists()
}

fn extract_quoted_url_attrs(input: &str) -> Vec<String> {
    let mut values = Vec::new();
    let lower = input.to_ascii_lowercase();
    let mut offset = 0;

    while let Some(relative_start) = lower[offset..].find(['h', 's']) {
        let start = offset + relative_start;
        let rest = &lower[start..];
        let attr_len = if rest.starts_with("href") {
            4
        } else if rest.starts_with("src") {
            3
        } else {
            offset = start + 1;
            continue;
        };

        let after_name = start + attr_len;
        let Some((value_start, quote)) = attr_value_start(input, after_name) else {
            offset = after_name;
            continue;
        };
        let Some(value_end) = input[value_start..]
            .find(quote)
            .map(|end| value_start + end)
        else {
            break;
        };
        values.push(input[value_start..value_end].to_owned());
        offset = value_end + quote.len_utf8();
    }

    values
}

fn attr_value_start(input: &str, offset: usize) -> Option<(usize, char)> {
    let mut index = offset;
    for character in input[index..].chars() {
        if character.is_whitespace() {
            index += character.len_utf8();
        } else {
            break;
        }
    }
    if input[index..].chars().next()? != '=' {
        return None;
    }
    index += 1;
    for character in input[index..].chars() {
        if character.is_whitespace() {
            index += character.len_utf8();
        } else {
            break;
        }
    }
    let quote = input[index..].chars().next()?;
    if quote == '"' || quote == '\'' {
        Some((index + quote.len_utf8(), quote))
    } else {
        None
    }
}

fn resolve_reference(base_url: &Url, value: &str) -> Option<Url> {
    let trimmed = value.trim();
    if trimmed.is_empty() || should_skip_reference(trimmed) {
        return None;
    }

    let without_fragment = trimmed.split_once('#').map_or(trimmed, |(path, _)| path);
    base_url.join(&unwrap_wayback_url(without_fragment)).ok()
}

fn unwrap_wayback_url(value: &str) -> String {
    let Ok(url) = Url::parse(value) else {
        return value.to_owned();
    };
    let Some(host) = url.host_str() else {
        return value.to_owned();
    };
    if !host.eq_ignore_ascii_case("web.archive.org") {
        return value.to_owned();
    }

    let path = url.path();
    let Some(rest) = path.strip_prefix("/web/") else {
        return value.to_owned();
    };
    let Some((_, original)) = rest.split_once('/') else {
        return value.to_owned();
    };
    original.to_owned()
}

fn collect_html_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_html_files_into(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_html_files_into(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry.with_context(|| format!("failed to read entry in {}", path.display()))?;
        let path = entry.path();
        let metadata = entry
            .metadata()
            .with_context(|| format!("failed to read metadata for {}", path.display()))?;
        if metadata.is_dir() {
            collect_html_files_into(&path, files)?;
        } else if metadata.is_file() && is_html_file(&path) {
            files.push(path);
        }
    }
    Ok(())
}

fn resolve_local_reference(root: &Path, from_file: &Path, path: &str) -> PathBuf {
    let decoded = percent_decode(path);
    let target = if decoded.starts_with('/') {
        root.join(decoded.trim_start_matches('/'))
    } else {
        from_file.parent().unwrap_or(root).join(decoded)
    };

    normalize_path(&target)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn is_downloadable_path(path: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, extension)| is_downloadable_extension(extension))
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

fn should_skip_reference(value: &str) -> bool {
    value.starts_with('#')
        || value.starts_with("//")
        || value.starts_with("mailto:")
        || value.starts_with("tel:")
        || value.starts_with("javascript:")
        || value.starts_with("data:")
        || value.starts_with("blob:")
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            output.push(high * 16 + low);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn read_lossy(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn is_html_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_related_download_links() {
        let mapper = SiteMapper::new("example.com").unwrap();
        let base = Url::parse("http://www.example.com/downloads.html").unwrap();
        let references = extract_downloadable_references(
            r#"<a href="http://downloads.example.com/file.exe">Download</a><a href="http://other.test/file.exe">Other</a>"#,
            &base,
            &mapper,
        );

        assert_eq!(
            references,
            vec!["http://downloads.example.com/file.exe".to_owned()]
        );
    }

    #[test]
    fn extracts_related_static_asset_links() {
        let mapper = SiteMapper::new("example.com").unwrap();
        let base = Url::parse("http://www.example.com/forums/topic.html").unwrap();
        let references = extract_downloadable_references(
            r#"<img src="http://downloads.example.com/files/preview/shot.jpg"><img src="http://other.test/shot.jpg">"#,
            &base,
            &mapper,
        );

        assert_eq!(
            references,
            vec!["http://downloads.example.com/files/preview/shot.jpg".to_owned()]
        );
    }

    #[test]
    fn removes_missing_local_download_links() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(
            root.join("index.html"),
            r#"<a href="_hosts/downloads.example.com/file.exe" target="download">Download</a>"#,
        )
        .unwrap();

        let removed = remove_missing_download_links(root).unwrap();

        assert_eq!(removed, 1);
        assert_eq!(
            fs::read_to_string(root.join("index.html")).unwrap(),
            r#"<a target="download">Download</a>"#
        );
    }

    #[test]
    fn keeps_existing_local_download_links() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("_hosts/downloads.example.com")).unwrap();
        fs::write(root.join("_hosts/downloads.example.com/file.exe"), b"exe").unwrap();
        fs::write(
            root.join("index.html"),
            r#"<a href="_hosts/downloads.example.com/file.exe">Download</a>"#,
        )
        .unwrap();

        let removed = remove_missing_download_links(root).unwrap();

        assert_eq!(removed, 0);
        assert_eq!(
            fs::read_to_string(root.join("index.html")).unwrap(),
            r#"<a href="_hosts/downloads.example.com/file.exe">Download</a>"#
        );
    }
}
