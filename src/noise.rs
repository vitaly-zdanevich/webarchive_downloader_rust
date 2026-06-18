use url::Url;

use crate::cdx::CdxRecord;
use crate::pathmap::canonical_query_without_volatile_params;

pub fn is_archive_noise_record(record: &CdxRecord) -> bool {
    is_archive_noise_url_with_mimetype(&record.original, &record.mimetype)
}

pub fn is_archive_noise_reference(original: &str) -> bool {
    let mimetype = static_mimetype_hint_from_url(original).unwrap_or("text/html");
    is_archive_noise_url_with_mimetype(original, mimetype)
}

pub fn is_archive_noise_url_with_mimetype(original: &str, mimetype: &str) -> bool {
    let Ok(url) = Url::parse(original) else {
        return false;
    };
    let path = url.path().to_ascii_lowercase();
    if path.starts_with("/cgi-sys/")
        || path.starts_with("/img-sys/")
        || path.starts_with("/sys_cpanel/")
    {
        return true;
    }

    if is_hosting_placeholder_path(&path) {
        return true;
    }

    if is_forum_action_noise_path(&path) {
        return true;
    }

    if is_static_asset_mimetype(mimetype) {
        return false;
    }

    if is_query_required_forum_path(&path) && url.query().is_none() {
        return true;
    }

    let Some(query) = url.query() else {
        return false;
    };
    if query.is_empty() {
        return true;
    }

    if has_interactive_action_query(&url) {
        return true;
    }

    if has_state_changing_query(&url) {
        return true;
    }

    canonical_query_without_volatile_params(&url).is_none()
}

fn static_mimetype_hint_from_url(original: &str) -> Option<&'static str> {
    let url = Url::parse(original).ok()?;
    let extension = url
        .path_segments()?
        .next_back()?
        .rsplit_once('.')?
        .1
        .to_ascii_lowercase();

    match extension.as_str() {
        "css" => Some("text/css"),
        "js" | "mjs" => Some("application/javascript"),
        "gif" => Some("image/gif"),
        "jpg" | "jpeg" | "jpe" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "svg" => Some("image/svg+xml"),
        "webp" => Some("image/webp"),
        "ico" => Some("image/x-icon"),
        "ttf" => Some("font/ttf"),
        "woff" => Some("font/woff"),
        "woff2" => Some("font/woff2"),
        "mp3" | "wav" | "ogg" => Some("audio/mpeg"),
        "mp4" | "webm" | "mov" => Some("video/mp4"),
        "pdf" => Some("application/pdf"),
        "rss" | "atom" | "xml" => Some("application/xml"),
        _ => None,
    }
}

fn is_static_asset_mimetype(mimetype: &str) -> bool {
    let mimetype = mimetype
        .split_once(';')
        .map_or(mimetype, |(base, _)| base)
        .trim()
        .to_ascii_lowercase();

    mimetype.starts_with("image/")
        || mimetype.starts_with("font/")
        || mimetype.starts_with("audio/")
        || mimetype.starts_with("video/")
        || matches!(
            mimetype.as_str(),
            "application/atom+xml"
                | "application/javascript"
                | "application/json"
                | "application/octet-stream"
                | "application/pdf"
                | "application/rss+xml"
                | "application/x-javascript"
                | "application/xml"
                | "text/css"
                | "text/javascript"
                | "text/xml"
        )
}

fn is_forum_action_noise_path(path: &str) -> bool {
    [
        "/cron.php",
        "/groupcp.php",
        "/login.php",
        "/memberlist.php",
        "/posting.php",
        "/profile.php",
        "/search.php",
        "/ucp.php",
        "/viewonline.php",
    ]
    .into_iter()
    .any(|suffix| path.ends_with(suffix))
}

fn is_hosting_placeholder_path(path: &str) -> bool {
    matches!(path, "/welcome.png")
        || path.ends_with("/defaultwebpage.cgi")
        || path.ends_with("/resellerpurchase.cgi")
}

fn is_query_required_forum_path(path: &str) -> bool {
    ["/viewforum.php", "/viewtopic.php"]
        .into_iter()
        .any(|suffix| path.ends_with(suffix))
}

fn has_interactive_action_query(url: &Url) -> bool {
    url.query_pairs().any(|(key, value)| {
        key.eq_ignore_ascii_case("action")
            && matches!(
                value.to_ascii_lowercase().as_str(),
                "add"
                    | "remove"
                    | "delete"
                    | "checkout"
                    | "purchase"
                    | "logout"
                    | "empty"
                    | "setcurrency"
            )
    })
}

fn has_state_changing_query(url: &Url) -> bool {
    url.query_pairs().any(|(key, value)| {
        key.eq_ignore_ascii_case("mark")
            && matches!(
                value.to_ascii_lowercase().as_str(),
                "topics" | "forums" | "all"
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_archive_noise_url(original: &str) -> bool {
        is_archive_noise_url_with_mimetype(original, "text/html")
    }

    #[test]
    fn detects_archive_noise() {
        assert!(is_archive_noise_url(
            "http://smallrockets.com/forums/memberlist.php?first_char=a"
        ));
        assert!(is_archive_noise_url(
            "http://www.smallrockets.com:80/forums/groupcp.php"
        ));
        assert!(is_archive_noise_url(
            "http://www.smallrockets.com:80/forums/login.php"
        ));
        assert!(is_archive_noise_url(
            "http://www.smallrockets.com:80/forums/memberlist.php"
        ));
        assert!(is_archive_noise_url(
            "http://www.smallrockets.com:80/forums/posting.php"
        ));
        assert!(is_archive_noise_url(
            "http://www.smallrockets.com:80/forums/search.php"
        ));
        assert!(is_archive_noise_url(
            "http://www.smallrockets.com:80/forums/viewforum.php"
        ));
        assert!(is_archive_noise_url(
            "http://www.smallrockets.com:80/forums/viewtopic.php"
        ));
        assert!(is_archive_noise_url(
            "http://www.smallrockets.com:80/forums/viewforum.php?"
        ));
        assert!(is_archive_noise_url(
            "http://smallrockets.com/forums/viewtopic.php?sid=abcdef"
        ));
        assert!(is_archive_noise_url(
            "http://smallrockets.com/forums/cron.php?cron_type=tidy_cache"
        ));
        assert!(is_archive_noise_url_with_mimetype(
            "http://example.com/forums/cron.php?cron_type=tidy_cache&sid=abcdef",
            "image/gif"
        ));
        assert!(is_archive_noise_url(
            "http://smallrockets.com/forums/login.php?redirect=posting.php&mode=quote&p=21728"
        ));
        assert!(is_archive_noise_url(
            "http://smallrockets.com/forums/posting.php?mode=reply&t=1"
        ));
        assert!(is_archive_noise_url(
            "http://smallrockets.com/forums/viewforum.php?f=10&mark=topics"
        ));
        assert!(is_archive_noise_url(
            "http://smallrockets.com/forums/index.php?mark=forums"
        ));
        assert!(!is_archive_noise_url(
            "http://smallrockets.com/forums/viewtopic.php?t=10020&view=next"
        ));
        assert!(!is_archive_noise_url(
            "http://smallrockets.com/forums/viewtopic.php?t=1&sid=abcdef"
        ));
        assert!(!is_archive_noise_url(
            "http://smallrockets.com/forums/viewtopic.php?t=1&amp;sid=abcdef"
        ));
        assert!(!is_archive_noise_url(
            "http://smallrockets.com/forums/viewforum.php?f=1&sid=abcdef"
        ));
        assert!(!is_archive_noise_url(
            "http://smallrockets.com/forums/viewtopic.php?p=18411&amp"
        ));
        assert!(!is_archive_noise_url(
            "http://smallrockets.com/forums/viewtopic.php?t=1&highlight=crash"
        ));
        assert!(!is_archive_noise_url_with_mimetype(
            "http://example.com/forums/style.php?sid=abcdef&id=1&lang=en",
            "text/css"
        ));
        assert!(!is_archive_noise_url_with_mimetype(
            "http://example.com/assets/logo.png?sid=abcdef",
            "image/png"
        ));
        assert!(!is_archive_noise_reference(
            "http://example.com/assets/logo.png?sid=abcdef"
        ));
        assert!(is_archive_noise_url(
            "http://smallrockets.com/forums/profile.php?mode=viewprofile&u=2"
        ));
        assert!(is_archive_noise_url(
            "http://smallrockets.com/forums/search.php?search_id=active_topics"
        ));
        assert!(is_archive_noise_url(
            "http://smallrockets.com/basket.htm?action=add&skuid=ArtIsDeadPC1&ticket=0.123"
        ));
        assert!(is_archive_noise_url(
            "http://smallrockets.com/basket.htm?action=empty&ticket=0.123"
        ));
        assert!(is_archive_noise_url(
            "http://smallrockets.com/basket.htm?action=setcurrency&currencyid=2&ticket=0.123"
        ));
        assert!(is_archive_noise_url("http://smallrockets.com/basket.htm?"));
        assert!(is_archive_noise_url(
            "http://smallrockets.com/cgi-sys/defaultwebpage.cgi"
        ));
        assert!(is_archive_noise_url(
            "http://smallrockets.com/img-sys/powered_by_cpanel.png"
        ));
        assert!(is_archive_noise_url(
            "http://smallrockets.com/sys_cpanel/images/bottombody.jpg"
        ));
        assert!(is_archive_noise_url(
            "http://smallrockets.com/cgi-bin/resellerpurchase.cgi"
        ));
        assert!(is_archive_noise_url("http://smallrockets.com/welcome.png"));
        assert!(!is_archive_noise_url(
            "http://smallrockets.com/forums/viewtopic.php?t=1"
        ));
        assert!(!is_archive_noise_url(
            "http://smallrockets.com/bimages/ic_logo.gif"
        ));
    }
}
