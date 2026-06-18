use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AliasRepairReport {
    pub created: usize,
    pub unresolved: usize,
    pub ambiguous: usize,
}

pub fn create_missing_topic_aliases(root: &Path) -> Result<AliasRepairReport> {
    let html_files = collect_html_files(root)?;
    let title_index = build_topic_title_index(root, &html_files)?;
    let post_index = build_post_anchor_index(root, &html_files)?;
    let mut report = AliasRepairReport::default();

    for file in &html_files {
        let input = read_lossy(file)?;
        let current_topic_title = extract_topic_title(&input).map(|title| normalize_text(&title));
        let current_anchors = extract_anchor_ids(&input)
            .into_iter()
            .collect::<HashSet<_>>();
        for link in extract_anchor_links(&input) {
            let Some(target) = missing_topic_target(root, file, &link.href)? else {
                continue;
            };
            if target.exists() {
                continue;
            }
            let source = match alias_source_for_link(
                root,
                file,
                &input,
                &current_anchors,
                current_topic_title.as_deref(),
                &link,
                &title_index,
                &post_index,
            ) {
                AliasSource::Resolved(source) => source,
                AliasSource::Unresolved => {
                    report.unresolved += 1;
                    continue;
                }
                AliasSource::Ambiguous => {
                    report.ambiguous += 1;
                    continue;
                }
            };

            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            let source = root.join(source);
            fs::copy(&source, &target).with_context(|| {
                format!(
                    "failed to copy topic alias {} to {}",
                    source.display(),
                    target.display()
                )
            })?;
            report.created += 1;
        }
    }

    Ok(report)
}

fn collect_html_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_html_files_into(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_html_files_into(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry.with_context(|| format!("failed to read entry in {}", path.display()))?;
        let path = entry.path();
        let metadata = entry
            .metadata()
            .with_context(|| format!("failed to read metadata for {}", path.display()))?;
        if metadata.is_dir() {
            collect_html_files_into(&path, files)?;
        } else if metadata.is_file() && is_html_file(&path) {
            files.push(path);
        }
    }
    Ok(())
}

fn build_topic_title_index(
    root: &Path,
    html_files: &[PathBuf],
) -> Result<HashMap<String, Vec<PathBuf>>> {
    let mut index: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for file in html_files {
        if !file
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("viewtopic.php"))
        {
            continue;
        }

        let input = read_lossy(file)?;
        let Some(title) = extract_topic_title(&input) else {
            continue;
        };
        let title = normalize_text(&title);
        if title.is_empty() {
            continue;
        }

        let relative = file
            .strip_prefix(root)
            .unwrap_or(file.as_path())
            .to_path_buf();
        index.entry(title).or_default().push(relative);
    }

    for candidates in index.values_mut() {
        candidates.sort();
        candidates.dedup();
    }
    Ok(index)
}

fn build_post_anchor_index(
    root: &Path,
    html_files: &[PathBuf],
) -> Result<HashMap<String, Vec<PathBuf>>> {
    let mut index: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for file in html_files {
        if !file
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("viewtopic.php"))
        {
            continue;
        }

        let input = read_lossy(file)?;
        let relative = file
            .strip_prefix(root)
            .unwrap_or(file.as_path())
            .to_path_buf();
        for anchor in extract_anchor_ids(&input) {
            index.entry(anchor).or_default().push(relative.clone());
        }
    }

    for candidates in index.values_mut() {
        candidates.sort();
        candidates.dedup();
    }
    Ok(index)
}

fn extract_topic_title(input: &str) -> Option<String> {
    let title = extract_tag_text(input, "title")?;
    let lower = title.to_ascii_lowercase();
    let marker = "view topic -";
    let start = lower.find(marker)? + marker.len();
    Some(title[start..].trim().to_owned())
}

fn missing_topic_target(root: &Path, from_file: &Path, href: &str) -> Result<Option<PathBuf>> {
    if should_skip_reference(href) {
        return Ok(None);
    }

    let target = resolve_local_reference(root, from_file, href)?;
    if !target.starts_with(&normalize_path(root)) {
        return Ok(None);
    }
    if target.exists() {
        return Ok(None);
    }

    if target
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("viewtopic.php__q_") && name.ends_with(".html"))
    {
        Ok(Some(target))
    } else {
        Ok(None)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AliasSource {
    Resolved(PathBuf),
    Unresolved,
    Ambiguous,
}

fn alias_source_for_link(
    root: &Path,
    file: &Path,
    input: &str,
    current_anchors: &HashSet<String>,
    current_topic_title: Option<&str>,
    link: &AnchorLink,
    title_index: &HashMap<String, Vec<PathBuf>>,
    post_index: &HashMap<String, Vec<PathBuf>>,
) -> AliasSource {
    let current_relative = file.strip_prefix(root).unwrap_or(file).to_path_buf();

    if let Some(fragment) = reference_fragment(&link.href) {
        for anchor in anchor_variants(fragment) {
            if current_anchors.contains(&anchor) {
                return AliasSource::Resolved(current_relative);
            }
            if let Some(candidates) = post_index.get(&anchor)
                && let Some(candidate) = candidates.first()
            {
                return AliasSource::Resolved(candidate.clone());
            }
        }
    }

    let text = normalize_text(&link.text);
    if current_topic_title.is_some() && text == "print view" {
        return AliasSource::Resolved(current_relative);
    }
    if current_topic_title.is_some_and(|title| title == text) {
        return AliasSource::Resolved(current_relative);
    }

    let Some(candidates) = title_index.get(&text) else {
        if let Some(source) =
            container_alias_source(root, file, input, link, &text, title_index, post_index)
        {
            return AliasSource::Resolved(source);
        }
        return AliasSource::Unresolved;
    };
    if candidates.len() == 1 {
        AliasSource::Resolved(candidates[0].clone())
    } else if let Some(source) =
        container_alias_source(root, file, input, link, &text, title_index, post_index)
    {
        AliasSource::Resolved(source)
    } else {
        AliasSource::Ambiguous
    }
}

fn container_alias_source(
    root: &Path,
    file: &Path,
    input: &str,
    link: &AnchorLink,
    title: &str,
    title_index: &HashMap<String, Vec<PathBuf>>,
    post_index: &HashMap<String, Vec<PathBuf>>,
) -> Option<PathBuf> {
    let container = containing_topic_container(input, link.start, link.end)?;
    for sibling in extract_anchor_links(container) {
        if sibling.href == link.href {
            continue;
        }

        if let Some(source) =
            existing_topic_source_from_href(root, file, &sibling.href, title, title_index)
        {
            return Some(source);
        }

        let Some(fragment) = reference_fragment(&sibling.href) else {
            continue;
        };
        for anchor in anchor_variants(fragment) {
            let Some(candidates) = post_index.get(&anchor) else {
                continue;
            };
            if let Some(candidate) = candidates
                .iter()
                .find(|candidate| topic_candidate_matches_title(candidate, title, title_index))
            {
                return Some(candidate.clone());
            }
        }
    }

    None
}

fn existing_topic_source_from_href(
    root: &Path,
    file: &Path,
    href: &str,
    title: &str,
    title_index: &HashMap<String, Vec<PathBuf>>,
) -> Option<PathBuf> {
    if should_skip_reference(href) {
        return None;
    }

    let target = resolve_local_reference(root, file, href).ok()?;
    if !target.exists() {
        return None;
    }
    if !target
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("viewtopic.php"))
    {
        return None;
    }

    let relative = target.strip_prefix(root).ok()?.to_path_buf();
    topic_candidate_matches_title(&relative, title, title_index).then_some(relative)
}

fn topic_candidate_matches_title(
    candidate: &Path,
    title: &str,
    title_index: &HashMap<String, Vec<PathBuf>>,
) -> bool {
    title_index
        .get(title)
        .is_some_and(|candidates| candidates.iter().any(|item| item == candidate))
}

fn containing_topic_container(input: &str, start: usize, end: usize) -> Option<&str> {
    ["li", "tr"]
        .into_iter()
        .filter_map(|tag| containing_tag(input, tag, start, end))
        .min_by_key(|container| container.len())
}

fn containing_tag<'a>(input: &'a str, tag: &str, start: usize, end: usize) -> Option<&'a str> {
    let lower = input.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let open_start = lower[..start].rfind(&open)?;
    if lower[open_start..start].rfind(&close).is_some() {
        return None;
    }
    let close_end = lower[end..].find(&close)? + end + close.len();
    Some(&input[open_start..close_end])
}

fn resolve_local_reference(root: &Path, from_file: &Path, href: &str) -> Result<PathBuf> {
    let path = href
        .split_once('#')
        .map_or(href, |(path, _)| path)
        .split_once('?')
        .map_or_else(
            || href.split_once('#').map_or(href, |(path, _)| path),
            |(path, _)| path,
        );
    let decoded = percent_decode(path);
    let target = if decoded.starts_with('/') {
        root.join(decoded.trim_start_matches('/'))
    } else {
        from_file.parent().unwrap_or(root).join(decoded)
    };

    Ok(normalize_path(&target))
}

fn should_skip_reference(href: &str) -> bool {
    let lower = href.trim().to_ascii_lowercase();
    lower.is_empty()
        || lower.starts_with('#')
        || lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with("javascript:")
        || lower.starts_with("data:")
        || lower.starts_with("tel:")
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AnchorLink {
    href: String,
    text: String,
    start: usize,
    end: usize,
}

fn extract_anchor_links(input: &str) -> Vec<AnchorLink> {
    let mut links = Vec::new();
    let lower = input.to_ascii_lowercase();
    let mut offset = 0;

    while let Some(relative_start) = lower[offset..].find("<a") {
        let start = offset + relative_start;
        let Some(open_end_relative) = lower[start..].find('>') else {
            break;
        };
        let open_end = start + open_end_relative;
        let open = &input[start..=open_end];
        let Some(href) = extract_attr(open, "href") else {
            offset = open_end + 1;
            continue;
        };

        let Some(close_relative) = lower[open_end + 1..].find("</a>") else {
            break;
        };
        let close = open_end + 1 + close_relative;
        let text = input[open_end + 1..close].to_owned();
        links.push(AnchorLink {
            href,
            text,
            start,
            end: close + 4,
        });
        offset = close + 4;
    }

    links
}

fn extract_anchor_ids(input: &str) -> Vec<String> {
    let mut anchors = Vec::new();
    let lower = input.to_ascii_lowercase();
    let mut offset = 0;

    while let Some(relative_start) = lower[offset..].find('<') {
        let start = offset + relative_start;
        let Some(open_end_relative) = lower[start..].find('>') else {
            break;
        };
        let open_end = start + open_end_relative;
        let open = &input[start..=open_end];
        for attr in ["id", "name"] {
            if let Some(value) = extract_attr(open, attr) {
                let value = normalize_anchor_id(&value);
                if !value.is_empty() {
                    anchors.push(value);
                }
            }
        }
        offset = open_end + 1;
    }

    anchors.sort();
    anchors.dedup();
    anchors
}

fn extract_attr(input: &str, name: &str) -> Option<String> {
    let lower = input.to_ascii_lowercase();
    let mut offset = 0;
    let needle = format!("{name}=");
    while let Some(relative_start) = lower[offset..].find(&needle) {
        let start = offset + relative_start + needle.len();
        let mut chars = input[start..].chars();
        let quote = chars.next()?;
        if quote != '"' && quote != '\'' {
            offset = start;
            continue;
        }
        let value_start = start + quote.len_utf8();
        let value_end = input[value_start..].find(quote)? + value_start;
        return Some(input[value_start..value_end].to_owned());
    }
    None
}

fn extract_tag_text(input: &str, tag: &str) -> Option<String> {
    let lower = input.to_ascii_lowercase();
    let open = format!("<{tag}");
    let start = lower.find(&open)?;
    let open_end = lower[start..].find('>')? + start;
    let close = lower[open_end + 1..].find(&format!("</{tag}>"))? + open_end + 1;
    Some(input[open_end + 1..close].to_owned())
}

fn normalize_text(input: &str) -> String {
    let stripped = strip_tags(input);
    let decoded = decode_entities(&stripped);
    decoded
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn reference_fragment(href: &str) -> Option<&str> {
    let fragment = href.split_once('#')?.1;
    let fragment = fragment
        .split_once('?')
        .map_or(fragment, |(fragment, _)| fragment);
    let fragment = fragment.trim();
    (!fragment.is_empty()).then_some(fragment)
}

fn anchor_variants(fragment: &str) -> Vec<String> {
    let normalized = normalize_anchor_id(fragment);
    if normalized.is_empty() {
        return Vec::new();
    }

    let mut variants = vec![normalized.clone()];
    if let Some(number) = normalized.strip_prefix('p') {
        if number.chars().all(|character| character.is_ascii_digit()) {
            variants.push(number.to_owned());
        }
    } else if normalized
        .chars()
        .all(|character| character.is_ascii_digit())
    {
        variants.push(format!("p{normalized}"));
    }
    variants.sort();
    variants.dedup();
    variants
}

fn normalize_anchor_id(input: &str) -> String {
    decode_entities(&percent_decode(input.trim())).to_ascii_lowercase()
}

fn strip_tags(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_tag = false;
    for character in input.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            character if !in_tag => output.push(character),
            _ => {}
        }
    }
    output
}

fn decode_entities(input: &str) -> String {
    input
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            output.push(high * 16 + low);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn read_lossy(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn is_html_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_missing_topic_alias_from_matching_title() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("forums")).unwrap();
        fs::write(
            root.join("forums/source.html"),
            r#"<a href="viewtopic.php__q_missing.html" class="topictitle">[RAS2] Diary Entry #32</a>"#,
        )
        .unwrap();
        fs::write(
            root.join("forums/viewtopic.php__q_existing.html"),
            r#"<title>Small Rockets Forum &bull; View topic - [RAS2] Diary Entry #32</title><p>topic</p>"#,
        )
        .unwrap();

        let report = create_missing_topic_aliases(root).unwrap();

        assert_eq!(report.created, 1);
        assert!(root.join("forums/viewtopic.php__q_missing.html").exists());
    }

    #[test]
    fn leaves_ambiguous_titles_unresolved() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("forums")).unwrap();
        fs::write(
            root.join("forums/source.html"),
            r#"<a href="viewtopic.php__q_missing.html">Duplicate</a>"#,
        )
        .unwrap();
        for name in ["one", "two"] {
            fs::write(
                root.join(format!("forums/viewtopic.php__q_{name}.html")),
                r#"<title>Small Rockets Forum &bull; View topic - Duplicate</title>"#,
            )
            .unwrap();
        }

        let report = create_missing_topic_aliases(root).unwrap();

        assert_eq!(report.created, 0);
        assert_eq!(report.ambiguous, 1);
    }

    #[test]
    fn creates_self_alias_for_matching_topic_title() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("forums")).unwrap();
        fs::write(
            root.join("forums/viewtopic.php__q_existing.html"),
            r#"<title>SmallRockets Forum :: View topic - Mission 16</title><a href="viewtopic.php__q_missing.html">Mission 16</a>"#,
        )
        .unwrap();

        let report = create_missing_topic_aliases(root).unwrap();

        assert_eq!(report.created, 1);
        assert!(root.join("forums/viewtopic.php__q_missing.html").exists());
    }

    #[test]
    fn creates_self_alias_for_post_fragment() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("forums")).unwrap();
        fs::write(
            root.join("forums/viewtopic.php__q_existing.html"),
            r#"<title>SmallRockets Forum &bull; View topic - Two questions</title><div id="p25315"></div><a href="viewtopic.php__q_missing.html#p25315"></a>"#,
        )
        .unwrap();

        let report = create_missing_topic_aliases(root).unwrap();

        assert_eq!(report.created, 1);
        assert!(root.join("forums/viewtopic.php__q_missing.html").exists());
    }

    #[test]
    fn creates_alias_for_post_fragment_found_in_another_topic_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("forums")).unwrap();
        fs::write(
            root.join("forums/source.html"),
            r#"<a href="viewtopic.php__q_missing.html#21710"></a>"#,
        )
        .unwrap();
        fs::write(
            root.join("forums/viewtopic.php__q_existing.html"),
            r#"<title>SmallRockets Forum :: View topic - Other</title><a name="21710"></a>"#,
        )
        .unwrap();

        let report = create_missing_topic_aliases(root).unwrap();

        assert_eq!(report.created, 1);
        assert!(root.join("forums/viewtopic.php__q_missing.html").exists());
    }

    #[test]
    fn resolves_ambiguous_topic_title_from_forum_row_latest_post_link() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("forums")).unwrap();
        fs::write(
            root.join("forums/viewforum.html"),
            r#"<li><a href="viewtopic.php__q_missing.html" class="topictitle">LAN Gaming HOW-TO: Multiplayer gaming over a local network</a><a href="viewtopic.php__q_latest.html#p28454">latest</a></li>"#,
        )
        .unwrap();
        fs::write(
            root.join("forums/viewtopic.php__q_latest.html"),
            r#"<title>Small Rockets Forum &bull; View topic - LAN Gaming HOW-TO: Multiplayer gaming over a local network</title><div id="p28454"></div><p>latest</p>"#,
        )
        .unwrap();
        fs::write(
            root.join("forums/viewtopic.php__q_other.html"),
            r#"<title>Small Rockets Forum &bull; View topic - LAN Gaming HOW-TO: Multiplayer gaming over a local network</title><p>other</p>"#,
        )
        .unwrap();

        let report = create_missing_topic_aliases(root).unwrap();

        assert_eq!(report.created, 1);
        assert_eq!(
            fs::read_to_string(root.join("forums/viewtopic.php__q_missing.html")).unwrap(),
            fs::read_to_string(root.join("forums/viewtopic.php__q_latest.html")).unwrap()
        );
    }
}
