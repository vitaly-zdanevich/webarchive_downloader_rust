use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::{Context, Result};
use lol_html::html_content::{ContentType, Element};
use lol_html::{HtmlRewriter, Settings, element, end_tag, text};
use url::Url;

use crate::download_refs::{is_downloadable_file_url, is_extra_file_url};
use crate::noise::is_archive_noise_reference;
use crate::pathmap::{
    SiteMapper, canonical_query_without_volatile_params, normalize_lookup_url, relative_link,
};

macro_rules! attr_rewriter {
    ($selector:literal, $attr:literal, $context:ident, $kind:expr) => {
        element!($selector, move |element| {
            rewrite_attr_url(element, $attr, $context, $kind);
            Ok(())
        })
    };
}

macro_rules! guarded_attr_rewriter {
    ($selector:literal, $attr:literal, $context:ident, $kind:expr) => {
        element!($selector, move |element| {
            rewrite_attr_url_if_reference_like(element, $attr, $context, $kind);
            Ok(())
        })
    };
}

macro_rules! js_attr_rewriter {
    ($selector:literal, $attr:literal, $context:ident) => {
        element!($selector, move |element| {
            rewrite_attr_javascript_urls(element, $attr, $context);
            Ok(())
        })
    };
}

/// Context needed to rewrite archived page references into local archive paths.
#[derive(Clone, Debug)]
pub struct RewriteContext<'a> {
    current_original: Url,
    current_local_path: PathBuf,
    known_paths: &'a HashMap<String, PathBuf>,
    mapper: Option<&'a SiteMapper>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Result of attempting to convert an archived URL reference to a local path.
pub enum UrlRewrite {
    /// Replace the original reference with the contained local relative path.
    Rewrite(String),
    /// Remove the reference because it is known noise or an unsupported external file.
    Suppress,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReferenceKind {
    Document,
    Resource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RolloverImageSource {
    name: String,
    src: String,
}

impl<'a> RewriteContext<'a> {
    /// Builds a rewriting context for one archived document or stylesheet.
    ///
    /// `current_original` is the original archived URL of the file being rewritten,
    /// `current_local_path` is that file's local output path, and `known_paths`
    /// maps normalized original URLs to selected local archive paths.
    pub fn new(
        current_original: &str,
        current_local_path: PathBuf,
        known_paths: &'a HashMap<String, PathBuf>,
    ) -> Result<Self> {
        Ok(Self {
            current_original: Url::parse(current_original)
                .with_context(|| format!("invalid original URL: {current_original}"))?,
            current_local_path,
            known_paths,
            mapper: None,
        })
    }

    /// Builds a rewriting context that can also map related subdomains.
    ///
    /// The mapper is used for explicit related-host downloads such as archived
    /// binaries or static assets hosted on `downloads.example.com`.
    pub fn new_with_mapper(
        current_original: &str,
        current_local_path: PathBuf,
        known_paths: &'a HashMap<String, PathBuf>,
        mapper: &'a SiteMapper,
    ) -> Result<Self> {
        Ok(Self {
            current_original: Url::parse(current_original)
                .with_context(|| format!("invalid original URL: {current_original}"))?,
            current_local_path,
            known_paths,
            mapper: Some(mapper),
        })
    }

    /// Rewrites one URL-like resource reference relative to the current local file.
    ///
    /// This is intended for resources by default. Document links should use the
    /// internal document-aware path handling so extensionless pages map to
    /// `index.html`.
    pub fn rewrite_url_reference(&self, value: &str) -> Option<UrlRewrite> {
        self.rewrite_url_reference_as(value, ReferenceKind::Resource)
    }

    fn rewrite_url_reference_as(&self, value: &str, kind: ReferenceKind) -> Option<UrlRewrite> {
        let trimmed = value.trim();
        if trimmed.is_empty() || should_skip_reference(trimmed) {
            return None;
        }

        let (without_fragment, fragment) = split_fragment(trimmed);
        let archive_unwrapped = unwrap_wayback_url(without_fragment);
        let resolved = self.current_original.join(&archive_unwrapped).ok()?;
        let mut lookup_url = resolved.clone();
        lookup_url.set_fragment(None);
        let lookup_key = normalize_lookup_url(lookup_url.as_str());
        let local_path = if let Some(local_path) = self.known_paths.get(&lookup_key).cloned() {
            local_path
        } else if is_archive_noise_reference(lookup_url.as_str()) {
            return Some(UrlRewrite::Suppress);
        } else if kind == ReferenceKind::Document && is_downloadable_file_url(&lookup_url) {
            match self.downloadable_fallback_path(&lookup_url) {
                Some(local_path) => local_path,
                None => return Some(UrlRewrite::Suppress),
            }
        } else if is_extra_file_url(&lookup_url) {
            self.related_resource_fallback_path(&lookup_url, kind)?
        } else {
            self.fallback_same_site_path(&lookup_url, kind)?
        };

        let mut rewritten = relative_link(&self.current_local_path, &local_path);
        if let Some(fragment) = fragment {
            rewritten.push('#');
            rewritten.push_str(fragment);
        }
        Some(UrlRewrite::Rewrite(rewritten))
    }

    fn fallback_same_site_path(&self, url: &Url, kind: ReferenceKind) -> Option<PathBuf> {
        if !hosts_are_same_site(self.current_original.host_str()?, url.host_str()?) {
            return None;
        }

        let mut path = match kind {
            ReferenceKind::Document => document_path_from_url(url),
            ReferenceKind::Resource => path_from_url_without_mimetype(url),
        };
        if let Some(query) = canonical_query_without_volatile_params(url) {
            append_query_hash(&mut path, &query);
        }
        Some(path)
    }

    fn downloadable_fallback_path(&self, url: &Url) -> Option<PathBuf> {
        let mapper = self.mapper?;
        let host = url.host_str()?;
        if mapper.is_related_host(host) {
            mapper
                .local_path_for_url(url.as_str(), "application/octet-stream")
                .ok()
        } else {
            None
        }
    }

    fn related_resource_fallback_path(&self, url: &Url, kind: ReferenceKind) -> Option<PathBuf> {
        let host = url.host_str()?;
        if hosts_are_same_site(self.current_original.host_str()?, host) {
            return self.fallback_same_site_path(url, kind);
        }
        let mapper = self.mapper?;
        if !mapper.is_related_host(host) {
            return None;
        }
        mapper
            .local_path_for_url(url.as_str(), "application/octet-stream")
            .ok()
    }
}

/// Rewrites localizable references in one HTML document.
///
/// The pass covers ordinary URL attributes, CSS in style blocks and attributes,
/// JavaScript string references, old rollover image handlers, `srcset`,
/// `meta refresh`, and legacy `APPLET`/`PARAM` patterns common in older sites.
pub fn rewrite_html(input: &str, context: &RewriteContext<'_>) -> Result<String> {
    let mut output = Vec::with_capacity(input.len());
    let rollover_stack = Rc::new(RefCell::new(Vec::<Vec<RolloverImageSource>>::new()));
    let settings = Settings {
        element_content_handlers: vec![
            element!("*", {
                let rollover_stack = Rc::clone(&rollover_stack);
                move |element| {
                    let sources = extract_rollover_image_sources(element, context);
                    if !sources.is_empty() && element.can_have_content() {
                        rollover_stack.borrow_mut().push(sources);
                        let rollover_stack = Rc::clone(&rollover_stack);
                        element.on_end_tag(end_tag!(move |_| {
                            rollover_stack.borrow_mut().pop();
                            Ok(())
                        }))?;
                    }
                    Ok(())
                }
            }),
            element!("img", {
                let rollover_stack = Rc::clone(&rollover_stack);
                move |element| {
                    infer_missing_img_src_from_rollover_stack(element, &rollover_stack);
                    Ok(())
                }
            }),
            attr_rewriter!("a[href]", "href", context, ReferenceKind::Document),
            attr_rewriter!("blockquote[cite]", "cite", context, ReferenceKind::Document),
            attr_rewriter!("q[cite]", "cite", context, ReferenceKind::Document),
            attr_rewriter!("del[cite]", "cite", context, ReferenceKind::Document),
            attr_rewriter!("ins[cite]", "cite", context, ReferenceKind::Document),
            attr_rewriter!("area[href]", "href", context, ReferenceKind::Document),
            attr_rewriter!("audio[src]", "src", context, ReferenceKind::Resource),
            attr_rewriter!("embed[src]", "src", context, ReferenceKind::Resource),
            attr_rewriter!("form[action]", "action", context, ReferenceKind::Document),
            attr_rewriter!("frame[src]", "src", context, ReferenceKind::Document),
            attr_rewriter!("iframe[src]", "src", context, ReferenceKind::Document),
            attr_rewriter!("img[src]", "src", context, ReferenceKind::Resource),
            attr_rewriter!("input[src]", "src", context, ReferenceKind::Resource),
            attr_rewriter!("link[href]", "href", context, ReferenceKind::Resource),
            attr_rewriter!("object[data]", "data", context, ReferenceKind::Resource),
            attr_rewriter!("script[src]", "src", context, ReferenceKind::Resource),
            attr_rewriter!("source[src]", "src", context, ReferenceKind::Resource),
            attr_rewriter!("track[src]", "src", context, ReferenceKind::Resource),
            attr_rewriter!("video[poster]", "poster", context, ReferenceKind::Resource),
            attr_rewriter!("video[src]", "src", context, ReferenceKind::Resource),
            guarded_attr_rewriter!("applet[code]", "code", context, ReferenceKind::Resource),
            guarded_attr_rewriter!(
                "object[codebase]",
                "codebase",
                context,
                ReferenceKind::Document
            ),
            guarded_attr_rewriter!("option[value]", "value", context, ReferenceKind::Document),
            guarded_attr_rewriter!("param[value]", "value", context, ReferenceKind::Resource),
            element!("*[srcset]", move |element| {
                rewrite_attr_srcset(element, "srcset", context);
                Ok(())
            }),
            element!("applet[archive]", move |element| {
                rewrite_attr_comma_separated_urls(
                    element,
                    "archive",
                    context,
                    ReferenceKind::Resource,
                );
                Ok(())
            }),
            attr_rewriter!(
                "*[background]",
                "background",
                context,
                ReferenceKind::Resource
            ),
            element!("meta[http-equiv][content]", move |element| {
                rewrite_meta_refresh(element, context);
                Ok(())
            }),
            element!("*[style]", move |element| {
                rewrite_attr_css(element, "style", context);
                Ok(())
            }),
            js_attr_rewriter!("*[onclick]", "onclick", context),
            js_attr_rewriter!("*[onload]", "onload", context),
            js_attr_rewriter!("*[onmousedown]", "onmousedown", context),
            js_attr_rewriter!("*[onmouseover]", "onmouseover", context),
            js_attr_rewriter!("*[onmouseout]", "onmouseout", context),
            js_attr_rewriter!("*[onmouseup]", "onmouseup", context),
            js_attr_rewriter!("*[onfocus]", "onfocus", context),
            js_attr_rewriter!("*[onblur]", "onblur", context),
            js_attr_rewriter!("*[onchange]", "onchange", context),
            text!("style", move |chunk| {
                let rewritten = rewrite_css(chunk.as_str(), context);
                chunk.replace(&rewritten, ContentType::Text);
                Ok(())
            }),
            text!("script", move |chunk| {
                let rewritten = rewrite_javascript_string_urls(chunk.as_str(), context);
                chunk.replace(&rewritten, ContentType::Text);
                Ok(())
            }),
        ],
        ..Settings::default()
    };

    let mut rewriter = HtmlRewriter::new(settings, |chunk: &[u8]| output.extend_from_slice(chunk));
    rewriter
        .write(input.as_bytes())
        .context("failed to rewrite HTML")?;
    rewriter.end().context("failed to finish HTML rewriting")?;

    String::from_utf8(output).context("rewritten HTML is not valid UTF-8")
}

/// Rewrites `url(...)` references in one CSS stylesheet or style fragment.
///
/// The parser is intentionally small but handles `url(` case-insensitively and
/// preserves the original quoting style when replacing values.
pub fn rewrite_css(input: &str, context: &RewriteContext<'_>) -> String {
    let lower = input.to_ascii_lowercase();
    let mut rewritten = String::with_capacity(input.len());
    let mut offset = 0;

    while let Some(relative_start) = lower[offset..].find("url(") {
        let value_start = offset + relative_start + 4;
        let Some(relative_end) = input[value_start..].find(')') else {
            break;
        };
        let value_end = value_start + relative_end;

        rewritten.push_str(&input[offset..value_start]);
        let raw_url = &input[value_start..value_end];
        let (quote, value) = trim_css_url(raw_url);
        match context.rewrite_url_reference(value) {
            Some(UrlRewrite::Rewrite(new_value)) => {
                if let Some(quote) = quote {
                    rewritten.push(quote);
                    rewritten.push_str(&new_value);
                    rewritten.push(quote);
                } else {
                    rewritten.push_str(&new_value);
                }
            }
            Some(UrlRewrite::Suppress) => {
                rewritten.push_str("\"\"");
            }
            None => rewritten.push_str(raw_url),
        }
        rewritten.push(')');
        offset = value_end + 1;
    }

    rewritten.push_str(&input[offset..]);
    rewritten
}

fn rewrite_attr_url(
    element: &mut Element<'_, '_>,
    attr: &str,
    context: &RewriteContext<'_>,
    kind: ReferenceKind,
) {
    let Some(value) = element.get_attribute(attr) else {
        return;
    };
    match context.rewrite_url_reference_as(&value, kind) {
        Some(UrlRewrite::Rewrite(rewritten)) => {
            element.set_attribute(attr, &rewritten).ok();
        }
        Some(UrlRewrite::Suppress) => suppress_attr_url(element, attr),
        None => {}
    }
}

/// Rewrites a guarded attribute only when it is clearly a URL or file reference.
///
/// Legacy attributes such as `option[value]` and `param[value]` often contain
/// arbitrary values, so this avoids treating labels like `Action` as paths.
fn rewrite_attr_url_if_reference_like(
    element: &mut Element<'_, '_>,
    attr: &str,
    context: &RewriteContext<'_>,
    kind: ReferenceKind,
) {
    let Some(value) = element.get_attribute(attr) else {
        return;
    };
    if !looks_like_url_reference_or_file(&value) {
        return;
    }
    match context.rewrite_url_reference_as(&value, kind) {
        Some(UrlRewrite::Rewrite(rewritten)) => {
            element.set_attribute(attr, &rewritten).ok();
        }
        Some(UrlRewrite::Suppress) => suppress_attr_url(element, attr),
        None => {}
    }
}

fn suppress_attr_url(element: &mut Element<'_, '_>, attr: &str) {
    let tag_name = element.tag_name().to_ascii_lowercase();
    if matches!(
        tag_name.as_str(),
        "audio"
            | "embed"
            | "frame"
            | "iframe"
            | "img"
            | "link"
            | "object"
            | "script"
            | "source"
            | "track"
            | "video"
    ) {
        element.remove();
    } else {
        element.remove_attribute(attr);
    }
}

fn rewrite_attr_srcset(element: &mut Element<'_, '_>, attr: &str, context: &RewriteContext<'_>) {
    let Some(value) = element.get_attribute(attr) else {
        return;
    };
    let Some(rewritten) = rewrite_srcset(&value, context) else {
        return;
    };
    if rewritten.is_empty() {
        element.remove_attribute(attr);
    } else {
        element.set_attribute(attr, &rewritten).ok();
    }
}

fn rewrite_srcset(input: &str, context: &RewriteContext<'_>) -> Option<String> {
    let mut changed = false;
    let mut candidates = Vec::new();

    for candidate in input.split(',') {
        let trimmed = candidate.trim();
        if trimmed.is_empty() {
            continue;
        }
        let url_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        let (url, descriptor) = trimmed.split_at(url_end);
        match context.rewrite_url_reference(url) {
            Some(UrlRewrite::Rewrite(rewritten)) => {
                changed = true;
                candidates.push(format!("{rewritten}{descriptor}"));
            }
            Some(UrlRewrite::Suppress) => {
                changed = true;
            }
            None => candidates.push(trimmed.to_owned()),
        }
    }

    changed.then(|| candidates.join(", "))
}

/// Rewrites comma-separated legacy URL lists such as `applet archive`.
fn rewrite_attr_comma_separated_urls(
    element: &mut Element<'_, '_>,
    attr: &str,
    context: &RewriteContext<'_>,
    kind: ReferenceKind,
) {
    let Some(value) = element.get_attribute(attr) else {
        return;
    };
    let mut changed = false;
    let mut rewritten_values = Vec::new();
    for raw_value in value.split(',') {
        let trimmed = raw_value.trim();
        if trimmed.is_empty() {
            continue;
        }
        match context.rewrite_url_reference_as(trimmed, kind) {
            Some(UrlRewrite::Rewrite(rewritten)) => {
                changed = true;
                rewritten_values.push(rewritten);
            }
            Some(UrlRewrite::Suppress) => {
                changed = true;
            }
            None => rewritten_values.push(trimmed.to_owned()),
        }
    }
    if changed {
        if rewritten_values.is_empty() {
            element.remove_attribute(attr);
        } else {
            element
                .set_attribute(attr, &rewritten_values.join(", "))
                .ok();
        }
    }
}

/// Rewrites `meta http-equiv=refresh` targets while preserving the delay prefix.
fn rewrite_meta_refresh(element: &mut Element<'_, '_>, context: &RewriteContext<'_>) {
    let Some(http_equiv) = element.get_attribute("http-equiv") else {
        return;
    };
    if !http_equiv.trim().eq_ignore_ascii_case("refresh") {
        return;
    }
    let Some(content) = element.get_attribute("content") else {
        return;
    };
    let Some(rewritten) = rewrite_meta_refresh_content(&content, context) else {
        return;
    };
    element.set_attribute("content", &rewritten).ok();
}

fn rewrite_meta_refresh_content(input: &str, context: &RewriteContext<'_>) -> Option<String> {
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
    let (quote, url, suffix) =
        if let Some(quote) = rest.chars().next().filter(|c| *c == '\'' || *c == '"') {
            let quoted_start = value_start + quote.len_utf8();
            let Some(relative_end) = input[quoted_start..].find(quote) else {
                return None;
            };
            let quoted_end = quoted_start + relative_end;
            (
                Some(quote),
                &input[quoted_start..quoted_end],
                &input[quoted_end + quote.len_utf8()..],
            )
        } else {
            let end = rest
                .find(|character: char| character == ';' || character.is_whitespace())
                .unwrap_or(rest.len());
            (None, &rest[..end], &rest[end..])
        };

    let rewritten = match context.rewrite_url_reference_as(url, ReferenceKind::Document)? {
        UrlRewrite::Rewrite(rewritten) => rewritten,
        UrlRewrite::Suppress => String::new(),
    };
    let mut output = String::with_capacity(input.len());
    output.push_str(&input[..value_start]);
    if let Some(quote) = quote {
        output.push(quote);
        output.push_str(&rewritten);
        output.push(quote);
    } else {
        output.push_str(&rewritten);
    }
    output.push_str(suffix);
    Some(output)
}

fn rewrite_attr_css(element: &mut Element<'_, '_>, attr: &str, context: &RewriteContext<'_>) {
    let Some(value) = element.get_attribute(attr) else {
        return;
    };
    let rewritten = rewrite_css(&value, context);
    if rewritten != value {
        element.set_attribute(attr, &rewritten).ok();
    }
}

fn rewrite_attr_javascript_urls(
    element: &mut Element<'_, '_>,
    attr: &str,
    context: &RewriteContext<'_>,
) {
    let Some(value) = element.get_attribute(attr) else {
        return;
    };
    let rewritten = rewrite_javascript_string_urls(&value, context);
    if rewritten != value {
        element.set_attribute(attr, &rewritten).ok();
    }
}

/// Fills missing image `src` values from surrounding old rollover markup.
///
/// Some archived pages omit `<img src>` but keep the intended image path in
/// surrounding JavaScript such as `changeImage('nav', 'nav_off.gif')`. The
/// stack tracks those surrounding handlers and fills `src` only when the image's
/// `name` or `id` matches the JavaScript target.
fn infer_missing_img_src_from_rollover_stack(
    element: &mut Element<'_, '_>,
    rollover_stack: &Rc<RefCell<Vec<Vec<RolloverImageSource>>>>,
) {
    if element.get_attribute("src").is_some() || element.get_attribute("srcset").is_some() {
        return;
    }
    let identifiers = image_identifier_candidates(element);
    if identifiers.is_empty() {
        return;
    }

    for sources in rollover_stack.borrow().iter().rev() {
        for identifier in &identifiers {
            if let Some(source) = sources
                .iter()
                .find(|source| source.name.eq_ignore_ascii_case(identifier))
            {
                element.set_attribute("src", &source.src).ok();
                return;
            }
        }
    }
}

fn image_identifier_candidates(element: &Element<'_, '_>) -> Vec<String> {
    let mut identifiers = Vec::new();
    for attr in ["name", "id"] {
        if let Some(value) = element.get_attribute(attr) {
            let trimmed = value.trim();
            if !trimmed.is_empty()
                && !identifiers
                    .iter()
                    .any(|candidate: &String| candidate.eq_ignore_ascii_case(trimmed))
            {
                identifiers.push(trimmed.to_owned());
            }
        }
    }
    identifiers
}

fn extract_rollover_image_sources(
    element: &Element<'_, '_>,
    context: &RewriteContext<'_>,
) -> Vec<RolloverImageSource> {
    let mut sources = Vec::new();
    for attr in [
        "onmouseout",
        "onmouseup",
        "onblur",
        "onmouseleave",
        "onmouseover",
        "onmousedown",
        "onfocus",
        "onmouseenter",
        "onclick",
        "onload",
    ] {
        if let Some(value) = element.get_attribute(attr) {
            sources.extend(extract_rollover_image_sources_from_javascript(
                &value, context,
            ));
        }
    }
    sources
}

fn extract_rollover_image_sources_from_javascript(
    input: &str,
    context: &RewriteContext<'_>,
) -> Vec<RolloverImageSource> {
    let strings = javascript_string_literals(input);
    let mut sources = Vec::new();
    for index in 0..strings.len() {
        let name = strings[index];
        if !looks_like_image_identifier(name) {
            continue;
        }
        let image = strings
            .get(index + 1)
            .filter(|value| looks_like_url_reference_or_file(value))
            .or_else(|| {
                strings
                    .get(index + 2)
                    .filter(|_| strings.get(index + 1).is_some_and(|value| value.is_empty()))
                    .filter(|value| looks_like_url_reference_or_file(value))
            });
        let Some(image) = image else {
            continue;
        };
        let Some(UrlRewrite::Rewrite(src)) = context.rewrite_url_reference(image) else {
            continue;
        };
        sources.push(RolloverImageSource {
            name: name.to_owned(),
            src,
        });
    }
    sources
}

fn rewrite_javascript_string_urls(input: &str, context: &RewriteContext<'_>) -> String {
    let bytes = input.as_bytes();
    let mut rewritten = String::with_capacity(input.len());
    let mut cursor = 0;
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
        let replacement = if !value.contains('\\') && looks_like_javascript_url_reference(value) {
            context
                .rewrite_url_reference(value)
                .map(url_rewrite_to_javascript_string)
        } else {
            rewrite_html_fragment_in_javascript_string(value, context)
        };

        if let Some(replacement) = replacement {
            rewritten.push_str(&input[cursor..index + 1]);
            rewritten.push_str(&replacement);
            rewritten.push(quote as char);
            cursor = end + 1;
        }

        index = end + 1;
    }

    rewritten.push_str(&input[cursor..]);
    rewritten
}

fn javascript_string_literals(input: &str) -> Vec<&str> {
    let bytes = input.as_bytes();
    let mut strings = Vec::new();
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
        if !value.contains('\\') {
            strings.push(value);
        }
        index = end + 1;
    }

    strings
}

fn url_rewrite_to_javascript_string(rewrite: UrlRewrite) -> String {
    match rewrite {
        UrlRewrite::Rewrite(value) => value,
        UrlRewrite::Suppress => String::new(),
    }
}

fn rewrite_html_fragment_in_javascript_string(
    value: &str,
    context: &RewriteContext<'_>,
) -> Option<String> {
    if !value.contains('<') || !value.contains('>') {
        return None;
    }

    let rewritten = rewrite_html(value, context).ok()?;
    (rewritten != value).then_some(rewritten)
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

fn looks_like_javascript_url_reference(value: &str) -> bool {
    looks_like_url_reference_or_file(value)
}

fn looks_like_url_reference_or_file(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty()
        && trimmed == value
        && !trimmed.contains('\\')
        && !trimmed.contains(char::is_whitespace)
        && !should_skip_reference(trimmed)
        && ((trimmed.starts_with('/') && !trimmed.starts_with("//"))
            || trimmed.starts_with("./")
            || trimmed.starts_with("../")
            || trimmed.starts_with("http://")
            || trimmed.starts_with("https://")
            || looks_like_file_reference(trimmed))
}

fn looks_like_file_reference(value: &str) -> bool {
    let path = strip_query_and_fragment(value);
    let Some(file_name) = path.rsplit('/').next() else {
        return false;
    };
    let Some((_, extension)) = file_name.rsplit_once('.') else {
        return false;
    };
    is_common_reference_extension(extension)
}

fn looks_like_image_identifier(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty()
        && trimmed == value
        && trimmed.len() <= 128
        && !trimmed.contains('/')
        && !trimmed.contains('\\')
        && !trimmed.contains(char::is_whitespace)
        && !looks_like_url_reference_or_file(trimmed)
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

fn hosts_are_same_site(left: &str, right: &str) -> bool {
    fn comparable(host: &str) -> &str {
        host.trim_end_matches('.')
            .strip_prefix("www.")
            .unwrap_or_else(|| host.trim_end_matches('.'))
    }

    comparable(left).eq_ignore_ascii_case(comparable(right))
}

fn path_from_url_without_mimetype(url: &Url) -> PathBuf {
    let mut path = PathBuf::new();
    let mut has_segments = false;

    if let Some(segments) = url.path_segments() {
        for segment in segments {
            if segment.is_empty() {
                continue;
            }
            path.push(sanitize_path_segment(segment));
            has_segments = true;
        }
    }

    if !has_segments || url.path().ends_with('/') {
        path.push("index.html");
    }

    path
}

fn document_path_from_url(url: &Url) -> PathBuf {
    let mut path = PathBuf::new();
    let mut segments = url
        .path_segments()
        .map(|segments| {
            segments
                .filter(|segment| !segment.is_empty())
                .map(sanitize_path_segment)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if segments.is_empty() || url.path().ends_with('/') {
        segments.push("index.html".to_owned());
    } else if extension_of(segments.last().unwrap()).is_none() {
        segments.push("index.html".to_owned());
    } else if !extension_of(segments.last().unwrap()).is_some_and(is_html_extension)
        && let Some(last_segment) = segments.last_mut()
    {
        last_segment.push_str(".html");
    }

    for segment in segments {
        path.push(segment);
    }
    path
}

fn extension_of(segment: &str) -> Option<&str> {
    segment.rsplit_once('.').map(|(_, extension)| extension)
}

fn is_html_extension(extension: &str) -> bool {
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "html" | "htm" | "xhtml"
    )
}

fn sanitize_path_segment(segment: &str) -> String {
    let mut cleaned = String::with_capacity(segment.len());
    for character in segment.chars() {
        match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => cleaned.push('_'),
            character if character.is_control() => cleaned.push('_'),
            character => cleaned.push(character),
        }
    }

    let cleaned = cleaned.trim_matches('.');
    if cleaned.is_empty() || cleaned == ".." {
        "_".to_owned()
    } else {
        cleaned.to_owned()
    }
}

fn append_query_hash(path: &mut PathBuf, query: &str) {
    let hash = fnv1a64(query.as_bytes());
    let Some(file_name) = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        path.push(format!("query__q_{hash:016x}"));
        return;
    };

    let (stem, extension) = match file_name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => {
            (stem.to_owned(), Some(extension.to_owned()))
        }
        _ => (file_name, None),
    };

    let new_name = match extension {
        Some(extension) => format!("{stem}__q_{hash:016x}.{extension}"),
        None => format!("{stem}__q_{hash:016x}"),
    };
    path.set_file_name(new_name);
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
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

fn split_fragment(value: &str) -> (&str, Option<&str>) {
    match value.split_once('#') {
        Some((path, fragment)) => (path, Some(fragment)),
        None => (value, None),
    }
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

fn trim_css_url(raw_url: &str) -> (Option<char>, &str) {
    let trimmed = raw_url.trim();
    if trimmed.len() >= 2 {
        let first = trimmed.as_bytes()[0] as char;
        let last = trimmed.as_bytes()[trimmed.len() - 1] as char;
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            return (Some(first), &trimmed[1..trimmed.len() - 1]);
        }
    }
    (None, trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_context<T>(test: T)
    where
        T: FnOnce(RewriteContext<'_>),
    {
        let mut known_paths = HashMap::new();
        known_paths.insert(
            "http://example.com/about".to_owned(),
            PathBuf::from("about/index.html"),
        );
        known_paths.insert(
            "http://example.com/img/logo.png".to_owned(),
            PathBuf::from("img/logo.png"),
        );
        known_paths.insert(
            "http://example.com/css/site.css".to_owned(),
            PathBuf::from("css/site.css"),
        );
        RewriteContext::new(
            "http://example.com/index.html",
            PathBuf::from("index.html"),
            &known_paths,
        )
        .map(test)
        .unwrap()
    }

    #[test]
    fn rewrites_common_html_attributes() {
        with_context(|context| {
            let rewritten = rewrite_html(
                r#"<a href="/about#team">About</a><img src="http://example.com/img/logo.png"><a href="mailto:test@example.com">Mail</a>"#,
                &context,
            )
            .unwrap();

            assert!(rewritten.contains(r#"href="about/index.html#team""#));
            assert!(rewritten.contains(r#"src="img/logo.png""#));
            assert!(rewritten.contains(r#"href="mailto:test@example.com""#));
        });
    }

    #[test]
    fn rewrites_css_url_references() {
        with_context(|context| {
            let rewritten = rewrite_css(r#".logo { background: URL('/img/logo.png'); }"#, &context);

            assert_eq!(rewritten, r#".logo { background: URL('img/logo.png'); }"#);
        });
    }

    #[test]
    fn rewrites_static_urls_in_rollover_event_attributes() {
        with_context(|context| {
            let rewritten = rewrite_html(
                r#"<a onMouseOver='changeImage("home","/img/logo.png")' onMouseOut='changeImage("home","http://example.com/img/logo.png")'><img src="/img/logo.png"></a>"#,
                &context,
            )
            .unwrap();

            assert!(rewritten.contains("img/logo.png"));
            assert!(!rewritten.contains("/img/logo.png"));
            assert!(!rewritten.contains("http://example.com/img/logo.png"));
        });
    }

    #[test]
    fn infers_missing_img_src_from_rollover_event_attributes() {
        with_context(|context| {
            let rewritten = rewrite_html(
                r#"<a onMouseOut="changeImage('webgames','/shared/bt_webgames_off.gif')"><img name="webgames" alt="Web games"></a>"#,
                &context,
            )
            .unwrap();

            assert!(rewritten.contains(r#"src="shared/bt_webgames_off.gif""#));
            assert!(!rewritten.contains(r#"src="/shared/bt_webgames_off.gif""#));
        });
    }

    #[test]
    fn rewrites_dropdown_and_onclick_document_references() {
        with_context(|context| {
            let rewritten = rewrite_html(
                r#"<select><option value="/pc/game/index.htm">Game</option><option value="Action">Action</option></select><a onclick="this.href='http://www.example.com/user/signin.htm?action=newcustomer&amp;email='">Account</a>"#,
                &context,
            )
            .unwrap();

            assert!(rewritten.contains(r#"value="pc/game/index.htm""#));
            assert!(rewritten.contains(r#"value="Action""#));
            assert!(rewritten.contains("this.href='user/signin"));
            assert!(!rewritten.contains("http://www.example.com"));
        });
    }

    #[test]
    fn rewrites_srcset_meta_refresh_and_legacy_resource_attributes() {
        with_context(|context| {
            let rewritten = rewrite_html(
                r#"<img srcset="/img/logo.png 1x, /img/logo@2x.png 2x"><meta http-equiv="Refresh" content="0; URL='/about'"><param name="movie" value="/movie.swf"><applet archive="/java/game.jar, helper.jar"></applet>"#,
                &context,
            )
            .unwrap();

            assert!(rewritten.contains(r#"srcset="img/logo.png 1x, img/logo@2x.png 2x""#));
            assert!(rewritten.contains(r#"content="0; URL='about/index.html'""#));
            assert!(rewritten.contains(r#"value="movie.swf""#));
            assert!(
                rewritten.contains(r#"archive="java/game.jar, helper.jar""#),
                "{rewritten}"
            );
        });
    }

    #[test]
    fn rewrites_urls_inside_script_document_write_html_strings() {
        with_context(|context| {
            let rewritten = rewrite_html(
                r#"<script>document.write('<IMG SRC="/shared/spacer.gif"><A HREF="/general/shop.htm">Shop</A>');</script>"#,
                &context,
            )
            .unwrap();

            assert!(rewritten.contains(r#"SRC="shared/spacer.gif""#));
            assert!(rewritten.contains(r#"HREF="general/shop.htm""#));
            assert!(!rewritten.contains(r#"SRC="/shared/spacer.gif""#));
            assert!(!rewritten.contains(r#"HREF="/general/shop.htm""#));
        });
    }

    #[test]
    fn falls_back_to_same_site_relative_paths_for_unknown_assets() {
        with_context(|context| {
            let rewritten = rewrite_html(
                r#"<img src="/img/missing.gif"><a href="https://www.example.com/user/index.htm">Account</a>"#,
                &context,
            )
            .unwrap();

            assert!(rewritten.contains(r#"src="img/missing.gif""#));
            assert!(rewritten.contains(r#"href="user/index.htm""#));
        });
    }

    #[test]
    fn falls_back_to_query_hashed_paths_for_unknown_same_site_urls() {
        with_context(|context| {
            let rewritten = rewrite_html(
                r#"<a href="/screenshots.htm?ScreenShot=0">First</a>"#,
                &context,
            )
            .unwrap();

            assert!(rewritten.contains(r#"href="screenshots__q_bd691851ee951d9c.htm""#));
        });
    }

    #[test]
    fn keeps_meaningful_forum_links_with_volatile_query_params() {
        with_context(|context| {
            let rewritten = rewrite_html(
                r#"<a href="/forums/viewforum.php?f=1&amp;sid=abcdef" class="forumtitle">General</a>"#,
                &context,
            )
            .unwrap();

            assert!(rewritten.contains("class=\"forumtitle\""));
            assert!(rewritten.contains("href=\"forums/viewforum.php__q_"));
            assert!(rewritten.contains(".html\""));
            assert!(!rewritten.contains("sid=abcdef"));
            assert!(!rewritten.contains("<a class=\"forumtitle\""));
        });
    }

    #[test]
    fn unwraps_wayback_links_before_rewriting() {
        with_context(|context| {
            let rewritten = rewrite_html(
                r#"<link href="https://web.archive.org/web/20200101000000id_/http://example.com/css/site.css" rel="stylesheet">"#,
                &context,
            )
            .unwrap();

            assert!(rewritten.contains(r#"href="css/site.css""#));
        });
    }

    #[test]
    fn suppresses_noise_references_without_fallback_paths() {
        with_context(|context| {
            let rewritten = rewrite_html(
                r#"<a href="/forums/login.php">Login</a><img src="/forums/cron.php" width="1" height="1" alt="cron"><img src="/img/logo.png?sid=abcdef">"#,
                &context,
            )
            .unwrap();

            assert!(rewritten.contains("<a>Login</a>"));
            assert!(!rewritten.contains("cron.php"));
            assert!(rewritten.contains(r#"src="img/logo.png""#));
        });
    }

    #[test]
    fn suppresses_unresolved_download_links() {
        with_context(|context| {
            let rewritten = rewrite_html(
                r#"<a href="http://downloads.example.com/file.exe" target="download">Download</a>"#,
                &context,
            )
            .unwrap();

            assert_eq!(rewritten, r#"<a target="download">Download</a>"#);
        });
    }

    #[test]
    fn maps_related_uncaptured_download_links_when_mapper_is_available() {
        let known_paths = HashMap::new();
        let mapper = SiteMapper::new("example.com").unwrap();
        let context = RewriteContext::new_with_mapper(
            "http://example.com/downloads.html",
            PathBuf::from("downloads.html"),
            &known_paths,
            &mapper,
        )
        .unwrap();

        let rewritten = rewrite_html(
            r#"<a href="http://downloads.example.com/file.exe">Download</a>"#,
            &context,
        )
        .unwrap();

        assert_eq!(
            rewritten,
            r#"<a href="_hosts/downloads.example.com/file.exe">Download</a>"#
        );
    }

    #[test]
    fn maps_related_uncaptured_static_assets_when_mapper_is_available() {
        let known_paths = HashMap::new();
        let mapper = SiteMapper::new("example.com").unwrap();
        let context = RewriteContext::new_with_mapper(
            "http://example.com/forums/topic.html",
            PathBuf::from("forums/topic.html"),
            &known_paths,
            &mapper,
        )
        .unwrap();

        let rewritten = rewrite_html(
            r#"<img src="http://downloads.example.com/files/preview/shot.jpg">"#,
            &context,
        )
        .unwrap();

        assert_eq!(
            rewritten,
            r#"<img src="../_hosts/downloads.example.com/files/preview/shot.jpg">"#
        );
    }

    #[test]
    fn keeps_captured_download_links() {
        let mut known_paths = HashMap::new();
        known_paths.insert(
            "http://downloads.example.com/file.exe".to_owned(),
            PathBuf::from("downloads/file.exe"),
        );
        let context = RewriteContext::new(
            "http://example.com/downloads.html",
            PathBuf::from("downloads.html"),
            &known_paths,
        )
        .unwrap();

        let rewritten = rewrite_html(
            r#"<a href="http://downloads.example.com/file.exe">Download</a>"#,
            &context,
        )
        .unwrap();

        assert_eq!(rewritten, r#"<a href="downloads/file.exe">Download</a>"#);
    }
}
