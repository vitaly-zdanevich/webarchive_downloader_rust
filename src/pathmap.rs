use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use url::{Url, form_urlencoded};

use crate::cdx::CdxRecord;

#[derive(Clone, Debug)]
pub struct SiteMapper {
    root_host: String,
    primary_hosts: Vec<String>,
}

impl SiteMapper {
    pub fn new(input: &str) -> Result<Self> {
        let url = parse_user_url(input)?;
        let host = normalize_host(url.host_str().context("target URL must contain a host")?);

        let mut primary_hosts = vec![host.clone()];
        if let Some(stripped) = host.strip_prefix("www.") {
            primary_hosts.push(stripped.to_owned());
        } else {
            primary_hosts.push(format!("www.{host}"));
        }
        primary_hosts.sort();
        primary_hosts.dedup();

        Ok(Self {
            root_host: host,
            primary_hosts,
        })
    }

    pub fn cdx_target(&self) -> &str {
        &self.root_host
    }

    pub fn primary_hosts(&self) -> &[String] {
        &self.primary_hosts
    }

    pub fn is_primary_host(&self, host: &str) -> bool {
        let host = normalize_host(host);
        self.primary_hosts
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(&host))
    }

    pub fn is_related_host(&self, host: &str) -> bool {
        let host = normalize_host(host);
        if self.is_primary_host(&host) {
            return true;
        }

        let base = self
            .root_host
            .strip_prefix("www.")
            .unwrap_or(&self.root_host);
        host.ends_with(&format!(".{base}"))
    }

    pub fn local_path_for_url(&self, original: &str, mimetype: &str) -> Result<PathBuf> {
        let url =
            Url::parse(original).with_context(|| format!("invalid archived URL: {original}"))?;
        let host = normalize_host(url.host_str().unwrap_or("unknown-host"));

        let mut path = PathBuf::new();
        if !self.is_primary_host(&host) {
            path.push("_hosts");
            path.push(sanitize_segment(&host));
        }

        let path_segments = url
            .path_segments()
            .map(|segments| segments.collect::<Vec<_>>())
            .unwrap_or_default();

        let mut sanitized_segments = path_segments
            .into_iter()
            .filter(|segment| !segment.is_empty())
            .map(sanitize_segment)
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();

        let should_be_html = is_html_mimetype(mimetype);
        let original_path = url.path();
        let needs_index_file = sanitized_segments.is_empty()
            || original_path.ends_with('/')
            || (should_be_html && extension_of(sanitized_segments.last().unwrap()).is_none());
        if needs_index_file {
            sanitized_segments.push("index.html".to_owned());
        } else if let Some(last_segment) = sanitized_segments.last_mut() {
            let current_extension = extension_of(last_segment).map(str::to_owned);
            if should_be_html && !current_extension.as_deref().is_some_and(is_html_extension) {
                last_segment.push_str(".html");
            } else if let Some(extension) = extension_for_mimetype(mimetype)
                && should_append_mimetype_extension(current_extension.as_deref(), extension)
            {
                last_segment.push('.');
                last_segment.push_str(extension);
            }
        }

        for segment in sanitized_segments {
            path.push(segment);
        }

        if let Some(query) = canonical_query_without_volatile_params(&url) {
            append_query_hash(&mut path, &query);
        }

        Ok(path)
    }

    pub fn records_to_paths(&self, records: &[CdxRecord]) -> Result<HashMap<String, PathBuf>> {
        let mut paths = HashMap::with_capacity(records.len());
        for record in records {
            let key = normalize_lookup_url(&record.original);
            let local_path = self.local_path_for_url(&record.original, &record.mimetype)?;
            paths.insert(key, local_path);
        }
        Ok(paths)
    }

    pub fn original_url_candidates_for_local_path(&self, local_path: &Path) -> Vec<String> {
        let mut components = local_path
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(component) => {
                    Some(component.to_string_lossy().into_owned())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if components.is_empty() {
            return Vec::new();
        }

        let hosts = if components.len() >= 3 && components[0] == "_hosts" {
            let host = components.remove(1);
            components.remove(0);
            vec![host]
        } else {
            self.primary_hosts.clone()
        };

        let mut candidates = Vec::new();
        for host in hosts {
            for scheme in ["http", "https"] {
                if let Some(candidate) = build_original_url_candidate(scheme, &host, &components) {
                    if scheme == "http" {
                        if let Some(path) = candidate.strip_prefix(&format!("http://{host}/")) {
                            candidates.push(format!("http://{host}:80/{path}"));
                        }
                    }
                    candidates.push(candidate);
                }
            }
        }
        candidates.sort();
        candidates.dedup();
        candidates
    }
}

fn build_original_url_candidate(scheme: &str, host: &str, components: &[String]) -> Option<String> {
    let mut url = Url::parse(&format!("{scheme}://{host}/")).ok()?;
    url.path_segments_mut()
        .ok()?
        .extend(components.iter().map(String::as_str));
    Some(url.to_string())
}

pub fn parse_user_url(input: &str) -> Result<Url> {
    match Url::parse(input) {
        Ok(url) => Ok(url),
        Err(url::ParseError::RelativeUrlWithoutBase) => Url::parse(&format!("http://{input}"))
            .with_context(|| format!("invalid target: {input}")),
        Err(error) => Err(error).with_context(|| format!("invalid target: {input}")),
    }
}

pub fn normalize_lookup_url(input: &str) -> String {
    let Ok(mut url) = Url::parse(input) else {
        return input.to_owned();
    };
    url.set_fragment(None);
    if let Some(host) = url.host_str() {
        let host = normalize_host(host);
        let _ = url.set_host(Some(&host));
    }
    let canonical_query = canonical_query_without_volatile_params(&url);
    url.set_query(canonical_query.as_deref());
    url.to_string()
}

pub fn canonical_query_without_volatile_params(url: &Url) -> Option<String> {
    let query = url.query()?;
    if query.is_empty() {
        return None;
    }

    let mut pairs = url
        .query_pairs()
        .filter_map(|(key, value)| {
            let key = normalized_query_key(&key).to_owned();
            if is_volatile_query_key(&key) {
                None
            } else {
                Some((key, value.into_owned()))
            }
        })
        .collect::<Vec<_>>();

    if pairs.is_empty() {
        return None;
    }

    pairs.sort();

    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(&key, &value);
    }
    Some(serializer.finish())
}

pub fn is_volatile_query_key(key: &str) -> bool {
    let key = normalized_query_key(key).to_ascii_lowercase();
    matches!(
        key.as_str(),
        "sid"
            | "sessionid"
            | "session_id"
            | "phpsessid"
            | "jsessionid"
            | "aspsessionid"
            | "cfid"
            | "cftoken"
            | "cron_type"
            | "amp"
            | "highlight"
            | "mark"
            | "postdays"
            | "postorder"
            | "sd"
            | "sk"
            | "ticket"
            | "st"
            | "view"
    )
}

fn normalized_query_key(key: &str) -> &str {
    if key
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("amp;"))
    {
        &key[4..]
    } else {
        key
    }
}

fn normalize_host(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

pub fn relative_link(from_file: &Path, to_file: &Path) -> String {
    let from_dir = from_file.parent().unwrap_or_else(|| Path::new(""));
    let from_components = from_dir.components().collect::<Vec<_>>();
    let to_components = to_file.components().collect::<Vec<_>>();

    let mut common = 0;
    while common < from_components.len()
        && common < to_components.len()
        && from_components[common] == to_components[common]
    {
        common += 1;
    }

    let mut parts = Vec::new();
    for _ in common..from_components.len() {
        parts.push("..".to_owned());
    }
    for component in &to_components[common..] {
        parts.push(component.as_os_str().to_string_lossy().replace('\\', "/"));
    }

    if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    }
}

pub fn is_html_mimetype(mimetype: &str) -> bool {
    mimetype.eq_ignore_ascii_case("text/html")
        || mimetype.eq_ignore_ascii_case("application/xhtml+xml")
}

pub fn is_css_mimetype(mimetype: &str) -> bool {
    mimetype.eq_ignore_ascii_case("text/css")
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

fn extension_of(segment: &str) -> Option<&str> {
    segment.rsplit_once('.').and_then(|(_, extension)| {
        if extension.is_empty() {
            None
        } else {
            Some(extension)
        }
    })
}

fn is_html_extension(extension: &str) -> bool {
    extension.eq_ignore_ascii_case("html")
        || extension.eq_ignore_ascii_case("htm")
        || extension.eq_ignore_ascii_case("xhtml")
}

fn extension_for_mimetype(mimetype: &str) -> Option<&'static str> {
    match mimetype
        .split_once(';')
        .map_or(mimetype, |(base, _)| base)
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "application/atom+xml" | "application/rss+xml" | "application/xml" | "text/xml" => {
            Some("xml")
        }
        "application/javascript" | "application/x-javascript" | "text/javascript" => Some("js"),
        "application/json" => Some("json"),
        "application/pdf" => Some("pdf"),
        "font/ttf" => Some("ttf"),
        "font/woff" => Some("woff"),
        "font/woff2" => Some("woff2"),
        "image/gif" => Some("gif"),
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/svg+xml" => Some("svg"),
        "image/webp" => Some("webp"),
        "image/x-icon" => Some("ico"),
        "text/css" => Some("css"),
        "text/plain" => Some("txt"),
        _ => None,
    }
}

fn should_append_mimetype_extension(
    current_extension: Option<&str>,
    mimetype_extension: &str,
) -> bool {
    !current_extension
        .is_some_and(|extension| extension_matches_mimetype(extension, mimetype_extension))
}

fn extension_matches_mimetype(extension: &str, mimetype_extension: &str) -> bool {
    if extension.eq_ignore_ascii_case(mimetype_extension) {
        return true;
    }

    match mimetype_extension {
        "jpg" => extension.eq_ignore_ascii_case("jpeg") || extension.eq_ignore_ascii_case("jpe"),
        "js" => extension.eq_ignore_ascii_case("mjs"),
        "xml" => extension.eq_ignore_ascii_case("rss") || extension.eq_ignore_ascii_case("atom"),
        _ => false,
    }
}

fn sanitize_segment(segment: &str) -> String {
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

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_primary_host_root_to_index() {
        let mapper = SiteMapper::new("example.com").unwrap();
        assert_eq!(
            mapper
                .local_path_for_url("http://example.com/", "text/html")
                .unwrap(),
            PathBuf::from("index.html")
        );
    }

    #[test]
    fn maps_extensionless_html_to_directory_index() {
        let mapper = SiteMapper::new("example.com").unwrap();
        assert_eq!(
            mapper
                .local_path_for_url("http://www.example.com/about", "text/html")
                .unwrap(),
            PathBuf::from("about/index.html")
        );
    }

    #[test]
    fn maps_trailing_dot_primary_host_to_root() {
        let mapper = SiteMapper::new("example.com").unwrap();
        assert_eq!(
            mapper
                .local_path_for_url("http://www.example.com.:80/", "text/html")
                .unwrap(),
            PathBuf::from("index.html")
        );
    }

    #[test]
    fn detects_related_subdomains() {
        let mapper = SiteMapper::new("example.com").unwrap();

        assert!(mapper.is_related_host("downloads.example.com"));
        assert!(mapper.is_related_host("www.example.com"));
        assert!(!mapper.is_related_host("notexample.com"));
    }

    #[test]
    fn maps_assets_without_forcing_index() {
        let mapper = SiteMapper::new("example.com").unwrap();
        assert_eq!(
            mapper
                .local_path_for_url("http://example.com/css/site.css", "text/css")
                .unwrap(),
            PathBuf::from("css/site.css")
        );
    }

    #[test]
    fn adds_mimetype_extension_to_extensionless_feed() {
        let mapper = SiteMapper::new("example.com").unwrap();
        assert_eq!(
            mapper
                .local_path_for_url("http://example.com/rss", "application/rss+xml")
                .unwrap(),
            PathBuf::from("rss.xml")
        );
    }

    #[test]
    fn appends_html_to_html_slugs_with_non_html_extensions() {
        let mapper = SiteMapper::new("example.com").unwrap();
        assert_eq!(
            mapper
                .local_path_for_url("http://example.com/catalog/tag/another.by", "text/html")
                .unwrap(),
            PathBuf::from("catalog/tag/another.by.html")
        );
    }

    #[test]
    fn puts_non_primary_hosts_under_hosts_directory() {
        let mapper = SiteMapper::new("example.com").unwrap();
        assert_eq!(
            mapper
                .local_path_for_url(
                    "https://cdn.example.com/lib/app.js",
                    "application/javascript"
                )
                .unwrap(),
            PathBuf::from("_hosts/cdn.example.com/lib/app.js")
        );
    }

    #[test]
    fn builds_original_url_candidates_for_primary_local_paths() {
        let mapper = SiteMapper::new("example.com").unwrap();

        assert_eq!(
            mapper.original_url_candidates_for_local_path(Path::new("shared/logo.gif")),
            vec![
                "http://example.com/shared/logo.gif".to_owned(),
                "http://example.com:80/shared/logo.gif".to_owned(),
                "http://www.example.com/shared/logo.gif".to_owned(),
                "http://www.example.com:80/shared/logo.gif".to_owned(),
                "https://example.com/shared/logo.gif".to_owned(),
                "https://www.example.com/shared/logo.gif".to_owned(),
            ]
        );
    }

    #[test]
    fn builds_original_url_candidates_for_host_local_paths() {
        let mapper = SiteMapper::new("example.com").unwrap();

        assert_eq!(
            mapper.original_url_candidates_for_local_path(Path::new(
                "_hosts/downloads.example.com/file.exe"
            )),
            vec![
                "http://downloads.example.com/file.exe".to_owned(),
                "http://downloads.example.com:80/file.exe".to_owned(),
                "https://downloads.example.com/file.exe".to_owned(),
            ]
        );
    }

    #[test]
    fn appends_stable_query_hash() {
        let mapper = SiteMapper::new("example.com").unwrap();
        let path = mapper
            .local_path_for_url("http://example.com/css/site.css?v=1", "text/css")
            .unwrap();

        assert!(path.to_string_lossy().starts_with("css/site__q_"));
        assert!(path.to_string_lossy().ends_with(".css"));
    }

    #[test]
    fn maps_dynamic_css_to_static_css_and_ignores_session_query() {
        let mapper = SiteMapper::new("example.com").unwrap();
        let stable_path = mapper
            .local_path_for_url(
                "http://example.com/forums/style.php?id=1&lang=en",
                "text/css",
            )
            .unwrap();
        let session_path = mapper
            .local_path_for_url(
                "http://example.com/forums/style.php?sid=abc&id=1&lang=en",
                "text/css",
            )
            .unwrap();

        assert_eq!(session_path, stable_path);
        assert!(
            stable_path
                .to_string_lossy()
                .starts_with("forums/style.php__q_")
        );
        assert!(stable_path.to_string_lossy().ends_with(".css"));
    }

    #[test]
    fn normalizes_lookup_url_host_trailing_dot() {
        assert_eq!(
            normalize_lookup_url("http://www.example.com.:80/about#section"),
            "http://www.example.com/about"
        );
    }

    #[test]
    fn normalize_lookup_url_drops_volatile_query_parameters() {
        assert_eq!(
            normalize_lookup_url("http://example.com/forums/style.php?sid=abc&id=1&lang=en"),
            "http://example.com/forums/style.php?id=1&lang=en"
        );
        assert_eq!(
            normalize_lookup_url("http://example.com/forums/viewtopic.php?t=1&amp;sid=abc"),
            "http://example.com/forums/viewtopic.php?t=1"
        );
    }

    #[test]
    fn computes_relative_links() {
        assert_eq!(
            relative_link(
                Path::new("about/team/index.html"),
                Path::new("css/site.css")
            ),
            "../../css/site.css"
        );
        assert_eq!(
            relative_link(Path::new("index.html"), Path::new("about/index.html")),
            "about/index.html"
        );
    }
}
