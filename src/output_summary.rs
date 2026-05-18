use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileSummary {
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputSummary {
    pub total_bytes: u64,
    pub biggest_files: Vec<FileSummary>,
}

pub fn summarize_output_dir(root: &Path, limit: usize) -> Result<OutputSummary> {
    let mut total_bytes = 0;
    let mut biggest_files = Vec::new();

    walk_dir(root, root, limit, &mut total_bytes, &mut biggest_files)?;

    Ok(OutputSummary {
        total_bytes,
        biggest_files,
    })
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

    if bytes < 1024 {
        return format!("{bytes} B");
    }

    let mut value = bytes as f64;
    let mut unit_index = 0;
    while value >= 1024.0 && unit_index < UNITS.len() - 1 {
        value /= 1024.0;
        unit_index += 1;
    }

    if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit_index])
    } else {
        format!("{value:.2} {}", UNITS[unit_index])
    }
}

fn walk_dir(
    root: &Path,
    current: &Path,
    limit: usize,
    total_bytes: &mut u64,
    biggest_files: &mut Vec<FileSummary>,
) -> Result<()> {
    let entries = fs::read_dir(current)
        .with_context(|| format!("failed to read directory {}", current.display()))?;

    for entry in entries {
        let entry = entry
            .with_context(|| format!("failed to read directory entry in {}", current.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to read file type for {}", entry.path().display()))?;
        let path = entry.path();

        if file_type.is_dir() {
            walk_dir(root, &path, limit, total_bytes, biggest_files)?;
        } else if file_type.is_file() {
            let metadata = entry
                .metadata()
                .with_context(|| format!("failed to read metadata for {}", path.display()))?;
            let bytes = metadata.len();
            *total_bytes += bytes;
            consider_file(root, &path, bytes, limit, biggest_files);
        }
    }

    Ok(())
}

fn consider_file(
    root: &Path,
    path: &Path,
    bytes: u64,
    limit: usize,
    biggest_files: &mut Vec<FileSummary>,
) {
    if limit == 0 {
        return;
    }

    let relative_path = path.strip_prefix(root).unwrap_or(path).to_path_buf();
    biggest_files.push(FileSummary {
        path: relative_path,
        bytes,
    });
    biggest_files.sort_by(|left, right| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.path.cmp(&right.path))
    });
    biggest_files.truncate(limit);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes_for_human_output() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(10 * 1024), "10.0 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.00 MiB");
    }

    #[test]
    fn summarizes_total_size_and_biggest_files() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir(root.join("nested")).unwrap();
        fs::write(root.join("small.txt"), [0_u8; 1]).unwrap();
        fs::write(root.join("medium.bin"), [0_u8; 4]).unwrap();
        fs::write(root.join("nested").join("large.dat"), [0_u8; 8]).unwrap();

        let summary = summarize_output_dir(root, 2).unwrap();

        assert_eq!(summary.total_bytes, 13);
        assert_eq!(
            summary.biggest_files,
            vec![
                FileSummary {
                    path: PathBuf::from("nested/large.dat"),
                    bytes: 8,
                },
                FileSummary {
                    path: PathBuf::from("medium.bin"),
                    bytes: 4,
                },
            ]
        );
    }

    #[test]
    fn keeps_biggest_files_in_stable_path_order_for_ties() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("b.bin"), [0_u8; 4]).unwrap();
        fs::write(root.join("a.bin"), [0_u8; 4]).unwrap();

        let summary = summarize_output_dir(root, 10).unwrap();

        assert_eq!(
            summary.biggest_files,
            vec![
                FileSummary {
                    path: PathBuf::from("a.bin"),
                    bytes: 4,
                },
                FileSummary {
                    path: PathBuf::from("b.bin"),
                    bytes: 4,
                },
            ]
        );
    }
}
