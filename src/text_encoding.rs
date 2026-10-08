//! Decodes archived HTML/CSS before rewriting, without silently discarding characters.

use std::borrow::Cow;
use std::cell::Cell;

use anyhow::{Result, ensure};
use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE, WINDOWS_1252, X_USER_DEFINED};
use lol_html::{RewriteStrSettings, element, rewrite_str};

/// Resolves a transport charset using MIME parameter parsing, including quoted labels.
fn transport_encoding(content_type: &str) -> Option<&'static Encoding> {
	let mime: mime::Mime = content_type.parse().ok()?;
	Encoding::for_label_no_replacement(mime.get_param(mime::CHARSET)?.as_str().as_bytes())
}

/// Returns a leading CSS encoding declaration's label and byte length.
///
/// CSS encoding declarations use this exact ASCII grammar, unlike ordinary at-rules.
pub(crate) fn css_charset(input: &str) -> Option<(&str, usize)> {
	let rest = input.strip_prefix("@charset \"")?;
	let (label, _) = rest.split_once("\";")?;
	Some((label, "@charset \"".len() + label.len() + "\";".len()))
}

/// Uses HTML tokenization so comments, scripts, and attribute ordering cannot spoof metadata.
fn html_encoding(bytes: &[u8]) -> Result<Option<&'static Encoding>> {
	let (prefix, _) = WINDOWS_1252.decode_without_bom_handling(&bytes[..bytes.len().min(1024)]);
	let encoding = Cell::new(None);
	rewrite_str(
		&prefix,
		RewriteStrSettings {
			element_content_handlers: vec![element!("meta", |element| {
				if encoding.get().is_some() {
					return Ok(());
				}
				let declared = if let Some(label) = element.get_attribute("charset") {
					Encoding::for_label_no_replacement(label.as_bytes())
				} else if element
					.get_attribute("http-equiv")
					.is_some_and(|value| value.eq_ignore_ascii_case("content-type"))
				{
					element
						.get_attribute("content")
						.and_then(|value| transport_encoding(&value))
				} else {
					None
				};
				encoding.set(declared.map(|value| {
					if value == UTF_16LE || value == UTF_16BE {
						UTF_8
					} else if value == X_USER_DEFINED {
						WINDOWS_1252
					} else {
						value
					}
				}));
				Ok(())
			})],
			..RewriteStrSettings::default()
		},
	)?;
	Ok(encoding.get())
}

/// Chooses BOM, transport charset, document declaration, then a deterministic fallback.
///
/// Undeclared valid UTF-8 stays UTF-8; other undeclared bytes use Windows-1252,
/// matching the Western legacy pages this downloader previously damaged.
/// Malformed data in a declared encoding is an error, never a lossy saved rewrite.
pub(crate) fn decode_text<'a>(
	bytes: &'a [u8],
	content_type: Option<&str>,
	css: bool,
) -> Result<Cow<'a, str>> {
	let (encoding, skip) = if let Some(bom) = Encoding::for_bom(bytes) {
		bom
	} else {
		let transport = content_type.and_then(transport_encoding);
		let declared = if transport.is_some() {
			None
		} else if css {
			let prefix = &bytes[..bytes.len().min(1024)];
			// Charset labels are ASCII even when the stylesheet body is not.
			let (prefix, _) = WINDOWS_1252.decode_without_bom_handling(prefix);
			css_charset(&prefix)
				.and_then(|(label, _)| Encoding::for_label_no_replacement(label.as_bytes()))
				.map(|encoding| {
					if encoding == UTF_16LE || encoding == UTF_16BE {
						UTF_8
					} else {
						encoding
					}
				})
		} else {
			html_encoding(bytes)?
		};
		let fallback = if std::str::from_utf8(bytes).is_ok() {
			UTF_8
		} else {
			WINDOWS_1252
		};
		(transport.or(declared).unwrap_or(fallback), 0)
	};
	let (text, malformed) = encoding.decode_without_bom_handling(&bytes[skip..]);
	ensure!(
		!malformed,
		"archived text is malformed for {}; refusing a lossy rewrite",
		encoding.name()
	);
	Ok(text)
}

/// Gives non-ASCII rewritten files a UTF-8 signature, including HTML without a head/meta tag.
pub(crate) fn utf8_bytes(mut text: String) -> Vec<u8> {
	if !text.is_ascii() && !text.starts_with('\u{feff}') {
		text.insert(0, '\u{feff}');
	}
	text.into_bytes()
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Commented metadata and script strings are not charset declarations.
	#[test]
	fn ignores_false_metadata_and_uses_http_equiv() {
		let bytes = b"<!-- <meta charset='utf-8'> --><script>const fake = '<meta charset=utf-8>';</script><meta content='text/html; charset=windows-1252' HTTP-EQUIV='Content-Type'><p>\x93quoted\x94</p>";
		let text = decode_text(bytes, None, false).unwrap();
		assert!(text.contains("\u{201c}quoted\u{201d}"));
	}

	/// HTTP labels are parsed as MIME parameters rather than split on punctuation.
	#[test]
	fn quoted_transport_charset_decodes_shift_jis() {
		assert_eq!(
			decode_text(
				b"\x93\xfa\x96\x7b",
				Some("text/html; Charset=\"Shift_JIS\""),
				false
			)
			.unwrap(),
			"\u{65e5}\u{672c}"
		);
	}

	/// UTF-8 BOMs take precedence, and output signatures must never accumulate.
	#[test]
	fn utf8_bom_overrides_http_and_output_is_idempotent() {
		let text = "\u{feff}<p>\u{e9}</p>";
		assert_eq!(
			decode_text(
				text.as_bytes(),
				Some("text/html; charset=windows-1251"),
				false
			)
			.unwrap(),
			"<p>\u{e9}</p>"
		);
		assert_eq!(utf8_bytes(text.to_owned()), text.as_bytes());
	}

	/// HTML declarations cannot select UTF-16 without a byte-order mark.
	#[test]
	fn utf16_meta_uses_utf8_and_unknown_labels_do_not_mask_later_metadata() {
		let bytes = "<meta charset=unknown><meta charset=utf-16><p>\u{65e5}</p>";
		assert_eq!(decode_text(bytes.as_bytes(), None, false).unwrap(), bytes);
	}

	/// Unsupported labels fall back without erasing legacy punctuation.
	#[test]
	fn unknown_charset_falls_back_without_replacement() {
		assert_eq!(
			decode_text(b"\x91x\x92", Some("text/html; charset=unknown"), false).unwrap(),
			"\u{2018}x\u{2019}"
		);
	}

	/// A declared encoding mismatch must never silently generate a lossy saved page.
	#[test]
	fn malformed_declared_encoding_is_an_error() {
		assert!(decode_text(b"<p>\xff</p>", Some("text/html; charset=utf-8"), false).is_err());
		assert!(decode_text(&[0xff, 0xfe, 0x00], None, false).is_err());
	}

	/// Metadata outside the HTML prescan window is not used as an encoding guess.
	#[test]
	fn bounds_html_metadata_scan() {
		let mut bytes = vec![b' '; 1024];
		bytes.extend_from_slice(b"<meta charset='utf-8'><p>\x93x\x94</p>");
		assert!(
			decode_text(&bytes, None, false)
				.unwrap()
				.contains("\u{201c}x\u{201d}")
		);
	}

	/// CSS BOM and HTTP precedence are independent of an obsolete @charset rule.
	#[test]
	fn css_honors_transport_and_bom() {
		let css = "\u{feff}@charset \"windows-1252\"; p::before { content: '\u{65e5}'; }";
		assert_eq!(
			decode_text(css.as_bytes(), Some("text/css; charset=windows-1251"), true).unwrap(),
			css.trim_start_matches('\u{feff}')
		);
		assert_eq!(
			decode_text(
				b"@charset \"windows-1252\"; /* \xcf */",
				Some("text/css; charset=windows-1251"),
				true
			)
			.unwrap(),
			"@charset \"windows-1252\"; /* \u{41f} */"
		);
	}
}
