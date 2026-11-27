use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result};
use lol_html::html_content::{ContentType, Element};
use lol_html::{HtmlRewriter, Settings, element, end_tag, text};
use url::Url;

use crate::download_refs::{is_downloadable_file_url, is_extra_file_url};
use crate::html_attrs::{decoded_attribute, set_decoded_attribute};
use crate::noise::is_archive_noise_reference;
use crate::pathmap::{
    SiteMapper, canonical_query_without_volatile_params, normalize_lookup_url, relative_link,
    sanitize_url_path_segment, unwrap_wayback_url,
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
    resolution_base: Url,
    current_local_path: PathBuf,
    known_paths: &'a HashMap<String, PathBuf>,
    mapper: Option<&'a SiteMapper>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Result of attempting to convert an archived URL reference to a local path.
pub enum UrlRewrite {
    /// Replace the original reference with the contained local relative path.
    Rewrite(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReferenceKind {
    Document,
    Resource,
}

trait ReferenceRewriter {
    fn rewrite_reference(&self, value: &str, kind: ReferenceKind) -> Option<UrlRewrite>;
    fn rewrite_embedded_html(&self, value: &str) -> Option<String>;
}

struct LocalPathRemapper<'a> {
    current_local_path: &'a Path,
    remapped_paths: &'a HashMap<PathBuf, PathBuf>,
    changed: Cell<bool>,
}

impl ReferenceRewriter for RewriteContext<'_> {
    fn rewrite_reference(&self, value: &str, kind: ReferenceKind) -> Option<UrlRewrite> {
        self.rewrite_url_reference_as(value, kind)
    }

    fn rewrite_embedded_html(&self, value: &str) -> Option<String> {
        let rewritten = rewrite_html(value, self).ok()?;
        (rewritten != value).then_some(rewritten)
    }
}

impl ReferenceRewriter for LocalPathRemapper<'_> {
    fn rewrite_reference(&self, value: &str, _kind: ReferenceKind) -> Option<UrlRewrite> {
        let trimmed = value.trim();
        if trimmed.is_empty() || should_skip_reference(trimmed) {
            return None;
        }
        let (without_fragment, fragment) = split_fragment(trimmed);
        if without_fragment.is_empty()
            || without_fragment.contains('?')
            || without_fragment.ends_with('/')
        {
            return None;
        }
        let virtual_root = Path::new("/__webarchive_root__");
        let from_file = virtual_root.join(self.current_local_path);
        let resolved = crate::link_validation::resolve_local_reference(
            virtual_root,
            &from_file,
            without_fragment,
        )?;
        let relative_target = resolved.strip_prefix(virtual_root).ok()?;
        let actual_target = self.remapped_paths.get(relative_target)?;
        let mut rewritten = relative_link(self.current_local_path, actual_target);
        if let Some(fragment) = fragment {
            rewritten.push('#');
            rewritten.push_str(fragment);
        }
        if rewritten == value {
            return None;
        }
        self.changed.set(true);
        Some(UrlRewrite::Rewrite(rewritten))
    }

    fn rewrite_embedded_html(&self, value: &str) -> Option<String> {
        let rewritten =
            remap_local_html_paths(value, self.current_local_path, self.remapped_paths)
                .ok()
                .flatten()?;
        self.changed.set(true);
        Some(rewritten)
    }
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
        let current_original = Url::parse(current_original)
            .with_context(|| format!("invalid original URL: {current_original}"))?;
        Ok(Self {
            resolution_base: current_original.clone(),
            current_original,
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
        let current_original = Url::parse(current_original)
            .with_context(|| format!("invalid original URL: {current_original}"))?;
        Ok(Self {
            resolution_base: current_original.clone(),
            current_original,
            current_local_path,
            known_paths,
            mapper: Some(mapper),
        })
    }

    fn with_resolution_base(&self, resolution_base: Url) -> Self {
        Self {
            current_original: self.current_original.clone(),
            resolution_base,
            current_local_path: self.current_local_path.clone(),
            known_paths: self.known_paths,
            mapper: self.mapper,
        }
    }

    /// Rewrites one URL-like resource reference relative to the current local file.
    ///
    /// This is intended for resources by default. Document links should use the
    /// internal document-aware path handling so extensionless pages map to
    /// `index.html`. `None` means the original reference must remain unchanged.
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
        let resolved = self.resolution_base.join(&archive_unwrapped).ok()?;
        let mut lookup_url = resolved.clone();
        lookup_url.set_fragment(None);
        let lookup_key = normalize_lookup_url(lookup_url.as_str());
        let known_path = self.known_paths.get(&lookup_key).or_else(|| {
			self.mapper?.known_path_for_url(&lookup_url, self.known_paths)
		});
        let local_path = if let Some(local_path) = known_path.cloned() {
            Some(local_path)
        } else if is_archive_noise_reference(lookup_url.as_str()) {
            None
        } else if kind == ReferenceKind::Document && is_downloadable_file_url(&lookup_url) {
            self.downloadable_fallback_path(&lookup_url)
        } else if is_extra_file_url(&lookup_url) {
            self.related_resource_fallback_path(&lookup_url, kind)
        } else {
            self.fallback_same_site_path(&lookup_url, kind)
        };

        let Some(local_path) = local_path else {
            let lower = archive_unwrapped.to_ascii_lowercase();
            if self.resolution_base != self.current_original
                && !lower.starts_with("http://")
                && !lower.starts_with("https://")
                && !archive_unwrapped.starts_with("//")
            {
                let mut absolute = resolved.to_string();
                if let Some(fragment) = fragment {
                    absolute.push('#');
                    absolute.push_str(fragment);
                }
                return Some(UrlRewrite::Rewrite(absolute));
            }
            return None;
        };

        let mut rewritten = relative_link(&self.current_local_path, &local_path);
        if let Some(fragment) = fragment {
            rewritten.push('#');
            rewritten.push_str(fragment);
        }
        Some(UrlRewrite::Rewrite(rewritten))
    }

    fn fallback_same_site_path(&self, url: &Url, kind: ReferenceKind) -> Option<PathBuf> {
        if kind == ReferenceKind::Resource
            && (url.path().is_empty() || url.path().ends_with('/'))
        {
            return None;
        }
		if let Some(mapper) = self.mapper
			&& mapper.is_related_host(url.host_str()?)
		{
			let mime = match kind {
				ReferenceKind::Document => "text/html",
				ReferenceKind::Resource => "application/octet-stream",
			};
			return mapper.local_path_for_url(url.as_str(), mime).ok();
		}
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
        if kind == ReferenceKind::Resource
            && (url.path().is_empty() || url.path().ends_with('/'))
        {
            return None;
        }
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

/// Resolves the document's effective base URL from the first `<base href="...">` element,
/// unwrapping any Wayback replay wrapper and resolving relative base hrefs against `document_url`.
pub(crate) fn extract_html_base_url(input: &str, document_url: &Url) -> Option<Url> {
    let seen_base_href = Rc::new(Cell::new(false));
    let resolved = Rc::new(RefCell::new(None::<Url>));
    let mut parser = HtmlRewriter::new(
        Settings {
            element_content_handlers: vec![element!("base[href]", {
                let seen_base_href = Rc::clone(&seen_base_href);
                let resolved = Rc::clone(&resolved);
                let document_url = document_url.clone();
                move |element| {
                    if seen_base_href.replace(true) {
                        return Ok(());
                    }
                    if let Some(href) = decoded_attribute(element, "href") {
                        let trimmed = href.trim();
                        if !trimmed.is_empty() {
                            let unwrapped = unwrap_wayback_url(trimmed);
                            if let Ok(url) = document_url.join(&unwrapped)
                                && matches!(url.scheme(), "http" | "https")
                            {
                                *resolved.borrow_mut() = Some(url);
                            }
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
    resolved.borrow().clone()
}

/// Rewrites localizable references in one HTML document.
///
/// The pass covers ordinary URL attributes, CSS in style blocks and attributes,
/// JavaScript string references, old rollover image handlers, `srcset`,
/// `meta refresh`, and legacy `APPLET`/`PARAM` patterns common in older sites.
/// Any `<base href>` element is honored during URL resolution and neutralized
/// in the rewritten output so browsers resolve relative local links against the
/// saved file rather than the original remote base URL.
pub fn rewrite_html(input: &str, context: &RewriteContext<'_>) -> Result<String> {
    let base_override = extract_html_base_url(input, &context.current_original);
    let effective_context;
    let context = if let Some(base_url) = base_override {
        effective_context = context.with_resolution_base(base_url);
        &effective_context
    } else {
        context
    };
    let mut output = Vec::with_capacity(input.len());
	let mut style_text = String::new();
	let mut script_text = String::new();
	let javascript_script = Rc::new(Cell::new(true));
    let rollover_stack = Rc::new(RefCell::new(Vec::<Vec<RolloverImageSource>>::new()));
    let settings = Settings {
        element_content_handlers: vec![
            element!("base[href]", |element| {
                element.remove_attribute("href");
                if element.attributes().is_empty() {
                    element.remove();
                }
                Ok(())
            }),
			element!("script", {
				let javascript_script = Rc::clone(&javascript_script);
				move |element| {
					javascript_script.set(crate::javascript::is_javascript_element(element));
					Ok(())
				}
			}),
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
            element!("link[href]", move |element| {
                let kind = link_reference_kind(element);
                rewrite_attr_url(element, "href", context, kind);
                Ok(())
            }),
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
				if element
					.get_attribute("http-equiv")
					.is_some_and(|value| value.eq_ignore_ascii_case("content-type"))
				{
					element.set_attribute("content", "text/html; charset=utf-8")?;
				}
                Ok(())
            }),
			element!("meta[charset]", |element| {
				element.set_attribute("charset", "utf-8")?;
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
				style_text.push_str(chunk.as_str());
				if chunk.last_in_text_node() {
					let rewritten = rewrite_css(&style_text, context);
					// HTML raw-text elements do not decode character references.
					chunk.replace(&rewritten, ContentType::Html);
					style_text.clear();
				} else {
					chunk.remove();
				}
                Ok(())
            }),
            text!("script", move |chunk| {
				// Buffer complete nodes so HTML tokenizer boundaries cannot split JS strings.
				script_text.push_str(chunk.as_str());
				if chunk.last_in_text_node() {
					let rewritten = if javascript_script.get() {
						rewrite_javascript_string_urls(&script_text, context)
					} else {
						script_text.clone()
					};
					chunk.replace(&rewritten, ContentType::Html);
					script_text.clear();
				} else {
					chunk.remove();
				}
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

/// Rewrites URLs and imports in one CSS stylesheet or style fragment.
///
/// CSS tokens preserve quoting and correctly distinguish escaped URLs from comments or strings.
pub fn rewrite_css(input: &str, context: &RewriteContext<'_>) -> String {
	rewrite_css_references(input, context, true)
}

fn rewrite_css_references<R: ReferenceRewriter>(
	input: &str,
	rewriter: &R,
	normalize_charset: bool,
) -> String {
	let mut rewritten = String::with_capacity(input.len());
	let mut offset = 0;
	if normalize_charset && let Some((_, end)) = crate::text_encoding::css_charset(input) {
		rewritten.push_str("@charset \"UTF-8\";");
		offset = end;
	}
	for reference in crate::css_refs::references(input) {
		if let Some(UrlRewrite::Rewrite(value)) =
			rewriter.rewrite_reference(&reference.value, ReferenceKind::Resource)
		{
			rewritten.push_str(&input[offset..reference.range.start]);
			rewritten.push_str(&reference.replacement(&value));
			offset = reference.range.end;
		}
	}
	rewritten.push_str(&input[offset..]);
	rewritten
}

/// Updates already-rewritten local references in one CSS document when late extra-download
/// discovery resolves a fallback path to a different on-disk filename.
///
/// Returns `None` when no reference changed so untouched files stay unmodified.
pub(crate) fn remap_local_css_paths(
	input: &str,
	current_local_path: &Path,
	remapped_paths: &HashMap<PathBuf, PathBuf>,
) -> Option<String> {
	let remapper = LocalPathRemapper {
		current_local_path,
		remapped_paths,
		changed: Cell::new(false),
	};
	let rewritten = rewrite_css_references(input, &remapper, false);
	remapper.changed.get().then_some(rewritten)
}

/// Updates already-rewritten local references in one HTML document when late extra-download
/// discovery resolves a fallback path to a different on-disk filename.
///
/// Returns `Ok(None)` when no reference changed so untouched files stay unmodified.
pub(crate) fn remap_local_html_paths(
	input: &str,
	current_local_path: &Path,
	remapped_paths: &HashMap<PathBuf, PathBuf>,
) -> Result<Option<String>> {
	let remapper = LocalPathRemapper {
		current_local_path,
		remapped_paths,
		changed: Cell::new(false),
	};
	let remapper_ref = &remapper;
	let mut output = Vec::with_capacity(input.len());
	let mut style_text = String::new();
	let mut script_text = String::new();
	let javascript_script = Rc::new(Cell::new(true));
	let settings = Settings {
		element_content_handlers: vec![
			element!("script", {
				let javascript_script = Rc::clone(&javascript_script);
				move |element| {
					javascript_script.set(crate::javascript::is_javascript_element(element));
					Ok(())
				}
			}),
			attr_rewriter!("a[href]", "href", remapper_ref, ReferenceKind::Document),
			attr_rewriter!("blockquote[cite]", "cite", remapper_ref, ReferenceKind::Document),
			attr_rewriter!("q[cite]", "cite", remapper_ref, ReferenceKind::Document),
			attr_rewriter!("del[cite]", "cite", remapper_ref, ReferenceKind::Document),
			attr_rewriter!("ins[cite]", "cite", remapper_ref, ReferenceKind::Document),
			attr_rewriter!("area[href]", "href", remapper_ref, ReferenceKind::Document),
			attr_rewriter!("audio[src]", "src", remapper_ref, ReferenceKind::Resource),
			attr_rewriter!("embed[src]", "src", remapper_ref, ReferenceKind::Resource),
			attr_rewriter!("form[action]", "action", remapper_ref, ReferenceKind::Document),
			attr_rewriter!("frame[src]", "src", remapper_ref, ReferenceKind::Document),
			attr_rewriter!("iframe[src]", "src", remapper_ref, ReferenceKind::Document),
			attr_rewriter!("img[src]", "src", remapper_ref, ReferenceKind::Resource),
			attr_rewriter!("input[src]", "src", remapper_ref, ReferenceKind::Resource),
			element!("link[href]", move |element| {
				let kind = link_reference_kind(element);
				rewrite_attr_url(element, "href", remapper_ref, kind);
				Ok(())
			}),
			attr_rewriter!("object[data]", "data", remapper_ref, ReferenceKind::Resource),
			attr_rewriter!("script[src]", "src", remapper_ref, ReferenceKind::Resource),
			attr_rewriter!("source[src]", "src", remapper_ref, ReferenceKind::Resource),
			attr_rewriter!("track[src]", "src", remapper_ref, ReferenceKind::Resource),
			attr_rewriter!("video[poster]", "poster", remapper_ref, ReferenceKind::Resource),
			attr_rewriter!("video[src]", "src", remapper_ref, ReferenceKind::Resource),
			guarded_attr_rewriter!("applet[code]", "code", remapper_ref, ReferenceKind::Resource),
			guarded_attr_rewriter!(
				"object[codebase]",
				"codebase",
				remapper_ref,
				ReferenceKind::Document
			),
			guarded_attr_rewriter!("option[value]", "value", remapper_ref, ReferenceKind::Document),
			guarded_attr_rewriter!("param[value]", "value", remapper_ref, ReferenceKind::Resource),
			element!("*[srcset]", move |element| {
				rewrite_attr_srcset(element, "srcset", remapper_ref);
				Ok(())
			}),
			element!("applet[archive]", move |element| {
				rewrite_attr_comma_separated_urls(
					element,
					"archive",
					remapper_ref,
					ReferenceKind::Resource,
				);
				Ok(())
			}),
			attr_rewriter!(
				"*[background]",
				"background",
				remapper_ref,
				ReferenceKind::Resource
			),
			element!("meta[http-equiv][content]", move |element| {
				rewrite_meta_refresh(element, remapper_ref);
				Ok(())
			}),
			element!("*[style]", move |element| {
				rewrite_attr_css(element, "style", remapper_ref);
				Ok(())
			}),
			js_attr_rewriter!("*[onclick]", "onclick", remapper_ref),
			js_attr_rewriter!("*[onload]", "onload", remapper_ref),
			js_attr_rewriter!("*[onmousedown]", "onmousedown", remapper_ref),
			js_attr_rewriter!("*[onmouseover]", "onmouseover", remapper_ref),
			js_attr_rewriter!("*[onmouseout]", "onmouseout", remapper_ref),
			js_attr_rewriter!("*[onmouseup]", "onmouseup", remapper_ref),
			js_attr_rewriter!("*[onfocus]", "onfocus", remapper_ref),
			js_attr_rewriter!("*[onblur]", "onblur", remapper_ref),
			js_attr_rewriter!("*[onchange]", "onchange", remapper_ref),
			text!("style", move |chunk| {
				style_text.push_str(chunk.as_str());
				if chunk.last_in_text_node() {
					let rewritten = rewrite_css_references(&style_text, remapper_ref, false);
					chunk.replace(&rewritten, ContentType::Html);
					style_text.clear();
				} else {
					chunk.remove();
				}
				Ok(())
			}),
			text!("script", move |chunk| {
				script_text.push_str(chunk.as_str());
				if chunk.last_in_text_node() {
					let rewritten = if javascript_script.get() {
						rewrite_javascript_string_urls(&script_text, remapper_ref)
					} else {
						script_text.clone()
					};
					chunk.replace(&rewritten, ContentType::Html);
					script_text.clear();
				} else {
					chunk.remove();
				}
				Ok(())
			}),
		],
		..Settings::default()
	};

	let mut rewriter = HtmlRewriter::new(settings, |chunk: &[u8]| output.extend_from_slice(chunk));
	rewriter
		.write(input.as_bytes())
		.context("failed to remap HTML references")?;
	rewriter
		.end()
		.context("failed to finish HTML reference remapping")?;

	if !remapper.changed.get() {
		return Ok(None);
	}
	String::from_utf8(output)
		.context("remapped HTML is not valid UTF-8")
		.map(Some)
}

fn link_reference_kind(element: &Element<'_, '_>) -> ReferenceKind {
    let Some(rel) = element.get_attribute("rel") else {
        return ReferenceKind::Resource;
    };
    let mut has_document_rel = false;
    for token in rel.split_whitespace() {
        let lower = token.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "stylesheet"
                | "icon"
                | "shortcut"
                | "apple-touch-icon"
                | "apple-touch-icon-precomposed"
                | "mask-icon"
                | "manifest"
                | "preload"
                | "modulepreload"
                | "prefetch"
                | "dns-prefetch"
                | "preconnect"
                | "pingback"
                | "search-plugin"
        ) {
            return ReferenceKind::Resource;
        }
        if matches!(
            lower.as_str(),
            "author"
                | "bookmark"
                | "canonical"
                | "chapter"
                | "contents"
                | "copyright"
                | "first"
                | "glossary"
                | "help"
                | "home"
                | "index"
                | "last"
                | "license"
                | "next"
                | "prev"
                | "previous"
                | "search"
                | "section"
                | "start"
                | "subsection"
                | "top"
                | "up"
        ) {
            has_document_rel = true;
        }
    }
    if has_document_rel {
        ReferenceKind::Document
    } else {
        ReferenceKind::Resource
    }
}

fn rewrite_attr_url<R: ReferenceRewriter>(
    element: &mut Element<'_, '_>,
    attr: &str,
    rewriter: &R,
    kind: ReferenceKind,
) {
    let Some(value) = decoded_attribute(element, attr) else {
        return;
    };
    match rewriter.rewrite_reference(&value, kind) {
        Some(UrlRewrite::Rewrite(rewritten)) => {
            set_decoded_attribute(element, attr, &rewritten).ok();
        }
        None => {}
    }
}

/// Rewrites a guarded attribute only when it is clearly a URL or file reference.
///
/// Legacy attributes such as `option[value]` and `param[value]` often contain
/// arbitrary values, so this avoids treating labels like `Action` as paths.
fn rewrite_attr_url_if_reference_like<R: ReferenceRewriter>(
    element: &mut Element<'_, '_>,
    attr: &str,
    rewriter: &R,
    kind: ReferenceKind,
) {
    let Some(value) = decoded_attribute(element, attr) else {
        return;
    };
    if !looks_like_url_reference_or_file(&value) {
        return;
    }
    match rewriter.rewrite_reference(&value, kind) {
        Some(UrlRewrite::Rewrite(rewritten)) => {
            set_decoded_attribute(element, attr, &rewritten).ok();
        }
        None => {}
    }
}

fn rewrite_attr_srcset<R: ReferenceRewriter>(
    element: &mut Element<'_, '_>,
    attr: &str,
    rewriter: &R,
) {
    let Some(value) = decoded_attribute(element, attr) else {
        return;
    };
    let Some(rewritten) = rewrite_srcset(&value, rewriter) else {
        return;
    };
    if rewritten.is_empty() {
        element.remove_attribute(attr);
    } else {
        set_decoded_attribute(element, attr, &rewritten).ok();
    }
}

fn rewrite_srcset<R: ReferenceRewriter>(input: &str, rewriter: &R) -> Option<String> {
    let mut changed = false;
    let mut candidates = Vec::new();

    for candidate in input.split(',') {
        let trimmed = candidate.trim();
        if trimmed.is_empty() {
            continue;
        }
        let url_end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        let (url, descriptor) = trimmed.split_at(url_end);
        match rewriter.rewrite_reference(url, ReferenceKind::Resource) {
            Some(UrlRewrite::Rewrite(rewritten)) => {
                changed = true;
                candidates.push(format!("{rewritten}{descriptor}"));
            }
            None => candidates.push(trimmed.to_owned()),
        }
    }

    changed.then(|| candidates.join(", "))
}

/// Rewrites comma-separated legacy URL lists such as `applet archive`.
fn rewrite_attr_comma_separated_urls<R: ReferenceRewriter>(
    element: &mut Element<'_, '_>,
    attr: &str,
    rewriter: &R,
    kind: ReferenceKind,
) {
    let Some(value) = decoded_attribute(element, attr) else {
        return;
    };
    let mut changed = false;
    let mut rewritten_values = Vec::new();
    for raw_value in value.split(',') {
        let trimmed = raw_value.trim();
        if trimmed.is_empty() {
            continue;
        }
        match rewriter.rewrite_reference(trimmed, kind) {
            Some(UrlRewrite::Rewrite(rewritten)) => {
                changed = true;
                rewritten_values.push(rewritten);
            }
            None => rewritten_values.push(trimmed.to_owned()),
        }
    }
    if changed {
        if rewritten_values.is_empty() {
            element.remove_attribute(attr);
        } else {
            set_decoded_attribute(element, attr, &rewritten_values.join(", ")).ok();
        }
    }
}

/// Rewrites `meta http-equiv=refresh` targets while preserving the delay prefix.
fn rewrite_meta_refresh<R: ReferenceRewriter>(element: &mut Element<'_, '_>, rewriter: &R) {
    let Some(http_equiv) = element.get_attribute("http-equiv") else {
        return;
    };
    if !http_equiv.trim().eq_ignore_ascii_case("refresh") {
        return;
    }
    let Some(content) = decoded_attribute(element, "content") else {
        return;
    };
    let Some(rewritten) = rewrite_meta_refresh_content(&content, rewriter) else {
        return;
    };
    set_decoded_attribute(element, "content", &rewritten).ok();
}

fn rewrite_meta_refresh_content<R: ReferenceRewriter>(
    input: &str,
    rewriter: &R,
) -> Option<String> {
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

    let UrlRewrite::Rewrite(rewritten) = rewriter.rewrite_reference(url, ReferenceKind::Document)?;
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

fn rewrite_attr_css<R: ReferenceRewriter>(
    element: &mut Element<'_, '_>,
    attr: &str,
    rewriter: &R,
) {
    let Some(value) = decoded_attribute(element, attr) else {
        return;
    };
    let rewritten = rewrite_css_references(&value, rewriter, false);
    if rewritten != value {
        set_decoded_attribute(element, attr, &rewritten).ok();
    }
}

fn rewrite_attr_javascript_urls<R: ReferenceRewriter>(
	element: &mut Element<'_, '_>,
	attr: &str,
	rewriter: &R,
) {
	let Some(value) = element.get_attribute(attr) else {
		return;
	};
	let decoded = html_escape::decode_html_entities(&value);
	let rewritten = rewrite_javascript_string_urls(&decoded, rewriter);
	if rewritten != decoded {
		let encoded = html_escape::encode_double_quoted_attribute(&rewritten);
		element.set_attribute(attr, &encoded).ok();
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
                set_decoded_attribute(element, "src", &source.src).ok();
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
                &html_escape::decode_html_entities(&value), context,
            ));
        }
    }
    sources
}

fn extract_rollover_image_sources_from_javascript(
    input: &str,
    context: &RewriteContext<'_>,
) -> Vec<RolloverImageSource> {
	let literals = crate::javascript::strings(input);
	let strings = literals.iter().map(|literal| literal.value.as_str()).collect::<Vec<_>>();
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

/// Rewrites decoded literal values and escapes each edited value for its original delimiter.
fn rewrite_javascript_string_urls<R: ReferenceRewriter>(input: &str, rewriter: &R) -> String {
	let mut rewritten = String::with_capacity(input.len());
	let mut cursor = 0;
	for literal in crate::javascript::strings(input) {
		let value = literal.value.as_str();
		let replacement = if !literal.is_concatenated && looks_like_javascript_url_reference(value) {
			let kind = javascript_reference_kind(value);
			rewriter
				.rewrite_reference(value, kind)
				.map(url_rewrite_to_javascript_string)
		} else {
			rewrite_html_fragment_in_javascript_string(value, rewriter)
		};

		if let Some(replacement) = replacement.filter(|replacement| replacement != value) {
			rewritten.push_str(&input[cursor..literal.range.start]);
			rewritten.push_str(&crate::javascript::quote(&replacement, literal.quote));
			cursor = literal.range.end;
		}
	}

	rewritten.push_str(&input[cursor..]);
	rewritten
}

fn javascript_reference_kind(value: &str) -> ReferenceKind {
    let path = strip_query_and_fragment(value.trim());
    let Some(file_name) = path.rsplit('/').next() else {
        return ReferenceKind::Resource;
    };
    let Some((_, extension)) = file_name.rsplit_once('.') else {
        return ReferenceKind::Resource;
    };
    if matches!(
        extension.to_ascii_lowercase().as_str(),
        "html" | "htm" | "xhtml" | "shtml" | "php" | "asp" | "aspx" | "jsp" | "cfm" | "cgi"
    ) {
        ReferenceKind::Document
    } else {
        ReferenceKind::Resource
    }
}

fn url_rewrite_to_javascript_string(rewrite: UrlRewrite) -> String {
    let UrlRewrite::Rewrite(value) = rewrite;
    value
}

fn rewrite_html_fragment_in_javascript_string<R: ReferenceRewriter>(
    value: &str,
    rewriter: &R,
) -> Option<String> {
    if !value.contains('<') || !value.contains('>') {
        return None;
    }

    rewriter.rewrite_embedded_html(value)
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
            path.push(sanitize_url_path_segment(segment));
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
                .map(sanitize_url_path_segment)
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

#[cfg(test)]
mod tests {
    use super::*;

	/// Session-only navigation must find an HTML capture across primary host variants.
	#[test]
	fn rewrites_session_navigation_across_primary_host_and_scheme_variants() {
		let mapper = SiteMapper::new("example.com").unwrap();
		let known = HashMap::from([(
			"http://www.example.com/forums/faq.php".to_owned(),
			PathBuf::from("forums/faq.php.html"),
		)]);
		for original in [
			"http://www.example.com/forums/topic.php?t=1",
			"http://example.com/forums/topic.php?t=1",
			"https://www.example.com/forums/topic.php?t=1",
			"https://example.com/forums/topic.php?t=1",
		] {
			let context = RewriteContext::new_with_mapper(original, "forums/topic.html".into(), &known, &mapper).unwrap();
			let actual = rewrite_html(r##"<a href="./faq.php?sid=abc#help">FAQ</a>"##, &context).unwrap();
			assert_eq!(actual, r##"<a href="faq.php.html#help">FAQ</a>"##, "{original}");
		}
	}

	/// Alternate lookup must retain page selectors and the destination's recorded MIME path.
	#[test]
	fn equivalent_lookup_preserves_query_specific_mime_paths() {
		let mapper = SiteMapper::new("example.com").unwrap();
		let known = HashMap::from([
			("http://www.example.com/forums/style.php?id=1".to_owned(), PathBuf::from("forums/selected.css")),
			("http://www.example.com/forums/faq.php?id=1".to_owned(), PathBuf::from("forums/selected.html")),
		]);
		let context = RewriteContext::new_with_mapper("https://example.com/forums/topic.php", "forums/topic.html".into(), &known, &mapper).unwrap();
		let actual = rewrite_html(r#"<link href="style.php?sid=abc&amp;id=1"><a href="faq.php?sid=abc&amp;id=1">FAQ</a>"#, &context).unwrap();
		assert_eq!(actual, r#"<link href="selected.css"><a href="selected.html">FAQ</a>"#);
		let different_page = rewrite_html(r#"<a href="faq.php?id=2">Other</a>"#, &context).unwrap();
		assert!(!different_page.contains("selected.html"));
	}

	/// Host fallback must not guess between conflicting paths or cross ports/subdomains.
	#[test]
	fn equivalent_lookup_keeps_ambiguous_and_unrelated_session_links() {
		let mapper = SiteMapper::new("example.com").unwrap();
		let known = HashMap::from([
			("http://example.com/faq.php".to_owned(), PathBuf::from("first.html")),
			("https://www.example.com/faq.php".to_owned(), PathBuf::from("second.html")),
		]);
		let context = RewriteContext::new_with_mapper("http://www.example.com/index.html", "index.html".into(), &known, &mapper).unwrap();
		for href in [
			"faq.php?sid=abc", "http://example.com:8080/faq.php?sid=abc",
			"http://other.example.com/faq.php?sid=abc", "http://unrelated.invalid/faq.php?sid=abc",
			"http://user:password@example.com/faq.php?sid=abc",
		] {
			let input = format!("<a href=\"{href}\">FAQ</a>");
			assert_eq!(rewrite_html(&input, &context).unwrap(), input);
		}
		let exact = RewriteContext::new_with_mapper("https://www.example.com/index.html", "index.html".into(), &known, &mapper).unwrap();
		assert_eq!(rewrite_html(r#"<a href="faq.php?sid=abc">FAQ</a>"#, &exact).unwrap(), r#"<a href="second.html">FAQ</a>"#);
	}

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

	/// Imports use the same local mapping as URLs; comments and literal strings are untouched.
	#[test]
	fn rewrites_css_imports_and_escaped_urls() {
		with_context(|context| {
			let input = r#"@import '/css/site.css' screen; /* url(/missing.png) */ .logo { content: 'url(/missing.png)'; background: url('/img/lo\67 o.png'); }"#;
			assert_eq!(rewrite_css(input, &context), r#"@import 'css/site.css' screen; /* url(/missing.png) */ .logo { content: 'url(/missing.png)'; background: url('img/logo.png'); }"#);
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
    fn preserves_unresolved_forum_anchor_targets() {
        with_context(|context| {
            let rewritten = rewrite_html(
                r#"<a class="topictitle" href="./viewtopic.php?f=43&amp;t=14887&amp;sid=abcdef">[RAS2] Diary Entry #20</a><a class="username-coloured" href="./memberlist.php?mode=viewprofile&amp;u=2&amp;sid=abcdef">jonathan</a><a href="./viewtopic.php?f=43&amp;p=22377#p22377"><img src="styles/prosilver/imageset/icon_topic_latest.gif"></a>"#,
                &context,
            )
            .unwrap();

            assert!(rewritten.contains(r#"<a class="topictitle" href="#));
            assert!(rewritten.contains(r#"<a class="username-coloured" href="#));
            assert_eq!(rewritten.matches("<a ").count(), 3);
            assert_eq!(rewritten.matches("href=").count(), 3);
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
    fn preserves_unresolved_references_without_removing_attributes() {
        with_context(|context| {
            let rewritten = rewrite_html(
                r#"<a href="/forums/login.php">Login</a><img src="/forums/cron.php" width="1" height="1" alt="cron"><img src="/img/logo.png?sid=abcdef">"#,
                &context,
            )
            .unwrap();

            assert!(rewritten.contains(r#"<a href="forums/login.php.html">Login</a>"#));
            assert!(rewritten.contains(r#"src="forums/cron.php""#));
            assert!(rewritten.contains(r#"src="img/logo.png""#));
        });
    }

    #[test]
    fn preserves_unresolved_download_links() {
        with_context(|context| {
            let rewritten = rewrite_html(
                r#"<a href="http://downloads.example.com/file.exe" target="download">Download</a>"#,
                &context,
            )
            .unwrap();

            assert_eq!(
                rewritten,
                r#"<a href="http://downloads.example.com/file.exe" target="download">Download</a>"#
            );
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

    /// Empty template variables in CSS `url(templates/subSilver/images/)` or resource
    /// attributes ending in `/` must not be rewritten to `.../index.html`.
    #[test]
    fn does_not_rewrite_directory_only_resource_references_to_index_html() {
        let known_paths = HashMap::new();
        let mapper = SiteMapper::new("example.com").unwrap();
        let context = RewriteContext::new_with_mapper(
            "http://example.com/forums/viewtopic.php?t=1",
            PathBuf::from("forums/viewtopic.php__q_1.html"),
            &known_paths,
            &mapper,
        )
        .unwrap();

        let html = r#"<style>th { background-image: url(templates/subSilver/images/); }</style><a href="templates/subSilver/">Theme dir</a>"#;
        let rewritten = rewrite_html(html, &context).unwrap();

        assert!(
            rewritten.contains("url(templates/subSilver/images/)"),
            "directory-only CSS resource must not become index.html: {rewritten}"
        );
        assert!(
            !rewritten.contains("templates/subSilver/images/index.html"),
            "{rewritten}"
        );
        assert!(
            rewritten.contains(r#"href="templates/subSilver/index.html""#),
            "document anchor links to directories should still map to index.html: {rewritten}"
        );
    }

    /// Navigation `<link rel="prev|next|up|chapter|top|help">` tags point to HTML documents
    /// and must map to `*.php__q_<hash>.html` / `*.php.html` rather than `application/octet-stream`
    /// paths like `viewtopic__q_<hash>.php`, while `<link rel="stylesheet">` stays a resource.
    #[test]
    fn rewrites_navigation_link_elements_as_documents_and_stylesheets_as_resources() {
        let known_paths = HashMap::new();
        let mapper = SiteMapper::new("example.com").unwrap();
        let context = RewriteContext::new_with_mapper(
            "http://example.com/forums/viewtopic.php?t=100",
            PathBuf::from("forums/viewtopic.php__q_100.html"),
            &known_paths,
            &mapper,
        )
        .unwrap();

        let html = concat!(
            r#"<link rel="stylesheet" href="style.php?id=1">"#,
            r#"<link rel="prev" href="viewtopic.php?t=100&amp;view=previous">"#,
            r#"<link rel="chapter" href="viewforum.php?f=1">"#,
            r#"<link rel="help" href="faq.php">"#,
        );
        let rewritten = rewrite_html(html, &context).unwrap();

        assert!(
            rewritten.contains(r#"<link rel="stylesheet" href="style__q_"#)
                && rewritten.contains(r#".php">"#),
            "stylesheet link should remain a resource: {rewritten}"
        );
        assert!(
            rewritten.contains(r#"<link rel="prev" href="viewtopic.php__q_"#),
            "prev link must map as HTML document: {rewritten}"
        );
        assert!(
            rewritten.contains(r#"<link rel="chapter" href="viewforum.php__q_"#),
            "chapter link must map as HTML document: {rewritten}"
        );
        assert!(
            rewritten.contains(r#"<link rel="help" href="faq.php.html">"#),
            "help link must map as HTML document: {rewritten}"
        );
        assert!(!rewritten.contains("viewtopic__q_"), "{rewritten}");
        assert!(!rewritten.contains("viewforum__q_"), "{rewritten}");
    }

    /// JavaScript string literals referencing dynamic document pages (e.g. `viewtopic.php?...`
    /// or `page.asp?...`) must use document path mapping (`*.php__q_<hash>.html`) rather than
    /// `application/octet-stream` (`viewtopic__q_<hash>.php`), while static assets remain resources.
    #[test]
    fn rewrites_javascript_document_url_literals_with_html_mapping() {
        let known_paths = HashMap::new();
        let mapper = SiteMapper::new("example.com").unwrap();
        let context = RewriteContext::new_with_mapper(
            "http://example.com/forums/viewtopic.php?t=100",
            PathBuf::from("forums/viewtopic.php__q_100.html"),
            &known_paths,
            &mapper,
        )
        .unwrap();

        let html = r#"<script>var base_url = './viewtopic.php?f=10&t=200'; var img = '/img/logo.gif';</script>"#;
        let rewritten = rewrite_html(html, &context).unwrap();

        assert!(
            rewritten.contains("var base_url = 'viewtopic.php__q_")
                && rewritten.contains(".html';"),
            "JS document literal should map as HTML: {rewritten}"
        );
        assert!(
            rewritten.contains("var img = '../img/logo.gif';"),
            "JS static asset literal should stay a resource: {rewritten}"
        );
        assert!(!rewritten.contains("viewtopic__q_"), "{rewritten}");
    }

    /// `<base href>` must drive relative URL resolution during rewriting and then be neutralized
    /// so browsers resolve the rewritten relative links against the saved local file path.
    #[test]
    fn honors_and_neutralizes_base_href_elements() {
        let mut known_paths = HashMap::new();
        known_paths.insert(
            "http://example.com/sub/dir/logo.gif".to_owned(),
            PathBuf::from("sub/dir/logo.gif"),
        );
        known_paths.insert(
            "http://example.com/sub/dir/about.html".to_owned(),
            PathBuf::from("sub/dir/about.html"),
        );
        let mapper = SiteMapper::new("example.com").unwrap();
        let context = RewriteContext::new_with_mapper(
            "http://example.com/archive/old.html",
            PathBuf::from("archive/old.html"),
            &known_paths,
            &mapper,
        )
        .unwrap();

        let html = concat!(
            r#"<html><head><base href="http://example.com/sub/dir/" target="_blank"></head>"#,
            r#"<body><img src="logo.gif"><a href="about.html#team">About</a></body></html>"#,
        );
        let rewritten = rewrite_html(html, &context).unwrap();
        assert!(
            rewritten.contains(r#"<base target="_blank">"#),
            "non-href base attributes should be preserved while href is removed: {rewritten}"
        );
        assert!(!rewritten.contains("href=\"http://example.com/sub/dir/\""), "{rewritten}");
        assert!(rewritten.contains(r#"src="../sub/dir/logo.gif""#), "{rewritten}");
        assert!(
            rewritten.contains(r#"href="../sub/dir/about.html#team""#),
            "{rewritten}"
        );

        let wayback_base_html = r#"<html><head><base href="/web/20060101/http://example.com/sub/dir/"></head><body><img src="logo.gif"></body></html>"#;
        let wayback_rewritten = rewrite_html(wayback_base_html, &context).unwrap();
        assert!(!wayback_rewritten.contains("<base"), "{wayback_rewritten}");
        assert!(
            wayback_rewritten.contains(r#"src="../sub/dir/logo.gif""#),
            "{wayback_rewritten}"
        );

        let external_base_html = r#"<html><head><base href="https://external.invalid/docs/"></head><body><a href="manual.pdf#p2">Manual</a></body></html>"#;
        let external_rewritten = rewrite_html(external_base_html, &context).unwrap();
        assert!(!external_rewritten.contains("<base"), "{external_rewritten}");
        assert!(
            external_rewritten.contains(r#"href="https://external.invalid/docs/manual.pdf#p2""#),
            "unmapped relative URLs under an external <base href> must become absolute: {external_rewritten}"
        );

        let empty_first_base_html = concat!(
            r#"<html><head><base href=""><base href="http://example.com/sub/dir/"></head>"#,
            r#"<body><img src="local.gif"></body></html>"#,
        );
        let empty_first_rewritten = rewrite_html(empty_first_base_html, &context).unwrap();
        assert!(!empty_first_rewritten.contains("<base"), "{empty_first_rewritten}");
        assert!(
            empty_first_rewritten.contains(r#"src="local.gif""#),
            "an empty first <base href> must ignore later <base href> elements: {empty_first_rewritten}"
        );
    }

    /// Late extra-download remapping must update stale fallback paths in HTML and CSS
    /// while leaving unrelated links, comments, directory URLs, queries, and plain JS text intact.
    #[test]
    fn remaps_stale_local_paths_in_html_and_css_without_touching_unrelated_content() {
        let mut remapped = HashMap::new();
        remapped.insert(PathBuf::from("photo"), PathBuf::from("photo.jpg"));
        remapped.insert(PathBuf::from("my photo"), PathBuf::from("my photo.jpg"));
        remapped.insert(
            PathBuf::from("thumb__q_abc.php"),
            PathBuf::from("thumb.php__q_abc.jpg"),
        );
        remapped.insert(
            PathBuf::from("manual/index.html"),
            PathBuf::from("manual.pdf"),
        );
        remapped.insert(PathBuf::from("dir"), PathBuf::from("dir/index.html"));

        let html = concat!(
            "<!-- Saved from Wayback Machine: https://web.archive.org/web/20060101/http://example.com/sub/index.html -->\n",
            r#"<img src="../photo" srcset="../my%20photo 1x, ../thumb__q_abc.php 2x">"#,
            r#"<a href="../manual/index.html#sec2">Manual</a>"#,
            r#"<a href="../photo?keep=1">Query</a>"#,
            r#"<div style="background:url(../photo)"></div>"#,
            r#"<style>body { background: url(../dir/); } .hero { background: url(../my%20photo); }</style>"#,
            r#"<script>var word = 'photo'; var src = '../thumb__q_abc.php'; document.write('<img src="../photo">');</script>"#,
        );

        let updated = remap_local_html_paths(html, Path::new("sub/index.html"), &remapped)
            .unwrap()
            .expect("HTML should be updated");

        assert!(
            updated.starts_with("<!-- Saved from Wayback Machine:"),
            "{updated}"
        );
        assert!(updated.contains(r#"src="../photo.jpg""#), "{updated}");
        assert!(
            updated.contains(r#"srcset="../my%20photo.jpg 1x, ../thumb.php__q_abc.jpg 2x""#),
            "{updated}"
        );
        assert!(updated.contains(r#"href="../manual.pdf#sec2""#), "{updated}");
        assert!(
            updated.contains(r#"href="../photo?keep=1""#),
            "query-bearing links must not be stripped: {updated}"
        );
        assert!(updated.contains("background:url(../photo.jpg)"), "{updated}");
        assert!(
            updated.contains("background: url(../dir/);"),
            "directory-only CSS URLs must not be remapped: {updated}"
        );
        assert!(
            updated.contains("background: url(../my%20photo.jpg);"),
            "{updated}"
        );
        assert!(
            updated.contains("var word = 'photo';"),
            "plain JS text must not be remapped: {updated}"
        );
        assert!(
            updated.contains("var src = '../thumb.php__q_abc.jpg';"),
            "{updated}"
        );
        assert!(
            updated.contains(r#"document.write('<img src="../photo.jpg">');"#),
            "{updated}"
        );

        let untouched_html = r#"<img src="../existing.gif">"#;
        assert_eq!(
            remap_local_html_paths(untouched_html, Path::new("sub/index.html"), &remapped)
                .unwrap(),
            None
        );

        let css = "body { background: url(photo); } .keep { background: url(dir/); }";
        let updated_css =
            remap_local_css_paths(css, Path::new("style.css"), &remapped).unwrap();
        assert_eq!(
            updated_css,
            "body { background: url(photo.jpg); } .keep { background: url(dir/); }"
        );
        assert_eq!(
            remap_local_css_paths("body { color: red; }", Path::new("style.css"), &remapped),
            None
        );
    }
}
