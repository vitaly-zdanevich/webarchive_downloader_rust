//! Durable recovery evidence and run-local capture lookup reuse.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cdx::CdxRecord;

/// One replay outcome; original references survive irreversible local query hashes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CaptureEvent {
	pub record: CdxRecord,
	pub local_path: PathBuf,
	pub outcome: String,
	pub references: Vec<String>,
	pub error: Option<String>,
}

/// One local file substitution created because an exact capture was missing or unusable.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct AliasEvent {
	pub target_path: PathBuf,
	pub source_path: PathBuf,
	pub reason: String,
	pub replaced_unusable: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub referred_from: Option<PathBuf>,
}

/// Appends one substitution provenance record to `.wayback-state/aliases.jsonl`.
pub(crate) fn record_alias_event(root: &Path, event: &AliasEvent) -> Result<()> {
	let directory = root.join(".wayback-state");
	fs::create_dir_all(&directory).context("failed to create recovery state directory")?;
	let path = directory.join("aliases.jsonl");
	let needs_leading_newline = match fs::read(&path) {
		Ok(bytes) => !bytes.is_empty() && !bytes.ends_with(b"\n"),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
		Err(error) => return Err(error).context("failed to inspect alias provenance journal"),
	};
	let mut file = OpenOptions::new()
		.create(true)
		.append(true)
		.open(&path)
		.context("failed to open alias provenance journal")?;
	if needs_leading_newline {
		file.write_all(b"\n")?;
	}
	let mut bytes = serde_json::to_vec(event)?;
	bytes.push(b'\n');
	file.write_all(&bytes)
		.context("failed to append alias provenance journal")?;
	file.flush()?;
	Ok(())
}

/// Keeps successful capture evidence separate from later unsuccessful attempts.
#[derive(Debug, Default)]
struct State {
	file: Option<File>,
	captures: HashMap<PathBuf, CaptureEvent>,
	lookups: HashMap<String, Vec<CdxRecord>>,
}

/// Shared by all download/recovery passes; failed CDX requests are never cached.
#[derive(Debug, Default)]
pub(crate) struct RecoveryState(Mutex<State>);

impl RecoveryState {
	/// Opens an append-only journal, tolerating an interrupted final write.
	pub fn open(root: &Path) -> Result<Self> {
		let directory = root.join(".wayback-state");
		fs::create_dir_all(&directory).context("failed to create recovery state directory")?;
		let path = directory.join("captures.jsonl");
		let bytes = match fs::read(&path) {
			Ok(bytes) => bytes,
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
			Err(error) => return Err(error).context("failed to read recovery journal"),
		};
		let mut state = State::default();
		let mut damaged = 0;
		for line in bytes.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
			match serde_json::from_slice::<CaptureEvent>(line) {
				Ok(event) if matches!(event.outcome.as_str(), "downloaded" | "unusable") => {
					state.captures.insert(event.local_path.clone(), event);
				}
				Ok(_) => {},
				Err(_) => damaged += 1,
			}
		}
		if damaged > 0 {
			eprintln!("recovery journal: ignored {damaged} incomplete/invalid entries; files will still be checked");
		}
		let mut file = OpenOptions::new().create(true).append(true).open(&path)
			.context("failed to open recovery journal")?;
		if !bytes.is_empty() && !bytes.ends_with(b"\n") {
			file.write_all(b"\n")?;
		}
		state.file = Some(file);
		Ok(Self(Mutex::new(state)))
	}

	/// Appends an outcome before updating the in-memory successful-capture map.
	pub fn record(&self, event: CaptureEvent) -> Result<()> {
		let mut state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
		if let Some(file) = &mut state.file {
			let mut bytes = serde_json::to_vec(&event)?;
			bytes.push(b'\n');
			file.write_all(&bytes).context("failed to append recovery journal")?;
			file.flush()?;
		}
		if matches!(event.outcome.as_str(), "downloaded" | "unusable") {
			state.captures.insert(event.local_path.clone(), event);
		}
		Ok(())
	}

	/// Returns raw references from the last saved capture, not from failed retries.
	pub fn references(&self, path: &Path) -> Vec<String> {
		self.0.lock().unwrap_or_else(|poison| poison.into_inner()).captures.get(path)
			.map(|entry| entry.references.clone()).unwrap_or_default()
	}

	/// Looks up a previously saved original URL without guessing a query hash.
	pub fn original_for_path(&self, path: &Path) -> Option<String> {
		self.0.lock().unwrap_or_else(|poison| poison.into_inner()).captures.get(path)
			.map(|entry| entry.record.original.clone())
	}

	/// Reuses only completed CDX responses within this run and query/date scope.
	pub fn lookup(&self, key: &str) -> Option<Vec<CdxRecord>> {
		self.0.lock().unwrap_or_else(|poison| poison.into_inner()).lookups.get(key).cloned()
	}

	/// Caches a successful response, including a genuinely empty result, for this run only.
	pub fn remember_lookup(&self, key: String, records: Vec<CdxRecord>) {
		self.0.lock().unwrap_or_else(|poison| poison.into_inner()).lookups.insert(key, records);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A failed retry or interrupted journal tail must not erase original references.
	#[test]
	fn reload_keeps_saved_references_after_failed_retry_and_partial_write() {
		let root = tempfile::tempdir().unwrap();
		let event = CaptureEvent {
			record: CdxRecord { timestamp: "20260917000000".into(), original: "http://example.com/".into(), mimetype: "text/html".into(), status_code: 200, digest: "A".into(), length: None },
			local_path: "index.html".into(), outcome: "downloaded".into(),
			references: vec!["http://example.com/topic.php?t=7".into()], error: None,
		};
		let state = RecoveryState::open(root.path()).unwrap();
		state.record(event.clone()).unwrap();
		state.record(CaptureEvent { outcome: "failed".into(), references: Vec::new(), ..event.clone() }).unwrap();
		drop(state);
		OpenOptions::new().append(true).open(root.path().join(".wayback-state/captures.jsonl")).unwrap().write_all(b"{\"record\":").unwrap();
		let state = RecoveryState::open(root.path()).unwrap();
		assert_eq!(state.references(Path::new("index.html")), event.references);
		state.record(event.clone()).unwrap();
		drop(state);
		let state = RecoveryState::open(root.path()).unwrap();
		assert_eq!(state.references(Path::new("index.html")), event.references);
	}

	/// Empty lookup results are retried on the next run, not remembered forever.
	#[test]
	fn lookup_cache_does_not_survive_restart() {
		let root = tempfile::tempdir().unwrap();
		let state = RecoveryState::open(root.path()).unwrap();
		state.remember_lookup("query".into(), Vec::new());
		assert_eq!(state.lookup("query"), Some(Vec::new()));
		assert_eq!(RecoveryState::open(root.path()).unwrap().lookup("query"), None);
	}

	/// Alias provenance appends valid JSONL entries even after a truncated prior line.
	#[test]
	fn record_alias_event_appends_jsonl_entries() {
		let root = tempfile::tempdir().unwrap();
		let first = AliasEvent {
			target_path: PathBuf::from("forums/viewtopic.php__q_a.html"),
			source_path: PathBuf::from("forums/viewtopic.php__q_b.html"),
			reason: "topic_post_anchor".into(),
			replaced_unusable: false,
			referred_from: Some(PathBuf::from("forums/index.html")),
		};
		record_alias_event(root.path(), &first).unwrap();
		OpenOptions::new()
			.append(true)
			.open(root.path().join(".wayback-state/aliases.jsonl"))
			.unwrap()
			.write_all(b"{\"partial\":")
			.unwrap();
		let second = AliasEvent {
			target_path: PathBuf::from("press/index.html"),
			source_path: PathBuf::from("press/index.htm"),
			reason: "directory_index".into(),
			replaced_unusable: true,
			referred_from: None,
		};
		record_alias_event(root.path(), &second).unwrap();

		let content = fs::read_to_string(root.path().join(".wayback-state/aliases.jsonl")).unwrap();
		let parsed: Vec<AliasEvent> = content
			.lines()
			.filter_map(|line| serde_json::from_str::<AliasEvent>(line).ok())
			.collect();
		assert_eq!(parsed, vec![first, second]);
	}
}
