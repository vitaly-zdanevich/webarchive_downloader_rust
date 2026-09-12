//! Token-based CSS reference discovery shared by validation and rewriting.

use std::fmt::Write;
use std::ops::Range;

use cssparser::{CssStringWriter, Parser, ParserInput, ToCss, Token};

/// A decoded URL and the source token that can be replaced without reformatting its rule.
pub(crate) struct CssReference {
	pub(crate) value: String,
	pub(crate) range: Range<usize>,
	quote: Option<char>,
}

impl CssReference {
	/// Escapes a replacement URL while preserving string quotes when present.
	pub(crate) fn replacement(&self, value: &str) -> String {
		match self.quote {
			Some(quote) => {
				let mut escaped = String::new();
				CssStringWriter::new(&mut escaped).write_str(value).unwrap();
				if quote == '\'' {
					escaped = escaped.replace('\'', "\\'");
				}
				format!("{quote}{escaped}{quote}")
			}
			None => Token::UnquotedUrl(value.into()).to_css_string(),
		}
	}
}

/// Finds `url(...)` and quoted `@import` references, including nested rules.
pub(crate) fn references(input: &str) -> Vec<CssReference> {
	let mut parser_input = ParserInput::new(input);
	let mut parser = Parser::new(&mut parser_input);
	let mut references = Vec::new();
	collect(&mut parser, input, false, &mut references);
	references
}

/// Walks CSS blocks without interpreting URL-looking comments or ordinary strings as links.
fn collect(
	parser: &mut Parser<'_, '_>,
	input: &str,
	url_string: bool,
	refs: &mut Vec<CssReference>,
) {
	let mut import_string = false;
	loop {
		let start = parser.position().byte_index();
		let Ok(token) = parser.next_including_whitespace_and_comments().cloned() else {
			break;
		};
		let end = parser.position().byte_index();
		match token {
			Token::WhiteSpace(_) | Token::Comment(_) => continue,
			Token::AtKeyword(name) if name.eq_ignore_ascii_case("import") => {
				import_string = true;
				continue;
			}
			Token::UnquotedUrl(value) => refs.push(CssReference {
				value: value.to_string(),
				range: start..end,
				quote: None,
			}),
			Token::QuotedString(value) if import_string || url_string => refs.push(CssReference {
				value: value.to_string(),
				range: start..end,
				quote: input[start..end].chars().next(),
			}),
			Token::Function(_)
			| Token::CurlyBracketBlock
			| Token::SquareBracketBlock
			| Token::ParenthesisBlock => {
				let is_url =
					matches!(&token, Token::Function(name) if name.eq_ignore_ascii_case("url"));
				let _ = parser.parse_nested_block(|nested| {
					collect(nested, input, is_url, refs);
					Ok::<_, cssparser::ParseError<'_, ()>>(())
				});
			}
			_ => {}
		}
		import_string = false;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Escaped delimiters, imports, comments and nested declarations use CSS token boundaries.
	#[test]
	fn discovers_real_css_references() {
		let input = r#"@import /* url(fake) */ 'theme.css'; @media screen { .x { background: URL('image).png'); mask: url(mask\).svg); content: 'url(fake)'; } }"#;
		let refs = references(input);
		assert_eq!(
			refs.iter()
				.map(|reference| reference.value.as_str())
				.collect::<Vec<_>>(),
			vec!["theme.css", "image).png", "mask).svg"]
		);
		for reference in refs {
			assert!(!input[reference.range].is_empty());
		}
	}

	/// Replacements must round-trip even when a local path contains CSS delimiters.
	#[test]
	fn escapes_replacement_values() {
		for css in ["url(old)", "url('old')", "@import \"old\";"] {
			let reference = references(css).remove(0);
			let value = "new'\"\\)\n.png";
			let rewritten = format!(
				"{}{}{}",
				&css[..reference.range.start],
				reference.replacement(value),
				&css[reference.range.end..]
			);
			assert_eq!(references(&rewritten)[0].value, value);
		}
	}
}
