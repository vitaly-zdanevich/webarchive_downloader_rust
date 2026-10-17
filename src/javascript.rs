//! Parses static JavaScript strings without confusing comments, regexes or template text with code.

use std::collections::HashSet;
use std::ops::Range;

use lol_html::html_content::Element;
use oxc_allocator::Allocator;
use oxc_ast::ast::{BinaryExpression, Expression, StringLiteral};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::{ParseOptions, Parser};
use oxc_span::SourceType;

/// A decoded, well-formed string with its exact source range and original quote style.
pub(crate) struct StaticString {
	pub range: Range<usize>,
	pub quote: char,
	pub value: String,
	/// A concatenation operand is a URL fragment, not a standalone resource reference.
	pub is_concatenated: bool,
}

/// Collects literals in source order; unsupported or invalid code is left untouched.
///
/// Oxc decodes JavaScript escapes, including legacy octal and line continuations.
/// Lone UTF-16 surrogates cannot round-trip through UTF-8 and are deliberately skipped.
pub(crate) fn strings(input: &str) -> Vec<StaticString> {
	let allocator = Allocator::default();
	let parsed = Parser::new(&allocator, input, SourceType::unambiguous())
		.with_options(ParseOptions {
			allow_return_outside_function: true,
			..ParseOptions::default()
		})
		.parse();
	if parsed.fatal_error || !parsed.diagnostics.is_empty() {
		return Vec::new();
	}
	let mut collector = StringCollector {
		input,
		strings: Vec::new(),
		concatenated: HashSet::new(),
	};
	collector.visit_program(&parsed.program);
	collector.strings.sort_by_key(|string| string.range.start);
	collector.strings
}

/// Visits only actual string literals, not lexical lookalikes.
struct StringCollector<'s> {
	input: &'s str,
	strings: Vec<StaticString>,
	concatenated: HashSet<u32>,
}

impl StringCollector<'_> {
	/// Marks direct string operands without suppressing complete URLs in nested function calls.
	fn mark_concatenated_operand(&mut self, expression: &Expression<'_>) {
		match expression {
			Expression::StringLiteral(literal) => {
				self.concatenated.insert(literal.span.start);
			}
			Expression::ParenthesizedExpression(parenthesized) => {
				self.mark_concatenated_operand(&parenthesized.expression);
			}
			_ => {}
		}
	}
}

impl<'a> Visit<'a> for StringCollector<'_> {
	fn visit_binary_expression(&mut self, expression: &BinaryExpression<'a>) {
		if expression.operator.as_str() == "+" {
			self.mark_concatenated_operand(&expression.left);
			self.mark_concatenated_operand(&expression.right);
		}
		walk::walk_binary_expression(self, expression);
	}

	fn visit_string_literal(&mut self, literal: &StringLiteral<'a>) {
		let range = literal.span.start as usize..literal.span.end as usize;
		let Some(raw) = self.input.get(range.clone()) else {
			return;
		};
		let Some(quote @ ('\'' | '"')) = raw.chars().next() else {
			return;
		};
		if !literal.lone_surrogates {
			self.strings.push(StaticString {
				range,
				quote,
				value: literal.value.to_string(),
				is_concatenated: self.concatenated.contains(&literal.span.start),
			});
		}
	}
}

/// Serializes a runtime value back to a JavaScript literal safe inside HTML raw text.
///
/// Only edited literals are serialized; all surrounding source remains byte-for-byte intact.
pub(crate) fn quote(value: &str, delimiter: char) -> String {
	use std::fmt::Write;
	let mut output = String::with_capacity(value.len() + 2);
	output.push(delimiter);
	for (offset, character) in value.char_indices() {
		match character {
			'\\' => output.push_str("\\\\"),
			'\n' => output.push_str("\\n"),
			'\r' => output.push_str("\\r"),
			'\t' => output.push_str("\\t"),
			'\u{2028}' => output.push_str("\\u2028"),
			'\u{2029}' => output.push_str("\\u2029"),
			character if character == delimiter => {
				output.push('\\');
				output.push(character);
			}
			'<' if value[offset..]
				.get(..8)
				.is_some_and(|text| text.eq_ignore_ascii_case("</script"))
				|| value[offset..]
					.get(..7)
					.is_some_and(|text| text.eq_ignore_ascii_case("<script"))
				|| value[offset..].starts_with("<!--") =>
			{
				output.push_str("\\x3c")
			}
			character if character.is_control() => {
				write!(output, "\\u{:04x}", character as u32)
					.expect("writing to a String cannot fail");
			}
			character => output.push(character),
		}
	}
	output.push(delimiter);
	output
}

/// Recognizes HTML JavaScript MIME types while excluding JSON, templates and other data blocks.
pub(crate) fn is_javascript_element(element: &Element<'_, '_>) -> bool {
	let kind = element.get_attribute("type").unwrap_or_default();
	let kind = kind.trim();
	if kind.is_empty() || kind.eq_ignore_ascii_case("module") {
		return true;
	}
	let Ok(kind) = kind.parse::<mime::Mime>() else {
		return false;
	};
	matches!(
		kind.essence_str(),
		"application/ecmascript"
			| "application/javascript"
			| "application/x-ecmascript"
			| "application/x-javascript"
			| "text/ecmascript"
			| "text/javascript"
			| "text/javascript1.0"
			| "text/javascript1.1"
			| "text/javascript1.2"
			| "text/javascript1.3"
			| "text/javascript1.4"
			| "text/javascript1.5"
			| "text/jscript"
			| "text/livescript"
			| "text/x-ecmascript"
			| "text/x-javascript"
	)
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Both delimiter styles must preserve the exact runtime value, including control characters.
	#[test]
	fn serialization_round_trips_all_sensitive_characters() {
		let value = "a'\"\\\n\r\t\0\u{0008}\u{000c}\u{001f}\u{007f}\u{2028}\u{2029}\u{1f680}<SCRIPT>text</ScRiPt><!--";
		for delimiter in ['\'', '"'] {
			let literal = quote(value, delimiter);
			assert!(!literal.to_ascii_lowercase().contains("<script"));
			assert!(!literal.to_ascii_lowercase().contains("</script"));
			assert!(!literal.contains("<!--"));
			let parsed = strings(&format!("consume({literal});"));
			assert_eq!(parsed.len(), 1, "{literal}");
			assert_eq!(parsed[0].value, value);
		}
	}

	/// The parser resolves paired surrogates and continuations but does not lose lone surrogates.
	#[test]
	fn decoded_literals_preserve_unicode_and_legacy_escapes() {
		let input = "consume('\\uD83D\\uDE80', '\\u{1f680}', '/im\\\r\ng/logo.gif', '\\x2fimg/logo.gif', '\\057img/logo.gif', '\\uD800');";
		let values = strings(input)
			.into_iter()
			.map(|literal| literal.value)
			.collect::<Vec<_>>();
		assert_eq!(
			values,
			vec![
				"\u{1f680}",
				"\u{1f680}",
				"/img/logo.gif",
				"/img/logo.gif",
				"/img/logo.gif"
			]
		);
	}
}
