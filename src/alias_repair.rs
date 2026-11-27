use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use url::Url;

use crate::pathmap::{canonical_query_without_volatile_params, unwrap_wayback_url};
use crate::recovery::CaptureEvent;

/// Topic identities are local to one forum directory, including its host prefix.
#[derive(Clone, Debug, Default)]
struct ForumTopicScope {
    title_index: HashMap<String, Vec<PathBuf>>,
    post_index: HashMap<String, Vec<PathBuf>>,
    topic_id_index: HashMap<String, Vec<PathBuf>>,
    hash_index: HashMap<String, Vec<PathBuf>>,
    file_topic_ids: HashMap<PathBuf, String>,
    file_anchors: HashMap<PathBuf, HashSet<String>>,
    file_reply_links: HashMap<PathBuf, HashSet<String>>,
    file_topic_links: HashMap<PathBuf, HashSet<String>>,
    file_start_offsets: HashMap<PathBuf, u64>,
    forum_ids: HashSet<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AliasRepairReport {
    pub created: usize,
    pub unresolved: usize,
    pub ambiguous: usize,
}

/// Restores missing or retained-unusable index.html files from a usable index.htm
/// in the same directory. Existing usable destinations, symlinks, and sources
/// resolving outside the archive are never touched.
pub fn create_missing_directory_aliases(root: &Path) -> Result<usize> {
	let root = fs::canonicalize(root)?;
	let report = crate::link_validation::validate_local_links(&root)?;
	let mut created = 0;
	let mut seen = HashSet::new();
	let candidates = report
		.missing
		.into_iter()
		.map(|missing| (missing.target, false))
		.chain(
			report
				.unusable_html
				.into_iter()
				.map(|target| (target, true)),
		);
	for (target, replace_unusable) in candidates {
		if target.file_name().is_none_or(|name| name != "index.html")
			|| !seen.insert(target.clone())
		{
			continue;
		}
		if replace_unusable {
			if !is_existing_unusable_regular_file(&target) {
				continue;
			}
		} else if fs::symlink_metadata(&target).is_ok() {
			continue;
		}
		let source = target.with_file_name("index.htm");
		let Ok(canonical) = fs::canonicalize(&source) else { continue; };
		if !canonical.starts_with(&root) || !canonical.is_file() { continue; }
		let bytes = fs::read(&canonical)?;
		if bytes.is_empty() || crate::soft_redirect::is_unusable_html_capture(&String::from_utf8_lossy(&bytes)) {
			continue;
		}
		write_alias_file(&target, &canonical, &bytes, replace_unusable)?;
		created += 1;
	}
	Ok(created)
}

/// Creates missing topic copies (or replaces retained-unusable topic error placeholders)
/// only beside their usable source, preserving relative URLs.
/// Matching requires structural topic/post identity within the destination forum
/// (numeric post anchors, topic/post query hashes, self print-view links, or same-row
/// post-anchor corroboration); bare titles never establish topic identity.
/// Existing usable files and symlinks are never replaced, and no cross-forum equivalence is inferred.
pub fn create_missing_topic_aliases(root: &Path) -> Result<AliasRepairReport> {
	let root = fs::canonicalize(root).context("failed to open topic alias directory")?;
	let root = root.as_path();
    let html_files = collect_html_files(root)?;
    let journal = load_journal_topic_metadata(root);
    let scopes = build_forum_topic_scopes(root, &html_files, &journal)?;
    let mut report = AliasRepairReport::default();
	let empty_scope = ForumTopicScope::default();
	let empty_anchors = HashSet::new();
    let mut written_targets = HashSet::new();

    for file in &html_files {
        let input = read_lossy(file)?;
        let file_is_usable = !crate::soft_redirect::is_unusable_html_capture(&input);
        let current_topic_title = file_is_usable
            .then(|| extract_topic_title(&input).map(|title| normalize_text(&title)))
            .flatten();
        let current_anchors = if file_is_usable {
            extract_anchor_ids(&input)
                .into_iter()
                .collect::<HashSet<_>>()
        } else {
            HashSet::new()
        };
        for link in extract_anchor_links(&input) {
            let Some((target, replace_unusable)) =
                alias_topic_target(root, file, &link.href, &written_targets)?
            else {
                continue;
            };
			let relative_target = target.strip_prefix(root)?;
			let scope_path = relative_target.parent().unwrap_or(Path::new(""));
			let same_forum = file.parent() == target.parent();
            let scope = scopes.get(scope_path).unwrap_or(&empty_scope);
            let source = match alias_source_for_link(
                root,
                file,
                &target,
                &input,
                relative_target,
				if same_forum { &current_anchors } else { &empty_anchors },
				if same_forum { current_topic_title.as_deref() } else { None },
                &link,
				scope,
                &journal,
            ) {
                AliasSource::Resolved(source) => source,
                AliasSource::Unresolved => {
                    if !replace_unusable {
                        report.unresolved += 1;
                    }
                    continue;
                }
                AliasSource::Ambiguous => {
                    if !replace_unusable {
                        report.ambiguous += 1;
                    }
                    continue;
                }
            };

			anyhow::ensure!(source.parent() == Some(scope_path), "topic alias source must share its destination directory");
            let source = root.join(source);
            if source == target {
                continue;
            }
			let bytes = fs::read(&source)
				.with_context(|| format!("failed to read topic source {}", source.display()))?;
            write_alias_file(&target, &source, &bytes, replace_unusable)?;
            written_targets.insert(target);
            report.created += 1;
        }
    }

    // Also replace any remaining retained-unusable viewtopic.php__q_<hash>.html files whose
    // query hash matches a usable topic or post already present in the same forum scope.
    for file in &html_files {
        if written_targets.contains(file) || !is_existing_unusable_regular_file(file) {
            continue;
        }
        let Ok(relative) = file.strip_prefix(root) else {
            continue;
        };
        let Some(hash) = extract_target_query_hash(relative) else {
            continue;
        };
        if journal.adjacent_nav_hashes.contains(hash) {
            continue;
        }
        let scope_path = relative.parent().unwrap_or(Path::new(""));
        let Some(scope) = scopes.get(scope_path) else {
            continue;
        };
        let Some(candidates) = scope.hash_index.get(hash) else {
            continue;
        };
        let Some(source_relative) = resolve_same_topic_candidates(candidates, scope) else {
            continue;
        };
        if source_relative == relative
            || source_relative.parent() != Some(scope_path)
            || candidate_conflicts_with_target(
                &source_relative,
                journal.queries_by_hash.get(hash),
                scope,
            )
        {
            continue;
        }
        let source = root.join(source_relative);
        let bytes = fs::read(&source)
            .with_context(|| format!("failed to read topic source {}", source.display()))?;
        write_alias_file(file, &source, &bytes, true)?;
        written_targets.insert(file.clone());
        report.created += 1;
    }

    Ok(report)
}

fn is_existing_unusable_regular_file(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return false;
    }
    let Ok(input) = read_lossy(path) else {
        return false;
    };
    crate::soft_redirect::is_unusable_html_capture(&input)
}

fn write_alias_file(
    target: &Path,
    source: &Path,
    bytes: &[u8],
    replace_unusable: bool,
) -> Result<()> {
    let permissions = fs::metadata(source)?.permissions();
    if !replace_unusable {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)
            .with_context(|| format!("failed to create alias {}", target.display()))?;
        file.set_permissions(permissions)?;
        std::io::Write::write_all(&mut file, bytes)
            .with_context(|| format!("failed to write alias {}", target.display()))?;
        return Ok(());
    }

    let metadata = fs::symlink_metadata(target)
        .with_context(|| format!("failed to inspect existing alias target {}", target.display()))?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink() && metadata.file_type().is_file(),
        "refusing to replace non-regular alias target {}",
        target.display()
    );
    let file_name = target
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "alias".into());
    let temp_path = target.with_file_name(format!(".{file_name}.webarchive-alias.tmp"));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp_path)
            .with_context(|| format!("failed to create temporary alias {}", temp_path.display()))?;
        file.set_permissions(permissions)?;
        std::io::Write::write_all(&mut file, bytes)
            .with_context(|| format!("failed to write temporary alias {}", temp_path.display()))?;
        std::io::Write::flush(&mut file)
            .with_context(|| format!("failed to flush temporary alias {}", temp_path.display()))?;
        drop(file);
        fs::rename(&temp_path, target).with_context(|| {
            format!(
                "failed to replace unusable capture {} with alias",
                target.display()
            )
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
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

#[derive(Clone, Debug, Default)]
struct JournalTopicMetadata {
    originals_by_path: HashMap<PathBuf, String>,
    queries_by_hash: HashMap<String, (Option<String>, Option<String>)>,
    adjacent_nav_hashes: HashSet<String>,
    forum_ids: HashSet<String>,
}

fn load_journal_topic_metadata(root: &Path) -> JournalTopicMetadata {
    let journal_path = root.join(".wayback-state/captures.jsonl");
    let Ok(bytes) = fs::read(&journal_path) else {
        return JournalTopicMetadata::default();
    };
    let mut metadata = JournalTopicMetadata::default();
    for line in bytes.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
        let Ok(event) = serde_json::from_slice::<CaptureEvent>(line) else {
            continue;
        };
        if event.outcome == "downloaded" {
            metadata
                .originals_by_path
                .insert(event.local_path.clone(), event.record.original.clone());
        }
        for url_str in std::iter::once(&event.record.original).chain(event.references.iter()) {
            let Ok(url) = Url::parse(url_str) else {
                continue;
            };
            let path = url.path().to_ascii_lowercase();
            if let Some(forum_id) = extract_numeric_query_param(&url, &["f", "forum"]) {
                metadata.forum_ids.insert(forum_id);
            }
            if !path.ends_with("/viewtopic.php") {
                continue;
            }
            let Some(canonical_query) = canonical_query_without_volatile_params(&url) else {
                continue;
            };
            let hash = query_hash_hex(&canonical_query);
            if is_adjacent_topic_navigation_url(&url) {
                metadata.adjacent_nav_hashes.insert(hash);
                continue;
            }
            let topic_id = extract_numeric_query_param(&url, &["t", "topic"]);
            let post_id = extract_numeric_query_param(&url, &["p", "post"]);
            if topic_id.is_some() || post_id.is_some() {
                metadata.queries_by_hash.insert(hash, (topic_id, post_id));
            }
        }
    }
    metadata
}

fn build_forum_topic_scopes(
    root: &Path,
    html_files: &[PathBuf],
    journal: &JournalTopicMetadata,
) -> Result<HashMap<PathBuf, ForumTopicScope>> {
    let mut scopes: HashMap<PathBuf, ForumTopicScope> = HashMap::new();

    for file in html_files {
        if !file
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("viewtopic.php"))
        {
            continue;
        }

        let input = read_lossy(file)?;
        if crate::soft_redirect::is_unusable_html_capture(&input) {
            continue;
        }
        let relative = file
            .strip_prefix(root)
            .unwrap_or(file.as_path())
            .to_path_buf();
        let scope_path = relative.parent().unwrap_or(Path::new("")).to_path_buf();
        let scope = scopes.entry(scope_path).or_default();

        if let Some(title) = extract_topic_title(&input) {
            let title = normalize_text(&title);
            if !title.is_empty() {
                scope
                    .title_index
                    .entry(title)
                    .or_default()
                    .push(relative.clone());
            }
        }

        for anchor in extract_anchor_ids(&input) {
            if anchor_variants(&anchor).is_empty() {
                continue;
            }
            scope
                .post_index
                .entry(anchor.clone())
                .or_default()
                .push(relative.clone());
            scope
                .file_anchors
                .entry(relative.clone())
                .or_default()
                .insert(anchor);
        }

        let mut topic_id = None;
        if let Some(original) = journal.originals_by_path.get(&relative)
            && let Ok(url) = Url::parse(original)
            && !is_adjacent_topic_navigation_url(&url)
        {
            topic_id = extract_numeric_query_param(&url, &["t", "topic"]);
            if let Some(forum_id) = extract_numeric_query_param(&url, &["f", "forum"]) {
                scope.forum_ids.insert(forum_id);
            }
            if let Some(start) = extract_numeric_query_param(&url, &["start"])
                .and_then(|value| value.parse::<u64>().ok())
            {
                scope.file_start_offsets.insert(relative.clone(), start);
            } else if topic_id.is_some()
                && extract_numeric_query_param(&url, &["p", "post"]).is_none()
            {
                scope.file_start_offsets.insert(relative.clone(), 0);
            }
        }

        let (html_topic_id, html_forum_ids) = extract_self_topic_and_forum_ids(&input);
        if topic_id.is_none() {
            topic_id = html_topic_id;
        }
        scope.forum_ids.extend(html_forum_ids);

        if let Some(topic_id) = topic_id {
            scope
                .file_topic_ids
                .insert(relative.clone(), topic_id.clone());
            scope
                .topic_id_index
                .entry(topic_id)
                .or_default()
                .push(relative.clone());
        }

        for link in extract_anchor_links(&input) {
            let href_path = link
                .href
                .split_once('#')
                .map_or(link.href.as_str(), |(path, _)| path)
                .split_once('?')
                .map_or_else(
                    || {
                        link.href
                            .split_once('#')
                            .map_or(link.href.as_str(), |(path, _)| path)
                    },
                    |(path, _)| path,
                );
            let Some(file_name) = Path::new(href_path).file_name().and_then(|name| name.to_str())
            else {
                continue;
            };
            if file_name.starts_with("posting.php__q_") {
                scope
                    .file_reply_links
                    .entry(relative.clone())
                    .or_default()
                    .insert(file_name.to_owned());
            } else if file_name.starts_with("viewtopic.php__q_") {
                scope
                    .file_topic_links
                    .entry(relative.clone())
                    .or_default()
                    .insert(file_name.to_owned());
            }
        }
    }

    for scope in scopes.values_mut() {
        scope.forum_ids.extend(journal.forum_ids.iter().cloned());
        for candidates in scope.title_index.values_mut() {
            candidates.sort();
            candidates.dedup();
        }
        for candidates in scope.post_index.values_mut() {
            candidates.sort();
            candidates.dedup();
        }
        for candidates in scope.topic_id_index.values_mut() {
            candidates.sort();
            candidates.dedup();
        }

        for (hash, (topic_id, post_id)) in &journal.queries_by_hash {
            if let Some(post_id) = post_id
                && let Some(files) = scope
                    .post_index
                    .get(&format!("p{post_id}"))
                    .or_else(|| scope.post_index.get(post_id))
            {
                scope
                    .hash_index
                    .entry(hash.clone())
                    .or_default()
                    .extend(files.iter().cloned());
            } else if let Some(topic_id) = topic_id
                && let Some(files) = scope.topic_id_index.get(topic_id)
            {
                scope
                    .hash_index
                    .entry(hash.clone())
                    .or_default()
                    .extend(files.iter().cloned());
            }
        }

        let forum_ids: Vec<String> = scope.forum_ids.iter().cloned().collect();
        for (anchor, files) in &scope.post_index {
            let number = anchor.strip_prefix('p').unwrap_or(anchor);
            if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            for query in [
                format!("p={number}"),
                format!("p={number}&start=0"),
            ] {
                scope
                    .hash_index
                    .entry(query_hash_hex(&query))
                    .or_default()
                    .extend(files.iter().cloned());
            }
            for forum_id in &forum_ids {
                let query = format!("f={forum_id}&p={number}");
                scope
                    .hash_index
                    .entry(query_hash_hex(&query))
                    .or_default()
                    .extend(files.iter().cloned());
            }
        }

        for (topic_id, files) in &scope.topic_id_index {
            for query in [
                format!("t={topic_id}"),
                format!("start=0&t={topic_id}"),
                format!("t={topic_id}&view=print"),
            ] {
                scope
                    .hash_index
                    .entry(query_hash_hex(&query))
                    .or_default()
                    .extend(files.iter().cloned());
            }
            for forum_id in &forum_ids {
                for query in [
                    format!("f={forum_id}&t={topic_id}"),
                    format!("f={forum_id}&start=0&t={topic_id}"),
                    format!("f={forum_id}&t={topic_id}&view=print"),
                ] {
                    scope
                        .hash_index
                        .entry(query_hash_hex(&query))
                        .or_default()
                        .extend(files.iter().cloned());
                }
            }
        }

        for candidates in scope.hash_index.values_mut() {
            candidates.sort();
            candidates.dedup();
        }
    }

    Ok(scopes)
}

fn is_adjacent_topic_navigation_url(url: &Url) -> bool {
    url.query_pairs().any(|(key, value)| {
        key.trim_start_matches("amp;").eq_ignore_ascii_case("view")
            && matches!(value.to_ascii_lowercase().as_str(), "next" | "previous")
    })
}

fn extract_numeric_query_param(url: &Url, names: &[&str]) -> Option<String> {
    for (key, value) in url.query_pairs() {
        let key = key.trim_start_matches("amp;");
        if names.iter().any(|name| key.eq_ignore_ascii_case(name)) {
            let trimmed = value.trim();
            if !trimmed.is_empty() && trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
                return Some(trimmed.to_owned());
            }
        }
    }
    None
}

fn extract_self_topic_and_forum_ids(input: &str) -> (Option<String>, HashSet<String>) {
    let mut topic_ids = HashSet::new();
    let mut forum_ids = HashSet::new();
    let Ok(base) = Url::parse("http://local.invalid/forums/") else {
        return (None, forum_ids);
    };

    for link in extract_anchor_links(input) {
        let decoded = decode_entities(&link.href);
        let Ok(url) = base.join(&unwrap_wayback_url(decoded.trim())) else {
            continue;
        };
        let path = url.path().to_ascii_lowercase();
        if path.ends_with("/viewforum.php")
            && let Some(f) = extract_numeric_query_param(&url, &["f", "forum"])
        {
            forum_ids.insert(f);
        }
        let is_self_topic_link = (path.ends_with("/posting.php")
            && url.query_pairs().any(|(k, v)| {
                k.trim_start_matches("amp;").eq_ignore_ascii_case("mode")
                    && matches!(v.to_ascii_lowercase().as_str(), "reply" | "quote" | "bump")
            }))
            || (path.ends_with("/viewtopic.php")
                && url.query_pairs().any(|(k, v)| {
                    let k = k.trim_start_matches("amp;");
                    (k.eq_ignore_ascii_case("watch") && v.eq_ignore_ascii_case("topic"))
                        || (k.eq_ignore_ascii_case("unwatch") && v.eq_ignore_ascii_case("topic"))
                        || (k.eq_ignore_ascii_case("view")
                            && matches!(
                                v.to_ascii_lowercase().as_str(),
                                "next" | "previous" | "print" | "unread"
                            ))
                        || k.eq_ignore_ascii_case("bookmark")
                }));
        if is_self_topic_link {
            if let Some(t) = extract_numeric_query_param(&url, &["t", "topic"]) {
                topic_ids.insert(t);
            }
            if let Some(f) = extract_numeric_query_param(&url, &["f", "forum"]) {
                forum_ids.insert(f);
            }
        }
    }

    let topic_id = (topic_ids.len() == 1)
        .then(|| topic_ids.into_iter().next())
        .flatten();
    (topic_id, forum_ids)
}

fn query_hash_hex(canonical_query: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in canonical_query.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn extract_target_query_hash(relative_target: &Path) -> Option<&str> {
    let file_name = relative_target.file_name()?.to_str()?;
    let rest = file_name.strip_prefix("viewtopic.php__q_")?;
    let hash = rest.strip_suffix(".html")?;
    (!hash.is_empty()).then_some(hash)
}

fn extract_topic_title(input: &str) -> Option<String> {
    let title = extract_tag_text(input, "title")?;
    let lower = title.to_ascii_lowercase();
    let marker = "view topic -";
    let start = lower.find(marker)? + marker.len();
    Some(title[start..].trim().to_owned())
}

fn alias_topic_target(
    root: &Path,
    from_file: &Path,
    href: &str,
    written_targets: &HashSet<PathBuf>,
) -> Result<Option<(PathBuf, bool)>> {
    if should_skip_reference(href) {
        return Ok(None);
    }

    let target = resolve_local_reference(root, from_file, href)?;
    if !target.starts_with(&normalize_path(root)) {
        return Ok(None);
    }
    if !target
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("viewtopic.php__q_") && name.ends_with(".html"))
    {
        return Ok(None);
    }
    if written_targets.contains(&target) {
        return Ok(None);
    }

    match fs::symlink_metadata(&target) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Some((target, false))),
        Ok(_) if is_existing_unusable_regular_file(&target) => Ok(Some((target, true))),
        _ => Ok(None),
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
    target: &Path,
    input: &str,
    relative_target: &Path,
    current_anchors: &HashSet<String>,
    current_topic_title: Option<&str>,
    link: &AnchorLink,
    scope: &ForumTopicScope,
    journal: &JournalTopicMetadata,
) -> AliasSource {
    let current_relative = file.strip_prefix(root).unwrap_or(file).to_path_buf();
    let target_hash = extract_target_query_hash(relative_target);
    if target_hash.is_some_and(|hash| journal.adjacent_nav_hashes.contains(hash)) {
        return AliasSource::Unresolved;
    }
    let target_identity = target_hash.and_then(|hash| journal.queries_by_hash.get(hash));

    if let Some(fragment) = reference_fragment(&link.href) {
        for anchor in anchor_variants(fragment) {
            if current_anchors.contains(&anchor)
                && !candidate_conflicts_with_target(&current_relative, target_identity, scope)
            {
                return AliasSource::Resolved(current_relative);
            }
            if let Some(candidates) = scope.post_index.get(&anchor) {
                return match resolve_same_topic_candidates(candidates, scope) {
                    Some(candidate)
                        if !candidate_conflicts_with_target(&candidate, target_identity, scope) =>
                    {
                        AliasSource::Resolved(candidate)
                    }
                    Some(_) => AliasSource::Unresolved,
                    None => AliasSource::Ambiguous,
                };
            }
        }
    }

    if let Some(hash) = target_hash
        && let Some(candidates) = scope.hash_index.get(hash)
    {
        return match resolve_same_topic_candidates(candidates, scope) {
            Some(resolved)
                if !candidate_conflicts_with_target(&resolved, target_identity, scope) =>
            {
                AliasSource::Resolved(resolved)
            }
            Some(_) => AliasSource::Unresolved,
            None => AliasSource::Ambiguous,
        };
    }

    let text = normalize_text(&link.text);
    if current_topic_title.is_some()
        && text == "print view"
        && !candidate_conflicts_with_target(&current_relative, target_identity, scope)
    {
        return AliasSource::Resolved(current_relative);
    }

    if !text.is_empty()
        && let Some(source) = container_alias_source(
            root,
            file,
            target,
            input,
            link,
            &text,
            target_identity,
            scope,
        )
    {
        return AliasSource::Resolved(source);
    }

    AliasSource::Unresolved
}

fn candidate_conflicts_with_target(
    candidate: &Path,
    target_identity: Option<&(Option<String>, Option<String>)>,
    scope: &ForumTopicScope,
) -> bool {
    let Some((target_topic_id, target_post_id)) = target_identity else {
        return false;
    };
    if let Some(target_topic_id) = target_topic_id
        && let Some(candidate_topic_id) = scope.file_topic_ids.get(candidate)
        && candidate_topic_id != target_topic_id
    {
        return true;
    }
    if let Some(target_post_id) = target_post_id {
        let has_post = scope.file_anchors.get(candidate).is_some_and(|anchors| {
            anchors.contains(target_post_id)
                || anchors.contains(&format!("p{target_post_id}"))
        });
        if !has_post {
            return true;
        }
    }
    false
}

fn resolve_same_topic_candidates(
    candidates: &[PathBuf],
    scope: &ForumTopicScope,
) -> Option<PathBuf> {
    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        return Some(candidates[0].clone());
    }

    let known_topic_ids: HashSet<&str> = candidates
        .iter()
        .filter_map(|candidate| scope.file_topic_ids.get(candidate).map(String::as_str))
        .collect();
    if known_topic_ids.len() > 1 {
        return None;
    }

    let all_share_known_topic_id = known_topic_ids.len() == 1
        && candidates
            .iter()
            .all(|candidate| scope.file_topic_ids.contains_key(candidate));

    if !all_share_known_topic_id && !candidates_are_same_topic_connected(candidates, scope) {
        return None;
    }

    candidates
        .iter()
        .min_by(|left, right| {
            let left_start = scope
                .file_start_offsets
                .get(*left)
                .copied()
                .unwrap_or(u64::MAX);
            let right_start = scope
                .file_start_offsets
                .get(*right)
                .copied()
                .unwrap_or(u64::MAX);
            left_start
                .cmp(&right_start)
                .then_with(|| {
                    min_post_number(*left, scope).cmp(&min_post_number(*right, scope))
                })
                .then_with(|| left.cmp(right))
        })
        .cloned()
}

fn candidates_are_same_topic_connected(
    candidates: &[PathBuf],
    scope: &ForumTopicScope,
) -> bool {
    let mut visited = vec![false; candidates.len()];
    let mut queue = std::collections::VecDeque::new();
    visited[0] = true;
    queue.push_back(0usize);

    while let Some(current) = queue.pop_front() {
        for (other, seen) in visited.iter_mut().enumerate() {
            if *seen {
                continue;
            }
            if candidates_share_topic_evidence(&candidates[current], &candidates[other], scope) {
                *seen = true;
                queue.push_back(other);
            }
        }
    }

    visited.into_iter().all(|seen| seen)
}

fn candidates_share_topic_evidence(
    left: &Path,
    right: &Path,
    scope: &ForumTopicScope,
) -> bool {
    if let (Some(left_id), Some(right_id)) = (
        scope.file_topic_ids.get(left),
        scope.file_topic_ids.get(right),
    ) && left_id == right_id
    {
        return true;
    }

    if let (Some(left_anchors), Some(right_anchors)) = (
        scope.file_anchors.get(left),
        scope.file_anchors.get(right),
    ) && left_anchors
        .iter()
        .any(|anchor| right_anchors.contains(anchor))
    {
        return true;
    }

    if let (Some(left_replies), Some(right_replies)) = (
        scope.file_reply_links.get(left),
        scope.file_reply_links.get(right),
    ) && left_replies
        .iter()
        .any(|reply| right_replies.contains(reply))
    {
        return true;
    }

    let left_name = left.file_name().and_then(|name| name.to_str());
    let right_name = right.file_name().and_then(|name| name.to_str());
    if let Some(right_name) = right_name
        && scope
            .file_topic_links
            .get(left)
            .is_some_and(|links| links.contains(right_name))
    {
        return true;
    }
    if let Some(left_name) = left_name
        && scope
            .file_topic_links
            .get(right)
            .is_some_and(|links| links.contains(left_name))
    {
        return true;
    }

    false
}

fn min_post_number(candidate: &Path, scope: &ForumTopicScope) -> u64 {
    scope
        .file_anchors
        .get(candidate)
        .into_iter()
        .flatten()
        .filter_map(|anchor| {
            anchor
                .strip_prefix('p')
                .unwrap_or(anchor)
                .parse::<u64>()
                .ok()
        })
        .min()
        .unwrap_or(u64::MAX)
}

fn container_alias_source(
    root: &Path,
    file: &Path,
    target: &Path,
    input: &str,
    link: &AnchorLink,
    title: &str,
    target_identity: Option<&(Option<String>, Option<String>)>,
    scope: &ForumTopicScope,
) -> Option<PathBuf> {
    let container = containing_topic_container(input, link.start, link.end)?;
    let siblings = extract_anchor_links(container);

    // Reject containers listing more than one distinct topic link with the same title.
    let same_title_hrefs: HashSet<&str> = siblings
        .iter()
        .filter(|sibling| normalize_text(&sibling.text) == title)
        .map(|sibling| sibling.href.as_str())
        .collect();
    if same_title_hrefs.len() > 1 {
        return None;
    }

    let mut matched_sources = HashSet::new();
    for sibling in siblings {
        if sibling.href == link.href || normalize_text(&sibling.text) == title {
            continue;
        }
        if should_skip_reference(&sibling.href) {
            continue;
        }
        let Ok(sibling_target) = resolve_local_reference(root, file, &sibling.href) else {
            continue;
        };
        if sibling_target.parent() != target.parent() {
            continue;
        }
        if !sibling_target
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("viewtopic.php"))
        {
            continue;
        }

        let Some(fragment) = reference_fragment(&sibling.href) else {
            continue;
        };
        for anchor in anchor_variants(fragment) {
            let Some(candidates) = scope.post_index.get(&anchor) else {
                continue;
            };
            let Some(candidate) = resolve_same_topic_candidates(candidates, scope) else {
                continue;
            };
            if !topic_candidate_matches_title(&candidate, title, &scope.title_index) {
                continue;
            }
            let canonical = scope
                .file_topic_ids
                .get(&candidate)
                .and_then(|topic_id| scope.topic_id_index.get(topic_id))
                .and_then(|topic_files| resolve_same_topic_candidates(topic_files, scope))
                .unwrap_or(candidate);
            if !candidate_conflicts_with_target(&canonical, target_identity, scope) {
                matched_sources.insert(canonical);
            }
        }
    }

    (matched_sources.len() == 1)
        .then(|| matched_sources.into_iter().next())
        .flatten()
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

/// Recognizes phpBB's numeric post anchors, excluding shared template IDs such as `top`.
fn anchor_variants(fragment: &str) -> Vec<String> {
    let normalized = normalize_anchor_id(fragment);
	let number = normalized.strip_prefix('p').unwrap_or(&normalized);
	if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Vec::new();
    }

	vec![number.to_owned(), format!("p{number}")]
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

	/// Alias repair keeps existing files byte-identical and rejects error-page sources.
	#[test]
	fn directory_aliases_only_use_existing_usable_sibling() {
		let root = tempfile::tempdir().unwrap();
		fs::write(root.path().join("source.html"), "<a href='press/index.html'>Press</a><a href='bad/index.html'>Bad</a>").unwrap();
		fs::create_dir(root.path().join("press")).unwrap();
		fs::create_dir(root.path().join("bad")).unwrap();
		fs::write(root.path().join("press/index.htm"), b"<p>Press archive \xe9</p>").unwrap();
		fs::write(root.path().join("bad/index.htm"), "<table><tr><td align='center'><span class='gen'>The forum you selected does not exist.</span></td></tr></table>").unwrap();
		assert_eq!(create_missing_directory_aliases(root.path()).unwrap(), 1);
		assert_eq!(fs::read(root.path().join("press/index.html")).unwrap(), b"<p>Press archive \xe9</p>");
		assert!(!root.path().join("bad/index.html").exists());
		assert_eq!(create_missing_directory_aliases(root.path()).unwrap(), 0);
	}

	/// Symlinked source directories outside the archive must not be copied or written.
	#[cfg(unix)]
	#[test]
	fn directory_aliases_do_not_follow_outside_symlinks() {
		let root = tempfile::tempdir().unwrap();
		let outside = tempfile::tempdir().unwrap();
		fs::write(outside.path().join("index.htm"), "<p>Outside</p>").unwrap();
		std::os::unix::fs::symlink(outside.path(), root.path().join("press")).unwrap();
		fs::write(root.path().join("source.html"), "<a href='press/index.html'>Press</a>").unwrap();
		assert_eq!(create_missing_directory_aliases(root.path()).unwrap(), 0);
		assert!(!outside.path().join("index.html").exists());
	}

    #[test]
    fn does_not_create_topic_alias_from_title_alone() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("forums")).unwrap();
        fs::write(
            root.join("forums/source.html"),
            r#"<a href="viewtopic.php__q_missing.html" class="topictitle">Welcome</a>"#,
        )
        .unwrap();
        fs::write(
            root.join("forums/viewtopic.php__q_existing.html"),
            r#"<title>Forum &bull; View topic - Welcome</title><p>Topic 1 text</p>"#,
        )
        .unwrap();

        let report = create_missing_topic_aliases(root).unwrap();

        assert_eq!(report.created, 0);
        assert_eq!(report.unresolved, 1);
        assert!(!root.join("forums/viewtopic.php__q_missing.html").exists());
    }

    #[test]
    fn leaves_ambiguous_post_anchors_unresolved() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("forums")).unwrap();
        fs::write(
            root.join("forums/source.html"),
            r#"<a href="viewtopic.php__q_missing.html#p42">Post</a>"#,
        )
        .unwrap();
        fs::write(
            root.join("forums/viewtopic.php__q_one.html"),
            r#"<title>Forum :: View topic - First</title><div id="p42"></div><a href="posting.php?mode=reply&amp;t=1">Reply</a>"#,
        )
        .unwrap();
        fs::write(
            root.join("forums/viewtopic.php__q_two.html"),
            r#"<title>Forum :: View topic - Second</title><div id="p42"></div><a href="posting.php?mode=reply&amp;t=2">Reply</a>"#,
        )
        .unwrap();

        let report = create_missing_topic_aliases(root).unwrap();

        assert_eq!(report.created, 0);
        assert_eq!(report.ambiguous, 1);
        assert!(!root.join("forums/viewtopic.php__q_missing.html").exists());
    }

    #[test]
    fn does_not_create_self_alias_from_title_alone_without_topic_identity() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("forums")).unwrap();
        fs::write(
            root.join("forums/viewtopic.php__q_existing.html"),
            r#"<title>Forum :: View topic - Mission 16</title><a href="viewtopic.php__q_missing.html">Mission 16</a>"#,
        )
        .unwrap();

        let report = create_missing_topic_aliases(root).unwrap();

        assert_eq!(report.created, 0);
        assert!(!root.join("forums/viewtopic.php__q_missing.html").exists());
    }

    #[test]
    fn does_not_alias_two_same_title_topics_in_same_container() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("forums")).unwrap();
        fs::write(
            root.join("forums/index.html"),
            r#"<li><a href="viewtopic.php__q_one.html">Welcome</a><a href="viewtopic.php__q_two.html">Welcome</a></li>"#,
        )
        .unwrap();
        fs::write(
            root.join("forums/viewtopic.php__q_one.html"),
            r#"<title>Forum :: View topic - Welcome</title><p>Topic 1</p>"#,
        )
        .unwrap();

        let report = create_missing_topic_aliases(root).unwrap();

        assert_eq!(report.created, 0);
        assert!(!root.join("forums/viewtopic.php__q_two.html").exists());
    }

    #[test]
    fn journal_topic_id_contradiction_blocks_alias() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("forums")).unwrap();
        fs::create_dir_all(root.join(".wayback-state")).unwrap();

        let t1_hash = query_hash_hex("t=1");
        let t2_hash = query_hash_hex("t=2");
        let t1_file = format!("viewtopic.php__q_{t1_hash}.html");
        let t2_file = format!("viewtopic.php__q_{t2_hash}.html");

        fs::write(
            root.join("forums").join(&t1_file),
            format!(
                r#"<title>Forum :: View topic - Welcome</title><div id="p10"></div><a href="posting.php?mode=reply&amp;t=1">Reply</a><a href="{t2_file}">Print view</a>"#
            ),
        )
        .unwrap();

        let event = CaptureEvent {
            record: crate::cdx::CdxRecord {
                timestamp: "20061125000000".to_owned(),
                original: "http://www.example.com/forums/viewtopic.php?t=1".to_owned(),
                mimetype: "text/html".to_owned(),
                status_code: 200,
                digest: "T1".to_owned(),
                length: Some(300),
            },
            local_path: PathBuf::from(format!("forums/{t1_file}")),
            outcome: "downloaded".to_owned(),
            references: vec!["http://www.example.com/forums/viewtopic.php?t=2".to_owned()],
            error: None,
        };
        fs::write(
            root.join(".wayback-state/captures.jsonl"),
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();

        let report = create_missing_topic_aliases(root).unwrap();

        assert_eq!(report.created, 0);
        assert!(!root.join("forums").join(&t2_file).exists());
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

    /// When one topic is downloaded under multiple query variants (e.g. page 1 and page 2
    /// of the same topic ID), structural matching (query hash or same-row latest-post anchor)
    /// must pick the first page rather than failing as ambiguous or aliasing to page 2.
    #[test]
    fn resolves_multi_capture_same_topic_candidates_to_first_page() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("forums")).unwrap();
        let topic_hash = query_hash_hex("t=10150");
        let missing_by_hash = format!("viewtopic.php__q_{topic_hash}.html");
        fs::write(
            root.join("forums/viewforum.html"),
            format!(
                r#"<ul><li><a href="viewtopic.php__q_row_missing.html" class="topictitle">Idioma Castellano</a><a href="viewtopic.php__q_page2.html#p24350">latest</a></li><li><a href="{missing_by_hash}">Direct link</a></li></ul>"#
            ),
        )
        .unwrap();
        fs::write(
            root.join("forums/viewtopic.php__q_page2.html"),
            r#"<title>Forum &bull; View topic - Idioma Castellano</title><div id="p24350"></div><a href="posting.php?mode=reply&amp;t=10150">Reply</a>"#,
        )
        .unwrap();
        fs::write(
            root.join("forums/viewtopic.php__q_page1.html"),
            r#"<title>Forum &bull; View topic - Idioma Castellano</title><div id="p24306"></div><a href="posting.php?mode=reply&amp;t=10150">Reply</a>"#,
        )
        .unwrap();

        let report = create_missing_topic_aliases(root).unwrap();

        assert_eq!(report.created, 2);
        assert_eq!(report.ambiguous, 0);
        let page1 = fs::read_to_string(root.join("forums/viewtopic.php__q_page1.html")).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("forums/viewtopic.php__q_row_missing.html")).unwrap(),
            page1
        );
        assert_eq!(
            fs::read_to_string(root.join("forums").join(&missing_by_hash)).unwrap(),
            page1
        );
    }

    /// Links to `viewtopic.php?t=<id>` or `viewtopic.php?p=<id>` (rewritten to `__q_<hash>`)
    /// must resolve directly via their topic/post query hash even when the anchor text is not
    /// the topic title and no `#p...` fragment is present.
    #[test]
    fn resolves_missing_topic_alias_by_topic_or_post_query_hash() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("forums")).unwrap();
        let topic_hash = query_hash_hex("t=10150");
        let post_hash = query_hash_hex("p=24306");
        let missing_topic_file = format!("viewtopic.php__q_{topic_hash}.html");
        let missing_post_file = format!("viewtopic.php__q_{post_hash}.html");

        fs::write(
            root.join("forums/source.html"),
            format!(
                r#"<a href="{missing_topic_file}">see this thread</a><a href="{missing_post_file}">latest post</a>"#
            ),
        )
        .unwrap();
        fs::write(
            root.join("forums/viewtopic.php__q_existing.html"),
            r#"<title>Small Rockets Forum &bull; View topic - Idioma Castellano</title><div id="p24306"></div><a href="posting.php?mode=reply&amp;f=29&amp;t=10150">Reply</a>"#,
        )
        .unwrap();

        let report = create_missing_topic_aliases(root).unwrap();

        assert_eq!(report.created, 2);
        assert!(root.join("forums").join(&missing_topic_file).exists());
        assert!(root.join("forums").join(&missing_post_file).exists());
    }

    /// A retained unusable error placeholder for `viewtopic.php?p=<id>` (e.g.
    /// `"The topic or post you requested does not exist"`) must be replaced with the
    /// usable topic file in the same forum containing post `<id>`, both when linked
    /// and when matched directly by its query hash.
    #[test]
    fn replaces_retained_unusable_topic_placeholder_with_usable_topic_in_same_forum() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("forums")).unwrap();
        let post_hash = query_hash_hex("p=20830");
        let unlinked_post_hash = query_hash_hex("p=20831");
        let linked_placeholder = format!("viewtopic.php__q_{post_hash}.html");
        let unlinked_placeholder = format!("viewtopic.php__q_{unlinked_post_hash}.html");
        let error_html = "<table><tr><td align='center'><span class='gen'>The topic or post you requested does not exist</span></td></tr></table>";
        let valid_html = format!(
            r#"<title>SmallRockets Discussion :: View topic - Creating A Restricted Game Server</title><a name="20830"></a><a name="20831"></a><a href="{linked_placeholder}#20830">Post</a>"#
        );

        fs::write(root.join("forums").join(&linked_placeholder), error_html).unwrap();
        fs::write(root.join("forums").join(&unlinked_placeholder), error_html).unwrap();
        fs::write(root.join("forums/viewtopic.php__q_valid.html"), &valid_html).unwrap();

        let report = create_missing_topic_aliases(root).unwrap();

        assert_eq!(report.created, 2);
        assert_eq!(
            fs::read_to_string(root.join("forums").join(&linked_placeholder)).unwrap(),
            valid_html
        );
        assert_eq!(
            fs::read_to_string(root.join("forums").join(&unlinked_placeholder)).unwrap(),
            valid_html
        );
        // Second run must be idempotent now that both placeholders are usable.
        assert_eq!(create_missing_topic_aliases(root).unwrap().created, 0);
    }

    /// A retained unusable meta-refresh `index.html` must be replaced by a usable
    /// sibling `index.htm` in the same directory.
    #[test]
    fn directory_aliases_replace_unusable_redirect_index_html_from_usable_index_htm() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("_hosts/pc.example.com")).unwrap();
        fs::write(
            root.path().join("_hosts/pc.example.com/index.html"),
            r#"<META HTTP-EQUIV="Refresh" Content="1;URL=../../index.html">"#,
        )
        .unwrap();
        let usable = b"<TITLE>Small Rockets - The Games Company</TITLE><p>Real page</p>";
        fs::write(root.path().join("_hosts/pc.example.com/index.htm"), usable).unwrap();

        assert_eq!(create_missing_directory_aliases(root.path()).unwrap(), 1);
        assert_eq!(
            fs::read(root.path().join("_hosts/pc.example.com/index.html")).unwrap(),
            usable
        );
        assert_eq!(create_missing_directory_aliases(root.path()).unwrap(), 0);
    }

    /// `viewtopic.php?t=N&view=next` and `view=previous` request an adjacent topic,
    /// not topic `N` itself. Journal metadata for those URLs must neither alias missing
    /// next/previous links back to topic `N` nor overwrite unusable next/previous captures
    /// with topic `N`.
    #[test]
    fn does_not_alias_view_next_or_previous_back_to_source_topic() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("forums")).unwrap();
        fs::create_dir_all(root.join(".wayback-state")).unwrap();

        let topic_hash = query_hash_hex("t=10269");
        let next_hash = query_hash_hex("t=10269&view=next");
        let prev_hash = query_hash_hex("t=10269&view=previous");
        let topic_file = format!("viewtopic.php__q_{topic_hash}.html");
        let next_file = format!("viewtopic.php__q_{next_hash}.html");
        let prev_file = format!("viewtopic.php__q_{prev_hash}.html");

        let topic_html = format!(
            r#"<title>Forum :: View topic - A Fun Game</title><a href="{next_file}">View next topic</a><a href="{prev_file}">View previous topic</a><a href="posting.php?mode=reply&amp;t=10269">Reply</a>"#
        );
        let no_newer_html =
            "<table><tr><td align='center'><span class='gen'>There are no newer topics in this forum</span></td></tr></table>";

        fs::write(root.join("forums").join(&topic_file), &topic_html).unwrap();
        fs::write(root.join("forums").join(&next_file), no_newer_html).unwrap();

        let journal_events = [
            CaptureEvent {
                record: crate::cdx::CdxRecord {
                    timestamp: "20061125000000".to_owned(),
                    original: "http://www.example.com/forums/viewtopic.php?t=10269".to_owned(),
                    mimetype: "text/html".to_owned(),
                    status_code: 200,
                    digest: "TOPIC".to_owned(),
                    length: Some(300),
                },
                local_path: PathBuf::from(format!("forums/{topic_file}")),
                outcome: "downloaded".to_owned(),
                references: vec![
                    "http://www.example.com/forums/viewtopic.php?t=10269&view=next".to_owned(),
                    "http://www.example.com/forums/viewtopic.php?t=10269&view=previous".to_owned(),
                ],
                error: None,
            },
            CaptureEvent {
                record: crate::cdx::CdxRecord {
                    timestamp: "20061125000001".to_owned(),
                    original: "http://www.example.com/forums/viewtopic.php?t=10269&view=next"
                        .to_owned(),
                    mimetype: "text/html".to_owned(),
                    status_code: 200,
                    digest: "NONEWER".to_owned(),
                    length: Some(200),
                },
                local_path: PathBuf::from(format!("forums/{next_file}")),
                outcome: "unusable".to_owned(),
                references: Vec::new(),
                error: None,
            },
        ];
        let journal = journal_events
            .iter()
            .map(|event| serde_json::to_string(event).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(root.join(".wayback-state/captures.jsonl"), format!("{journal}\n")).unwrap();

        let report = create_missing_topic_aliases(root).unwrap();

        assert_eq!(report.created, 0);
        assert_eq!(
            fs::read_to_string(root.join("forums").join(&next_file)).unwrap(),
            no_newer_html
        );
        assert!(!root.join("forums").join(&prev_file).exists());
    }
}
