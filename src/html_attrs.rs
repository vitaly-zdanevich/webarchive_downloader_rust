//! Browser-value access for HTML attributes; raw script/style text must bypass these helpers.

use lol_html::errors::AttributeNameError;
use lol_html::html_content::Element;

/// Decodes character references once, before URL, CSS or JavaScript interpretation.
pub(crate) fn decoded_attribute(element: &Element<'_, '_>, name: &str) -> Option<String> {
	element
		.get_attribute(name)
		.map(|value| html_escape::decode_html_entities(&value).into_owned())
}

/// Escapes a browser value for HTML because lol_html does not escape ampersands itself.
pub(crate) fn set_decoded_attribute(
	element: &mut Element<'_, '_>,
	name: &str,
	value: &str,
) -> Result<(), AttributeNameError> {
	element.set_attribute(name, &html_escape::encode_double_quoted_attribute(value))
}
