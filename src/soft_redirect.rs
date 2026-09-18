use std::cell::{Cell, RefCell};
use lol_html::{HtmlRewriter, Settings, element, text};

/// Detects redirects and structured error pages without rejecting quoted discussion text.
pub fn is_unusable_html_capture(input: &str) -> bool {
	let (discussion, forum_error) = forum_page_signals(input);
	if discussion {
		return false;
	}
	forum_error || is_soft_redirect_html(input) || is_access_denied_placeholder_html(input)
}

/// Recognizes short redirect-only documents rather than ordinary timed refreshes.
pub fn is_soft_redirect_html(input: &str) -> bool {
    has_soft_redirect_marker(input) && visible_text_len(input) <= 256
}

/// Parses phpBB message containers independently of surrounding navigation and CSS.
fn forum_page_signals(input: &str) -> (bool, bool) {
	let discussion = Cell::new(false);
	let messages = RefCell::new(String::new());
	let settings = Settings {
		element_content_handlers: vec![
			element!(".postbody, .post, blockquote, .quote", |_| {
				discussion.set(true);
				Ok(())
			}),
			text!("td[align=center] > span.gen, #message .content, .error", |chunk| {
				messages.borrow_mut().push_str(chunk.as_str());
				Ok(())
			}),
		],
		..Settings::default()
	};
	let mut parser = HtmlRewriter::new(settings, |_: &[u8]| {});
	if parser.write(input.as_bytes()).and_then(|_| parser.end()).is_err() {
		return (discussion.get(), false);
	}
	let message = html_escape::decode_html_entities(messages.borrow().as_str()).split_whitespace()
		.collect::<Vec<_>>().join(" ").to_ascii_lowercase();
	let error = [
		"the topic or post you requested does not exist",
		"the requested topic does not exist",
		"sorry, but that user does not exist",
		"the requested user does not exist",
		"the forum you selected does not exist",
		"the requested forum does not exist",
		"you are not authorised to read this forum",
		"you are not authorized to read this forum",
		"only users granted special access can read topics in this forum",
	].iter().any(|phrase| message.contains(phrase));
	(discussion.get(), error)
}

fn has_soft_redirect_marker(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    let has_meta_refresh =
        lower.contains("http-equiv") && lower.contains("refresh") && lower.contains("url=");
    let has_script_redirect = lower.contains("window.location")
        || lower.contains("document.location")
        || lower.contains("location.href")
        || lower.contains("location.replace");

    has_meta_refresh || has_script_redirect
}

fn is_access_denied_placeholder_html(input: &str) -> bool {
    let lower = input.to_ascii_lowercase();
    let short_access_denied = visible_text_len(input) <= 768
        && [
            "you are not authorised to read this forum",
            "you are not authorized to read this forum",
            "only users granted special access can read topics in this forum",
            "the forum you selected does not exist",
        ]
        .into_iter()
        .any(|phrase| lower.contains(phrase));

    let login_wall = lower
        .contains("requires you to be registered and logged in to view this forum")
        || lower.contains("must be registered and logged in to view this forum");

    short_access_denied
        || (login_wall
            && lower.contains("<form")
            && (lower.contains("name=\"login\"") || lower.contains("id=\"login\"")))
}

fn visible_text_len(input: &str) -> usize {
    let mut in_tag = false;
    let mut count = 0;

    for character in input.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            character if !in_tag && !character.is_whitespace() => count += 1,
            _ => {}
        }
    }

    count
}

#[cfg(test)]
mod tests {
    use super::*;

	/// Full phpBB templates must not hide missing-content messages behind CSS/navigation.
	#[test]
	fn detects_structured_forum_errors_in_large_templates() {
		for message in [
			"The topic or post you requested does not exist",
			"Sorry, but that user does not exist.",
			"The forum you selected does not exist.",
		] {
			let html = format!("<html><head><style>{}</style></head><body><nav>{}</nav><table><tr><td align='center'><span class='gen'>{message}</span></td></tr></table></body></html>", "body {{ color: black; }}".repeat(100), "Navigation ".repeat(100));
			assert!(is_unusable_html_capture(&html), "{message}");
		}
	}

	/// A user's quotation of a forum error is content, not a failed capture.
	#[test]
	fn keeps_discussions_quoting_forum_errors() {
		for container in ["<div class='postbody'>{}</div>", "<blockquote>{}</blockquote>"] {
			let html = container.replace("{}", "<table><tr><td align='center'><span class='gen'>The forum you selected does not exist.</span></td></tr></table>");
			assert!(!is_unusable_html_capture(&html));
		}
	}

    #[test]
    fn detects_meta_refresh_redirect() {
        assert!(is_soft_redirect_html(
            r#"<html><head><META HTTP-EQUIV="refresh" CONTENT="0;URL=/cgi-sys/defaultwebpage.cgi"></head><body></body></html>"#
        ));
    }

    #[test]
    fn detects_script_redirect() {
        assert!(is_soft_redirect_html(
            r#"<html><body><script>window.location = "/old";</script></body></html>"#
        ));
    }

    #[test]
    fn does_not_treat_real_content_as_redirect() {
        let body = format!(
            r#"<html><head><meta http-equiv="refresh" content="30;url=/next"></head><body>{}</body></html>"#,
            "real content ".repeat(40)
        );

        assert!(!is_soft_redirect_html(&body));
    }

    #[test]
    fn detects_short_forum_access_denied_placeholder() {
        assert!(is_unusable_html_capture(
            r#"<html><body><h2>Information</h2><p>You are not authorised to read this forum.</p></body></html>"#
        ));
    }

    #[test]
    fn detects_forum_login_wall_placeholder() {
        assert!(is_unusable_html_capture(
            r#"<html><body><form method="post" id="login"><h2>The board requires you to be registered and logged in to view this forum.</h2></form></body></html>"#
        ));
    }

    #[test]
    fn does_not_treat_long_real_content_as_placeholder() {
        let body = format!(
            r#"<html><body><p>You are not authorised to read this forum.</p><p>{}</p></body></html>"#,
            "real discussion content ".repeat(80)
        );

        assert!(!is_unusable_html_capture(&body));
    }
}
