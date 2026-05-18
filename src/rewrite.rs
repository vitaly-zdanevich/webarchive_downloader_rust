use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use lol_html::html_content::{ContentType, Element};
use lol_html::{HtmlRewriter, Settings, element, text};
use url::Url;

use crate::pathmap::{normalize_lookup_url, relative_link};

macro_rules! attr_rewriter {
    ($selector:literal, $attr:literal, $context:ident) => {
        element!($selector, move |element| {
            rewrite_attr_url(element, $attr, $context);
            Ok(())
        })
    };
}

#[derive(Clone, Debug)]
pub struct RewriteContext<'a> {
    current_original: Url,
    current_local_path: PathBuf,
    known_paths: &'a HashMap<String, PathBuf>,
}

impl<'a> RewriteContext<'a> {
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
        })
    }

    pub fn rewrite_url_reference(&self, value: &str) -> Option<String> {
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
        let local_path = self.known_paths.get(&lookup_key)?;

        let mut rewritten = relative_link(&self.current_local_path, local_path);
        if let Some(fragment) = fragment {
            rewritten.push('#');
            rewritten.push_str(fragment);
        }
        Some(rewritten)
    }
}

pub fn rewrite_html(input: &str, context: &RewriteContext<'_>) -> Result<String> {
    let mut output = Vec::with_capacity(input.len());
    let settings = Settings {
        element_content_handlers: vec![
            attr_rewriter!("a[href]", "href", context),
            attr_rewriter!("area[href]", "href", context),
            attr_rewriter!("audio[src]", "src", context),
            attr_rewriter!("embed[src]", "src", context),
            attr_rewriter!("form[action]", "action", context),
            attr_rewriter!("frame[src]", "src", context),
            attr_rewriter!("iframe[src]", "src", context),
            attr_rewriter!("img[src]", "src", context),
            attr_rewriter!("input[src]", "src", context),
            attr_rewriter!("link[href]", "href", context),
            attr_rewriter!("object[data]", "data", context),
            attr_rewriter!("script[src]", "src", context),
            attr_rewriter!("source[src]", "src", context),
            attr_rewriter!("track[src]", "src", context),
            attr_rewriter!("video[poster]", "poster", context),
            attr_rewriter!("video[src]", "src", context),
            attr_rewriter!("*[background]", "background", context),
            element!("*[style]", move |element| {
                rewrite_attr_css(element, "style", context);
                Ok(())
            }),
            text!("style", move |chunk| {
                let rewritten = rewrite_css(chunk.as_str(), context);
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

pub fn rewrite_css(input: &str, context: &RewriteContext<'_>) -> String {
    let mut rewritten = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(start) = rest.find("url(") {
        rewritten.push_str(&rest[..start + 4]);
        rest = &rest[start + 4..];

        let Some(end) = rest.find(')') else {
            rewritten.push_str(rest);
            return rewritten;
        };

        let raw_url = &rest[..end];
        let (quote, value) = trim_css_url(raw_url);
        if let Some(new_value) = context.rewrite_url_reference(value) {
            if let Some(quote) = quote {
                rewritten.push(quote);
                rewritten.push_str(&new_value);
                rewritten.push(quote);
            } else {
                rewritten.push_str(&new_value);
            }
        } else {
            rewritten.push_str(raw_url);
        }
        rewritten.push(')');
        rest = &rest[end + 1..];
    }

    rewritten.push_str(rest);
    rewritten
}

fn rewrite_attr_url(element: &mut Element<'_, '_>, attr: &str, context: &RewriteContext<'_>) {
    let Some(value) = element.get_attribute(attr) else {
        return;
    };
    if let Some(rewritten) = context.rewrite_url_reference(&value) {
        element.set_attribute(attr, &rewritten).ok();
    }
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
            let rewritten = rewrite_css(r#".logo { background: url('/img/logo.png'); }"#, &context);

            assert_eq!(rewritten, r#".logo { background: url('img/logo.png'); }"#);
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
}
