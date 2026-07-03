use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result, anyhow};
use lol_html::{HtmlRewriter, Settings, element, text};
use url::Url;

macro_rules! collect_attr {
    ($selector:literal, $attr:literal, $references:ident) => {{
        let references = Rc::clone(&$references);
        element!($selector, move |element| {
            if let Some(value) = element.get_attribute($attr) {
                references.borrow_mut().push(value);
            }
            Ok(())
        })
    }};
}

macro_rules! collect_srcset_attr {
    ($selector:literal, $attr:literal, $references:ident) => {{
        let references = Rc::clone(&$references);
        element!($selector, move |element| {
            if let Some(value) = element.get_attribute($attr) {
                references
                    .borrow_mut()
                    .extend(parse_srcset_references(&value));
            }
            Ok(())
        })
    }};
}

macro_rules! collect_javascript_attr {
    ($selector:literal, $attr:literal, $references:ident) => {{
        let references = Rc::clone(&$references);
        element!($selector, move |element| {
            if let Some(value) = element.get_attribute($attr) {
                references
                    .borrow_mut()
                    .extend(extract_javascript_string_references(&value));
            }
            Ok(())
        })
    }};
}

macro_rules! collect_guarded_attr {
    ($selector:literal, $attr:literal, $references:ident) => {{
        let references = Rc::clone(&$references);
        element!($selector, move |element| {
            if let Some(value) = element.get_attribute($attr)
                && looks_like_url_reference_or_file(&value)
            {
                references.borrow_mut().push(value);
            }
            Ok(())
        })
    }};
}

macro_rules! resource_attr_remover {
    ($selector:literal, $attr:literal, $root:ident, $file:ident, $removed_in_file:ident) => {{
        let removed_in_file = Rc::clone(&$removed_in_file);
        element!($selector, move |element| {
            if should_remove_missing_local_reference($root, $file, element.get_attribute($attr)) {
                element.remove_attribute($attr);
                removed_in_file.set(removed_in_file.get() + 1);
            }
            Ok(())
        })
    }};
}

macro_rules! event_attr_resource_remover {
    ($selector:literal, $attr:literal, $root:ident, $file:ident, $removed_in_file:ident) => {{
        let removed_in_file = Rc::clone(&$removed_in_file);
        element!($selector, move |element| {
            if let Some(value) = element.get_attribute($attr) {
                let (rewritten, removed) =
                    remove_missing_javascript_string_references(&value, $root, $file);
                if removed > 0 {
                    element.set_attribute($attr, &rewritten).ok();
                    removed_in_file.set(removed_in_file.get() + removed);
                }
            }
            Ok(())
        })
    }};
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// A local reference whose rewritten target does not exist in the output tree.
pub struct MissingLocalLink {
    /// HTML or CSS file that contains the missing reference.
    pub source: PathBuf,
    /// Reference text as it appeared in the source file.
    pub href: String,
    /// Resolved local filesystem target expected by the reference.
    pub target: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// An image element that has neither `src` nor `srcset`.
pub struct MissingImageSource {
    /// HTML file that contains the image element.
    pub source: PathBuf,
    /// Compact description of the image attributes useful for locating it.
    pub descriptor: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
/// Result of scanning generated local files for preservation defects.
pub struct LinkValidationReport {
    /// Number of local URL references checked for an existing target.
    pub checked: usize,
    /// Local URL references whose target does not exist.
    pub missing: Vec<MissingLocalLink>,
    /// Image elements that cannot render because no source attribute exists.
    pub missing_image_sources: Vec<MissingImageSource>,
}

/// Validates local references in generated HTML and CSS files.
///
/// The scan checks ordinary attributes, `srcset`, inline CSS, CSS files, common
/// JavaScript string references, legacy applet/object attributes, meta refresh
/// targets, and image elements that are missing both `src` and `srcset`.
pub fn validate_local_links(root: &Path) -> Result<LinkValidationReport> {
    let root = normalize_path(root);
    let mut files = Vec::new();
    collect_candidate_files(&root, &mut files)?;
    files.sort();

    let mut report = LinkValidationReport::default();
    for file in files {
        let input = read_lossy(&file)?;
        let references =
            if is_html_file(&file) {
                let html_report = extract_html_references(&input)
                    .with_context(|| format!("failed to parse {}", file.display()))?;
                report.missing_image_sources.extend(
                    html_report
                        .missing_image_sources
                        .into_iter()
                        .map(|descriptor| MissingImageSource {
                            source: file.clone(),
                            descriptor,
                        }),
                );
                html_report.references
            } else {
                extract_css_url_references(&input)
            };
        let mut seen_in_file = HashSet::new();

        for href in references {
            let Some(target) = resolve_local_reference(&root, &file, &href) else {
                continue;
            };
            if !seen_in_file.insert((href.clone(), target.clone())) {
                continue;
            }
            report.checked += 1;
            if !target_exists_for_static_host(&target) {
                report.missing.push(MissingLocalLink {
                    source: file.clone(),
                    href,
                    target,
                });
            }
        }
    }

    Ok(report)
}

/// Removes missing local document links from anchor and image-map elements.
///
/// This post-processing step keeps the visible text/content but drops `href`
/// attributes that point at files not present in the generated archive.
pub fn remove_missing_local_href_links(root: &Path) -> Result<usize> {
    let root = normalize_path(root);
    let mut files = Vec::new();
    collect_html_files(&root, &mut files)?;
    files.sort();

    let mut removed = 0;
    for file in files {
        let input = read_lossy(&file)?;
        let mut output = Vec::with_capacity(input.len());
        let removed_in_file = Rc::new(Cell::new(0));
        let removed_in_anchor = Rc::clone(&removed_in_file);
        let removed_in_area = Rc::clone(&removed_in_file);
        let settings = Settings {
            element_content_handlers: vec![
                element!("a[href]", |element| {
                    if should_remove_local_href(&root, &file, element.get_attribute("href")) {
                        element.remove_attribute("href");
                        removed_in_anchor.set(removed_in_anchor.get() + 1);
                    }
                    Ok(())
                }),
                element!("area[href]", |element| {
                    if should_remove_local_href(&root, &file, element.get_attribute("href")) {
                        element.remove_attribute("href");
                        removed_in_area.set(removed_in_area.get() + 1);
                    }
                    Ok(())
                }),
            ],
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

        let removed_in_file = removed_in_file.get();
        if removed_in_file > 0 {
            fs::write(&file, output)
                .with_context(|| format!("failed to write {}", file.display()))?;
            removed += removed_in_file;
        }
    }

    Ok(removed)
}

/// Removes missing local resource references from HTML and CSS files.
///
/// This is used after recovery has exhausted Wayback lookups, so pages do not
/// keep references to local images, scripts, stylesheets, or media files that
/// cannot exist in the output.
pub fn remove_missing_local_resource_references(root: &Path) -> Result<usize> {
    let root = normalize_path(root);
    let mut files = Vec::new();
    collect_candidate_files(&root, &mut files)?;
    files.sort();

    let mut removed = 0;
    for file in files {
        let input = read_lossy(&file)?;
        let removed_in_file = if is_html_file(&file) {
            rewrite_html_missing_resource_references(&root, &file, &input)?
        } else {
            let (output, removed_in_file) = remove_missing_css_url_references(&input, &root, &file);
            if removed_in_file > 0 {
                fs::write(&file, output)
                    .with_context(|| format!("failed to write {}", file.display()))?;
            }
            removed_in_file
        };

        removed += removed_in_file;
    }

    Ok(removed)
}

fn should_remove_local_href(root: &Path, file: &Path, href: Option<String>) -> bool {
    let Some(href) = href else {
        return false;
    };
    let Some(target) = resolve_local_reference(root, file, &href) else {
        return false;
    };
    !target_exists_for_static_host(&target)
}

fn rewrite_html_missing_resource_references(
    root: &Path,
    file: &Path,
    input: &str,
) -> Result<usize> {
    let mut output = Vec::with_capacity(input.len());
    let removed_in_file = Rc::new(Cell::new(0));
    let settings = Settings {
        element_content_handlers: vec![
            resource_attr_remover!("audio[src]", "src", root, file, removed_in_file),
            resource_attr_remover!("embed[src]", "src", root, file, removed_in_file),
            resource_attr_remover!("form[action]", "action", root, file, removed_in_file),
            resource_attr_remover!("frame[src]", "src", root, file, removed_in_file),
            resource_attr_remover!("iframe[src]", "src", root, file, removed_in_file),
            resource_attr_remover!("img[src]", "src", root, file, removed_in_file),
            resource_attr_remover!("input[src]", "src", root, file, removed_in_file),
            resource_attr_remover!("link[href]", "href", root, file, removed_in_file),
            resource_attr_remover!("object[data]", "data", root, file, removed_in_file),
            resource_attr_remover!("script[src]", "src", root, file, removed_in_file),
            resource_attr_remover!("source[src]", "src", root, file, removed_in_file),
            resource_attr_remover!("track[src]", "src", root, file, removed_in_file),
            resource_attr_remover!("video[poster]", "poster", root, file, removed_in_file),
            resource_attr_remover!("video[src]", "src", root, file, removed_in_file),
            resource_attr_remover!("*[background]", "background", root, file, removed_in_file),
            event_attr_resource_remover!("*[onclick]", "onclick", root, file, removed_in_file),
            event_attr_resource_remover!("*[onload]", "onload", root, file, removed_in_file),
            event_attr_resource_remover!(
                "*[onmousedown]",
                "onmousedown",
                root,
                file,
                removed_in_file
            ),
            event_attr_resource_remover!(
                "*[onmouseout]",
                "onmouseout",
                root,
                file,
                removed_in_file
            ),
            event_attr_resource_remover!(
                "*[onmouseover]",
                "onmouseover",
                root,
                file,
                removed_in_file
            ),
            event_attr_resource_remover!("*[onmouseup]", "onmouseup", root, file, removed_in_file),
            element!("*[style]", {
                let removed_in_file = Rc::clone(&removed_in_file);
                move |element| {
                    if let Some(value) = element.get_attribute("style") {
                        let (rewritten, removed) =
                            remove_missing_css_url_references(&value, root, file);
                        if removed > 0 {
                            element.set_attribute("style", &rewritten).ok();
                            removed_in_file.set(removed_in_file.get() + removed);
                        }
                    }
                    Ok(())
                }
            }),
            text!("style", {
                let removed_in_file = Rc::clone(&removed_in_file);
                move |chunk| {
                    let (rewritten, removed) =
                        remove_missing_css_url_references(chunk.as_str(), root, file);
                    if removed > 0 {
                        chunk.replace(&rewritten, lol_html::html_content::ContentType::Text);
                        removed_in_file.set(removed_in_file.get() + removed);
                    }
                    Ok(())
                }
            }),
            text!("script", {
                let removed_in_file = Rc::clone(&removed_in_file);
                move |chunk| {
                    let (rewritten, removed) =
                        remove_missing_javascript_string_references(chunk.as_str(), root, file);
                    if removed > 0 {
                        chunk.replace(&rewritten, lol_html::html_content::ContentType::Text);
                        removed_in_file.set(removed_in_file.get() + removed);
                    }
                    Ok(())
                }
            }),
        ],
        ..Settings::default()
    };

    let mut rewriter = HtmlRewriter::new(settings, |chunk: &[u8]| output.extend_from_slice(chunk));
    rewriter
        .write(input.as_bytes())
        .with_context(|| format!("failed to rewrite {}", file.display()))?;
    rewriter
        .end()
        .with_context(|| format!("failed to finish rewriting {}", file.display()))?;

    let removed = removed_in_file.get();
    if removed > 0 {
        fs::write(file, output).with_context(|| format!("failed to write {}", file.display()))?;
    }

    Ok(removed)
}

fn should_remove_missing_local_reference(root: &Path, file: &Path, href: Option<String>) -> bool {
    let Some(href) = href else {
        return false;
    };
    let Some(target) = resolve_local_reference(root, file, &href) else {
        return false;
    };
    !target_exists_for_static_host(&target)
}

fn remove_missing_css_url_references(input: &str, root: &Path, file: &Path) -> (String, usize) {
    let lower = input.to_ascii_lowercase();
    let mut output = String::with_capacity(input.len());
    let mut offset = 0;
    let mut removed = 0;

    while let Some(relative_start) = lower[offset..].find("url(") {
        let value_start = offset + relative_start + 4;
        let Some(relative_end) = input[value_start..].find(')') else {
            break;
        };
        let value_end = value_start + relative_end;
        let raw_url = &input[value_start..value_end];
        output.push_str(&input[offset..value_start]);
        if should_remove_missing_local_reference(root, file, Some(trim_css_url(raw_url).to_owned()))
        {
            output.push_str("\"\"");
            removed += 1;
        } else {
            output.push_str(raw_url);
        }
        output.push(')');
        offset = value_end + 1;
    }

    output.push_str(&input[offset..]);
    (output, removed)
}

fn remove_missing_javascript_string_references(
    input: &str,
    root: &Path,
    file: &Path,
) -> (String, usize) {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    let mut index = 0;
    let mut removed = 0;

    while index < bytes.len() {
        let quote = bytes[index];
        if quote != b'\'' && quote != b'"' {
            index += 1;
            continue;
        }

        let Some(end) = find_javascript_string_end(bytes, quote, index + 1) else {
            break;
        };
        let value = &input[index + 1..end];
        if !value.contains('\\')
            && looks_like_url_reference_or_file(value)
            && should_remove_missing_local_reference(root, file, Some(value.to_owned()))
        {
            output.push_str(&input[cursor..index + 1]);
            output.push(quote as char);
            cursor = end + 1;
            removed += 1;
        }

        index = end + 1;
    }

    output.push_str(&input[cursor..]);
    (output, removed)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct HtmlReferenceReport {
    references: Vec<String>,
    missing_image_sources: Vec<String>,
}

fn extract_html_references(input: &str) -> Result<HtmlReferenceReport> {
    let references = Rc::new(RefCell::new(Vec::new()));
    let missing_image_sources = Rc::new(RefCell::new(Vec::new()));
    {
        let script_text_references = Rc::clone(&references);
        let style_attr_references = Rc::clone(&references);
        let style_text_references = Rc::clone(&references);
        let missing_image_sources_handler = Rc::clone(&missing_image_sources);
        let settings = Settings {
            element_content_handlers: vec![
                collect_attr!("*[href]", "href", references),
                collect_attr!("*[src]", "src", references),
                collect_attr!("blockquote[cite]", "cite", references),
                collect_attr!("q[cite]", "cite", references),
                collect_attr!("del[cite]", "cite", references),
                collect_attr!("ins[cite]", "cite", references),
                collect_attr!("form[action]", "action", references),
                collect_attr!("object[data]", "data", references),
                collect_guarded_attr!("object[codebase]", "codebase", references),
                collect_guarded_attr!("applet[code]", "code", references),
                collect_guarded_attr!("option[value]", "value", references),
                collect_guarded_attr!("param[value]", "value", references),
                collect_attr!("video[poster]", "poster", references),
                collect_attr!("*[background]", "background", references),
                collect_srcset_attr!("*[srcset]", "srcset", references),
                element!("applet[archive]", {
                    let references = Rc::clone(&references);
                    move |element| {
                        if let Some(value) = element.get_attribute("archive") {
                            references.borrow_mut().extend(
                                value
                                    .split(',')
                                    .map(str::trim)
                                    .filter(|value| looks_like_url_reference_or_file(value))
                                    .map(str::to_owned),
                            );
                        }
                        Ok(())
                    }
                }),
                element!("meta[http-equiv][content]", {
                    let references = Rc::clone(&references);
                    move |element| {
                        if element
                            .get_attribute("http-equiv")
                            .is_some_and(|value| value.trim().eq_ignore_ascii_case("refresh"))
                            && let Some(value) = element.get_attribute("content")
                            && let Some(reference) = extract_meta_refresh_reference(&value)
                        {
                            references.borrow_mut().push(reference);
                        }
                        Ok(())
                    }
                }),
                collect_javascript_attr!("*[onclick]", "onclick", references),
                collect_javascript_attr!("*[onfocus]", "onfocus", references),
                collect_javascript_attr!("*[onblur]", "onblur", references),
                collect_javascript_attr!("*[onchange]", "onchange", references),
                collect_javascript_attr!("*[onload]", "onload", references),
                collect_javascript_attr!("*[onmousedown]", "onmousedown", references),
                collect_javascript_attr!("*[onmouseout]", "onmouseout", references),
                collect_javascript_attr!("*[onmouseover]", "onmouseover", references),
                collect_javascript_attr!("*[onmouseup]", "onmouseup", references),
                element!("img", move |element| {
                    if element
                        .get_attribute("src")
                        .is_none_or(|value| value.trim().is_empty())
                        && element
                            .get_attribute("srcset")
                            .is_none_or(|value| value.trim().is_empty())
                    {
                        missing_image_sources_handler
                            .borrow_mut()
                            .push(describe_img_without_source(element));
                    }
                    Ok(())
                }),
                element!("*[style]", move |element| {
                    if let Some(value) = element.get_attribute("style") {
                        style_attr_references
                            .borrow_mut()
                            .extend(extract_css_url_references(&value));
                    }
                    Ok(())
                }),
                text!("style", move |chunk| {
                    style_text_references
                        .borrow_mut()
                        .extend(extract_css_url_references(chunk.as_str()));
                    Ok(())
                }),
                text!("script", move |chunk| {
                    script_text_references
                        .borrow_mut()
                        .extend(extract_javascript_string_references(chunk.as_str()));
                    Ok(())
                }),
            ],
            ..Settings::default()
        };

        let mut rewriter = HtmlRewriter::new(settings, |_chunk: &[u8]| {});
        rewriter.write(input.as_bytes())?;
        rewriter.end()?;
    }

    let references = Rc::try_unwrap(references)
        .map_err(|_| anyhow!("HTML reference collector still has outstanding references"))
        .map(RefCell::into_inner)?;
    let missing_image_sources = Rc::try_unwrap(missing_image_sources)
        .map_err(|_| anyhow!("HTML image-source collector still has outstanding references"))
        .map(RefCell::into_inner)?;

    Ok(HtmlReferenceReport {
        references,
        missing_image_sources,
    })
}

fn describe_img_without_source(element: &lol_html::html_content::Element<'_, '_>) -> String {
    let mut parts = Vec::new();
    for attr in ["id", "name", "alt", "width", "height", "class"] {
        if let Some(value) = element.get_attribute(attr) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                parts.push(format!("{attr}={trimmed:?}"));
            }
        }
    }
    if parts.is_empty() {
        "img".to_owned()
    } else {
        format!("img {}", parts.join(" "))
    }
}

fn extract_meta_refresh_reference(input: &str) -> Option<String> {
    let lower = input.to_ascii_lowercase();
    let url_marker = lower.find("url=")?;
    let value_start = url_marker + 4;
    let leading_ws = input[value_start..]
        .chars()
        .take_while(|character| character.is_whitespace())
        .map(char::len_utf8)
        .sum::<usize>();
    let value_start = value_start + leading_ws;
    let rest = &input[value_start..];
    if let Some(quote) = rest.chars().next().filter(|c| *c == '\'' || *c == '"') {
        let quoted_start = value_start + quote.len_utf8();
        let relative_end = input[quoted_start..].find(quote)?;
        Some(input[quoted_start..quoted_start + relative_end].to_owned())
    } else {
        let end = rest
            .find(|character: char| character == ';' || character.is_whitespace())
            .unwrap_or(rest.len());
        (!rest[..end].is_empty()).then(|| rest[..end].to_owned())
    }
}

fn extract_css_url_references(input: &str) -> Vec<String> {
    let lower = input.to_ascii_lowercase();
    let mut references = Vec::new();
    let mut offset = 0;

    while let Some(relative_start) = lower[offset..].find("url(") {
        let value_start = offset + relative_start + 4;
        let Some(relative_end) = input[value_start..].find(')') else {
            break;
        };
        let value_end = value_start + relative_end;
        references.push(trim_css_url(&input[value_start..value_end]).to_owned());
        offset = value_end + 1;
    }

    references
}

fn parse_srcset_references(input: &str) -> Vec<String> {
    input
        .split(',')
        .filter_map(|candidate| candidate.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

fn extract_javascript_string_references(input: &str) -> Vec<String> {
    let bytes = input.as_bytes();
    let mut references = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        let quote = bytes[index];
        if quote != b'\'' && quote != b'"' {
            index += 1;
            continue;
        }

        let Some(end) = find_javascript_string_end(bytes, quote, index + 1) else {
            break;
        };
        let value = &input[index + 1..end];
        if looks_like_url_reference_or_file(value) {
            references.push(value.to_owned());
        }
        index = end + 1;
    }

    references
}

fn find_javascript_string_end(bytes: &[u8], quote: u8, start: usize) -> Option<usize> {
    let mut escaped = false;
    for (offset, byte) in bytes[start..].iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }

        if *byte == b'\\' {
            escaped = true;
            continue;
        }

        if *byte == quote {
            return Some(start + offset);
        }
    }

    None
}

fn looks_like_url_reference_or_file(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty()
        && trimmed == value
        && !trimmed.contains('\\')
        && !trimmed.contains(char::is_whitespace)
        && !should_skip_reference(trimmed)
        && !is_dynamic_action_reference(trimmed)
        && ((trimmed.starts_with('/') && !trimmed.starts_with("//"))
            || trimmed.starts_with("./")
            || trimmed.starts_with("../")
            || trimmed.starts_with("http://")
            || trimmed.starts_with("https://")
            || looks_like_file_reference(trimmed))
}

fn is_dynamic_action_reference(value: &str) -> bool {
    let path = strip_query_and_fragment(value).to_ascii_lowercase();
    path == "/cgi-bin" || path.starts_with("/cgi-bin/") || path.contains("/cgi-bin/")
}

fn looks_like_file_reference(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed != value
        || trimmed.contains('\\')
        || trimmed.contains(char::is_whitespace)
        || should_skip_reference(trimmed)
    {
        return false;
    }

    let path = strip_query_and_fragment(trimmed);
    let Some(file_name) = path.rsplit('/').next() else {
        return false;
    };
    let Some((_, extension)) = file_name.rsplit_once('.') else {
        return false;
    };
    is_common_reference_extension(extension)
}

fn is_common_reference_extension(extension: &str) -> bool {
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "7z" | "avi"
            | "bmp"
            | "class"
            | "css"
            | "exe"
            | "gif"
            | "gz"
            | "htm"
            | "html"
            | "ico"
            | "jar"
            | "jpeg"
            | "jpg"
            | "js"
            | "mid"
            | "midi"
            | "mov"
            | "mp3"
            | "mp4"
            | "msi"
            | "pdf"
            | "png"
            | "rar"
            | "svg"
            | "swf"
            | "tar"
            | "tgz"
            | "txt"
            | "wav"
            | "webp"
            | "woff"
            | "woff2"
            | "zip"
    )
}

fn collect_candidate_files(root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let entry = entry.with_context(|| format!("failed to read entry in {}", root.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to read file type for {}", entry.path().display()))?;
        let path = entry.path();

        if file_type.is_dir() {
            collect_candidate_files(&path, files)?;
        } else if file_type.is_file() && is_candidate_file(&path) {
            files.push(path);
        }
    }
    Ok(())
}

fn collect_html_files(root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let entry = entry.with_context(|| format!("failed to read entry in {}", root.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to read file type for {}", entry.path().display()))?;
        let path = entry.path();

        if file_type.is_dir() {
            collect_html_files(&path, files)?;
        } else if file_type.is_file() && is_html_file(&path) {
            files.push(path);
        }
    }
    Ok(())
}

fn resolve_local_reference(root: &Path, from_file: &Path, href: &str) -> Option<PathBuf> {
    let trimmed = href.trim();
    if trimmed.is_empty() || should_skip_reference(trimmed) || Url::parse(trimmed).is_ok() {
        return None;
    }

    let path = strip_query_and_fragment(trimmed);
    if path.is_empty() {
        return None;
    }

    let decoded = percent_decode(path);
    let target = if decoded.starts_with('/') {
        root.join(decoded.trim_start_matches('/'))
    } else {
        from_file.parent().unwrap_or(root).join(decoded)
    };
    let target = normalize_path(&target);
    if target.starts_with(root) {
        Some(target)
    } else {
        None
    }
}

fn target_exists_for_static_host(target: &Path) -> bool {
    target.is_file() || target.join("index.html").is_file() || target.join("index.htm").is_file()
}

fn strip_query_and_fragment(value: &str) -> &str {
    let query = value.find('?');
    let fragment = value.find('#');
    match (query, fragment) {
        (Some(query), Some(fragment)) => &value[..query.min(fragment)],
        (Some(index), None) | (None, Some(index)) => &value[..index],
        (None, None) => value,
    }
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

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
        }
    }
    normalized
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

fn trim_css_url(raw_url: &str) -> &str {
    let trimmed = raw_url.trim();
    if trimmed.len() >= 2 {
        let first = trimmed.as_bytes()[0] as char;
        let last = trimmed.as_bytes()[trimmed.len() - 1] as char;
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            return &trimmed[1..trimmed.len() - 1];
        }
    }
    if trimmed.len() >= 12 && trimmed.starts_with("&quot;") && trimmed.ends_with("&quot;") {
        return &trimmed[6..trimmed.len() - 6];
    }
    if trimmed.len() >= 10 && trimmed.starts_with("&#39;") && trimmed.ends_with("&#39;") {
        return &trimmed[5..trimmed.len() - 5];
    }
    if trimmed.len() >= 12 && trimmed.starts_with("&#x27;") && trimmed.ends_with("&#x27;") {
        return &trimmed[6..trimmed.len() - 6];
    }
    trimmed
}

fn read_lossy(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn is_candidate_file(path: &Path) -> bool {
    is_html_file(path) || is_css_file(path)
}

fn is_html_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("html")
                || extension.eq_ignore_ascii_case("htm")
                || extension.eq_ignore_ascii_case("xhtml")
        })
}

fn is_css_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("css"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_existing_local_links() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("about")).unwrap();
        fs::create_dir_all(root.join("img")).unwrap();
        fs::write(
            root.join("index.html"),
            r#"<a href="about/">About</a><img src="img/logo.png"><link href="style.css" rel="stylesheet">"#,
        )
        .unwrap();
        fs::write(root.join("about/index.html"), "about").unwrap();
        fs::write(root.join("img/logo.png"), "png").unwrap();
        fs::write(root.join("style.css"), "body { color: black; }").unwrap();

        let report = validate_local_links(root).unwrap();

        assert_eq!(report.checked, 3);
        assert!(report.missing.is_empty());
    }

    #[test]
    fn reports_missing_local_links_and_ignores_external_links() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(
            root.join("index.html"),
            r##"<a href="missing.html#topic">Missing</a><img src="https://example.com/logo.png"><a href="#top">Top</a>"##,
        )
        .unwrap();

        let report = validate_local_links(root).unwrap();

        assert_eq!(report.checked, 1);
        assert_eq!(report.missing.len(), 1);
        assert_eq!(report.missing[0].href, "missing.html#topic");
        assert_eq!(report.missing[0].target, root.join("missing.html"));
    }

    #[test]
    fn validates_css_url_references() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("img")).unwrap();
        fs::write(
            root.join("style.css"),
            r#"body { background: URL("img/bg.png"); }"#,
        )
        .unwrap();

        let report = validate_local_links(root).unwrap();

        assert_eq!(report.checked, 1);
        assert_eq!(report.missing.len(), 1);
        assert_eq!(report.missing[0].href, "img/bg.png");

        fs::write(root.join("img/bg.png"), "png").unwrap();
        let report = validate_local_links(root).unwrap();
        assert_eq!(report.checked, 1);
        assert!(report.missing.is_empty());
    }

    #[test]
    fn validates_root_relative_and_percent_decoded_links() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("docs/page one.html"), "page").unwrap();
        fs::write(
            root.join("index.html"),
            r#"<a href="/docs/page%20one.html">Page</a>"#,
        )
        .unwrap();

        let report = validate_local_links(root).unwrap();

        assert_eq!(report.checked, 1);
        assert!(report.missing.is_empty());
    }

    #[test]
    fn validates_dropdown_srcset_meta_refresh_and_legacy_references() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("pc/game")).unwrap();
        fs::create_dir_all(root.join("img")).unwrap();
        fs::create_dir_all(root.join("java")).unwrap();
        fs::write(root.join("pc/game/index.htm"), "game").unwrap();
        fs::write(root.join("about.html"), "about").unwrap();
        fs::write(root.join("img/small.png"), "small").unwrap();
        fs::write(root.join("img/large.png"), "large").unwrap();
        fs::write(root.join("movie.swf"), "movie").unwrap();
        fs::write(root.join("java/game.jar"), "jar").unwrap();
        fs::write(
            root.join("index.html"),
            r#"<select><option value="/pc/game/index.htm">Game</option><option value="Action">Action</option></select><img srcset="img/small.png 1x, img/large.png 2x"><meta http-equiv="refresh" content="0; url=about.html"><param value="movie.swf"><applet archive="java/game.jar"></applet>"#,
        )
        .unwrap();

        let report = validate_local_links(root).unwrap();

        assert_eq!(report.checked, 6);
        assert!(report.missing.is_empty(), "{:?}", report.missing);
        assert!(report.missing_image_sources.is_empty());
    }

    #[test]
    fn reports_images_without_sources() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(
            root.join("index.html"),
            r#"<img name="webgames" alt="Web games"><img src="ok.gif">"#,
        )
        .unwrap();
        fs::write(root.join("ok.gif"), "gif").unwrap();

        let report = validate_local_links(root).unwrap();

        assert_eq!(report.checked, 1);
        assert!(report.missing.is_empty());
        assert_eq!(report.missing_image_sources.len(), 1);
        assert_eq!(
            report.missing_image_sources[0].source,
            root.join("index.html")
        );
        assert!(
            report.missing_image_sources[0]
                .descriptor
                .contains(r#"name="webgames""#)
        );
    }

    #[test]
    fn validates_javascript_event_image_references() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(
            root.join("index.html"),
            r#"<a onmouseover="swap('button', 'images/button_on.gif')">Button</a>"#,
        )
        .unwrap();

        let report = validate_local_links(root).unwrap();

        assert_eq!(report.checked, 1);
        assert_eq!(report.missing.len(), 1);
        assert_eq!(report.missing[0].href, "images/button_on.gif");
    }

    #[test]
    fn validates_script_string_references_without_treating_text_as_links() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("images")).unwrap();
        fs::write(root.join("images/button.gif"), "gif").unwrap();
        fs::write(
            root.join("index.html"),
            r#"<script>var ok = "images/button.gif"; var text = "SmallRockets.com";</script>"#,
        )
        .unwrap();

        let report = validate_local_links(root).unwrap();

        assert_eq!(report.checked, 1);
        assert!(report.missing.is_empty());
    }

    #[test]
    fn ignores_javascript_dynamic_action_paths_without_static_extensions() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(
            root.join("index.html"),
            r#"<script>location.href = "/cgi-bin/login2.cgi?action=signin";</script>"#,
        )
        .unwrap();

        let report = validate_local_links(root).unwrap();

        assert_eq!(report.checked, 0);
        assert!(report.missing.is_empty());
    }

    #[test]
    fn removes_missing_local_anchor_hrefs_after_alias_repair() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(
            root.join("index.html"),
            r##"<a href="user/index.htm">Account</a><a href="https://example.com/">External</a><a href="#top">Top</a>"##,
        )
        .unwrap();

        let removed = remove_missing_local_href_links(root).unwrap();

        assert_eq!(removed, 1);
        assert_eq!(
            fs::read_to_string(root.join("index.html")).unwrap(),
            r##"<a>Account</a><a href="https://example.com/">External</a><a href="#top">Top</a>"##
        );
    }

    #[test]
    fn keeps_existing_local_anchor_hrefs() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("user")).unwrap();
        fs::write(root.join("user/index.htm"), "account").unwrap();
        fs::write(
            root.join("index.html"),
            r#"<a href="user/index.htm">Account</a>"#,
        )
        .unwrap();

        let removed = remove_missing_local_href_links(root).unwrap();

        assert_eq!(removed, 0);
        assert_eq!(
            fs::read_to_string(root.join("index.html")).unwrap(),
            r#"<a href="user/index.htm">Account</a>"#
        );
    }

    #[test]
    fn removes_missing_local_resource_references_after_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("images")).unwrap();
        fs::write(root.join("images/ok.gif"), "gif").unwrap();
        fs::write(
            root.join("index.html"),
            r#"<img src="images/missing.gif" alt="missing"><img src="images/ok.gif"><script src="missing.js"></script><link rel="stylesheet" href="missing.css">"#,
        )
        .unwrap();

        let removed = remove_missing_local_resource_references(root).unwrap();

        assert_eq!(removed, 3);
        let output = fs::read_to_string(root.join("index.html")).unwrap();
        assert!(!output.contains("images/missing.gif"));
        assert!(!output.contains("missing.js"));
        assert!(!output.contains("missing.css"));
        assert!(output.contains("images/ok.gif"));
        let report = validate_local_links(root).unwrap();
        assert!(report.missing.is_empty(), "{:?}", report.missing);
    }

    #[test]
    fn removes_missing_css_and_event_resource_references() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("images")).unwrap();
        fs::write(root.join("images/ok.gif"), "gif").unwrap();
        fs::write(
            root.join("index.html"),
            r#"<a onmouseover="swap('button','images/missing_on.gif')" style="background: url(images/missing_bg.gif)">Button</a>"#,
        )
        .unwrap();
        fs::write(
            root.join("style.css"),
            r#".missing { background: URL("images/missing_css.gif"); } .ok { background: url("images/ok.gif"); }"#,
        )
        .unwrap();

        let removed = remove_missing_local_resource_references(root).unwrap();

        assert_eq!(removed, 3);
        let report = validate_local_links(root).unwrap();
        assert!(report.missing.is_empty(), "{:?}", report.missing);
        assert!(
            fs::read_to_string(root.join("style.css"))
                .unwrap()
                .contains(r#"url("images/ok.gif")"#)
        );
    }
}
