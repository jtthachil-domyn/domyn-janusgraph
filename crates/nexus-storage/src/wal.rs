//! Write-Ahead Log (WAL) for crash recovery.
//!
//! Every mutation (add vertex, add edge, set property) is serialized to the WAL
//! before being applied to in-memory structures. On crash recovery, the WAL is
//! replayed to rebuild state up to the last checkpoint.

use crate::error::{StorageError, StorageResult};
use crc32fast::Hasher;
use nexus_core::types::Value;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

/// A single logical operation that can be grouped into an atomic cross-model
/// commit record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WalOp {
    AddVertex {
        id: u64,
        label: String,
    },
    SetVertexProperty {
        vertex_id: u64,
        key: String,
        value: Value,
    },
    SetVertexLabel {
        vertex_id: u64,
        label: String,
    },
    AddEdge {
        edge_id: u64,
        source: u64,
        target: u64,
        label: String,
    },
    SetEdgeProperty {
        edge_id: u64,
        key: String,
        value: Value,
    },
    RemoveVertex {
        vertex_id: u64,
    },
    RemoveEdge {
        edge_id: u64,
    },
    UpsertDocument {
        collection: String,
        key: String,
        document: serde_json::Value,
    },
    DeleteDocument {
        collection: String,
        key: String,
    },
    UpsertVector {
        index: String,
        vertex_id: u64,
        embedding: Vec<f32>,
    },
    RemoveVector {
        index: String,
        vertex_id: u64,
    },
}

/// A single WAL entry representing either a legacy mutation or an atomic
/// cross-model commit. Legacy mutation variants are retained for old WAL files
/// and low-level tests; server writes should prefer `Commit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WalEntry {
    AddVertex {
        id: u64,
        label: String,
    },
    SetVertexProperty {
        vertex_id: u64,
        key: String,
        value: Value,
    },
    SetVertexLabel {
        vertex_id: u64,
        label: String,
    },
    AddEdge {
        edge_id: u64,
        source: u64,
        target: u64,
        label: String,
    },
    SetEdgeProperty {
        edge_id: u64,
        key: String,
        value: Value,
    },
    RemoveVertex {
        vertex_id: u64,
    },
    RemoveEdge {
        edge_id: u64,
    },
    UpsertDocument {
        collection: String,
        key: String,
        document: serde_json::Value,
    },
    DeleteDocument {
        collection: String,
        key: String,
    },
    Commit {
        tx_id: u64,
        ops: Vec<WalOp>,
    },
    Checkpoint {
        sequence: u64,
    },
}

impl WalEntry {
    pub fn ops(&self) -> Vec<WalOp> {
        match self {
            WalEntry::AddVertex { id, label } => vec![WalOp::AddVertex {
                id: *id,
                label: label.clone(),
            }],
            WalEntry::SetVertexProperty {
                vertex_id,
                key,
                value,
            } => vec![WalOp::SetVertexProperty {
                vertex_id: *vertex_id,
                key: key.clone(),
                value: value.clone(),
            }],
            WalEntry::SetVertexLabel { vertex_id, label } => vec![WalOp::SetVertexLabel {
                vertex_id: *vertex_id,
                label: label.clone(),
            }],
            WalEntry::AddEdge {
                edge_id,
                source,
                target,
                label,
            } => vec![WalOp::AddEdge {
                edge_id: *edge_id,
                source: *source,
                target: *target,
                label: label.clone(),
            }],
            WalEntry::SetEdgeProperty {
                edge_id,
                key,
                value,
            } => vec![WalOp::SetEdgeProperty {
                edge_id: *edge_id,
                key: key.clone(),
                value: value.clone(),
            }],
            WalEntry::RemoveVertex { vertex_id } => vec![WalOp::RemoveVertex {
                vertex_id: *vertex_id,
            }],
            WalEntry::RemoveEdge { edge_id } => vec![WalOp::RemoveEdge { edge_id: *edge_id }],
            WalEntry::UpsertDocument {
                collection,
                key,
                document,
            } => vec![WalOp::UpsertDocument {
                collection: collection.clone(),
                key: key.clone(),
                document: document.clone(),
            }],
            WalEntry::DeleteDocument { collection, key } => vec![WalOp::DeleteDocument {
                collection: collection.clone(),
                key: key.clone(),
            }],
            WalEntry::Commit { ops, .. } => ops.clone(),
            WalEntry::Checkpoint { .. } => Vec::new(),
        }
    }
}

/// WAL file lifecycle options.
///
/// Rotation bounds the live WAL file size. Rotated live segments remain part of
/// crash recovery until a durable snapshot compacts them. Retention applies
/// only after compaction, when redundant live segments can be archived without
/// affecting recovery.
#[derive(Debug, Clone)]
pub struct WalOptions {
    pub max_segment_bytes: Option<u64>,
    pub retained_archived_segments: usize,
    pub sync_interval: usize,
}

impl Default for WalOptions {
    fn default() -> Self {
        Self {
            max_segment_bytes: None,
            retained_archived_segments: 0,
            sync_interval: 1000,
        }
    }
}

impl WalOptions {
    pub fn with_rotation(max_segment_bytes: u64, retained_archived_segments: usize) -> Self {
        Self {
            max_segment_bytes: Some(max_segment_bytes),
            retained_archived_segments,
            ..Self::default()
        }
    }
}

/// Append-only WAL writer. Entries are fsynced in batches for durability.
pub struct WalWriter {
    path: PathBuf,
    writer: BufWriter<File>,
    sequence: u64,
    entries_since_sync: usize,
    active_bytes: u64,
    options: WalOptions,
}

impl WalWriter {
    pub fn open(path: impl AsRef<Path>) -> StorageResult<Self> {
        Self::open_with_options(path, WalOptions::default())
    }

    pub fn open_with_options(
        path: impl AsRef<Path>,
        mut options: WalOptions,
    ) -> StorageResult<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if options.sync_interval == 0 {
            options.sync_interval = 1;
        }

        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let active_bytes = file.metadata()?.len();

        let existing_count = Self::count_entries(&path)?;

        Ok(Self {
            path,
            writer: BufWriter::new(file),
            sequence: existing_count as u64,
            entries_since_sync: 0,
            active_bytes,
            options,
        })
    }

    pub fn append(&mut self, entry: &WalEntry) -> StorageResult<u64> {
        let json =
            serde_json::to_string(entry).map_err(|e| StorageError::Serialization(e.to_string()))?;

        let mut hasher = Hasher::new();
        hasher.update(json.as_bytes());
        let crc = hasher.finalize();

        let line = format!("{crc}\t{json}\n");
        self.writer.write_all(line.as_bytes())?;
        self.sequence += 1;
        self.entries_since_sync += 1;
        self.active_bytes += line.len() as u64;

        if self.entries_since_sync >= self.options.sync_interval {
            self.sync()?;
        }

        if self.should_rotate() {
            self.rotate()?;
        }

        Ok(self.sequence)
    }

    pub fn sync(&mut self) -> StorageResult<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        self.entries_since_sync = 0;
        Ok(())
    }

    pub fn checkpoint(&mut self) -> StorageResult<u64> {
        let seq = self.sequence;
        self.append(&WalEntry::Checkpoint { sequence: seq })?;
        self.sync()?;
        Ok(seq)
    }

    /// Rotate the active WAL into a numbered live segment.
    ///
    /// Live segments are still part of crash recovery. They are only archived or
    /// pruned by `compact()` after a durable snapshot has made them redundant.
    pub fn rotate(&mut self) -> StorageResult<Option<PathBuf>> {
        if self.active_bytes == 0 {
            return Ok(None);
        }

        self.sync()?;
        let segment_path = live_segment_path(&self.path, self.sequence);
        if segment_path.exists() {
            fs::remove_file(&segment_path)?;
        }
        fs::rename(&self.path, &segment_path)?;
        sync_parent_dir(&self.path)?;

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.writer = BufWriter::new(file);
        self.active_bytes = 0;
        self.entries_since_sync = 0;
        Ok(Some(segment_path))
    }

    /// Truncate the WAL after a durable snapshot/checkpoint has made all prior
    /// entries redundant.
    pub fn compact(&mut self) -> StorageResult<()> {
        self.compact_with_retention(self.options.retained_archived_segments)
    }

    pub fn compact_with_retention(
        &mut self,
        retained_archived_segments: usize,
    ) -> StorageResult<()> {
        self.sync()?;

        archive_or_prune_live_segments(&self.path, retained_archived_segments)?;

        {
            let file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.path)?;
            file.sync_all()?;
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.writer = BufWriter::new(file);
        self.sequence = 0;
        self.active_bytes = 0;
        self.entries_since_sync = 0;
        Ok(())
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    fn count_entries(path: &Path) -> StorageResult<usize> {
        Ok(WalReader::new(path).read_all()?.len())
    }

    fn should_rotate(&self) -> bool {
        self.options
            .max_segment_bytes
            .is_some_and(|limit| limit > 0 && self.active_bytes >= limit)
    }
}

/// WAL reader for crash recovery. Replays entries since the last checkpoint.
pub struct WalReader {
    path: PathBuf,
}

impl WalReader {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    /// Read all entries from the WAL, validating CRC32 checksums.
    /// Corrupted entries are skipped with a warning logged to tracing.
    pub fn read_all(&self) -> StorageResult<Vec<WalEntry>> {
        let mut entries = Vec::new();
        for path in read_paths(&self.path)? {
            read_entries_from_path(&path, &mut entries)?;
        }
        Ok(entries)
    }

    /// Read only entries after the last checkpoint (for recovery).
    pub fn read_since_last_checkpoint(&self) -> StorageResult<Vec<WalEntry>> {
        let all = self.read_all()?;

        let last_checkpoint_pos = all
            .iter()
            .rposition(|e| matches!(e, WalEntry::Checkpoint { .. }));

        match last_checkpoint_pos {
            Some(pos) => Ok(all[pos + 1..].to_vec()),
            None => Ok(all),
        }
    }
}

/// Truncate the WAL file (called after a successful snapshot).
pub fn truncate_wal(path: impl AsRef<Path>) -> StorageResult<()> {
    let path = path.as_ref();
    if path.exists() {
        File::create(path)?;
    }
    Ok(())
}

fn read_entries_from_path(path: &Path, entries: &mut Vec<WalEntry>) -> StorageResult<()> {
    if !path.exists() {
        return Ok(());
    }
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    for (line_num, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let (json, valid) = if let Some((crc_str, json_part)) = line.split_once('\t') {
            let stored_crc: u32 = match crc_str.parse() {
                Ok(c) => c,
                Err(_) => {
                    tracing::warn!(
                        file = %path.display(),
                        line = line_num + 1,
                        "WAL: invalid CRC format, skipping"
                    );
                    continue;
                }
            };
            let mut hasher = Hasher::new();
            hasher.update(json_part.as_bytes());
            let computed_crc = hasher.finalize();
            if stored_crc != computed_crc {
                tracing::warn!(
                    file = %path.display(),
                    line = line_num + 1,
                    stored_crc,
                    computed_crc,
                    "WAL: CRC mismatch, skipping corrupted entry"
                );
                continue;
            }
            (json_part, true)
        } else {
            (&line[..], false)
        };

        if !valid {
            tracing::warn!(
                file = %path.display(),
                line = line_num + 1,
                "WAL: legacy entry without CRC, accepting as-is"
            );
        }

        match serde_json::from_str(json) {
            Ok(entry) => entries.push(entry),
            Err(err) => {
                tracing::warn!(
                    file = %path.display(),
                    line = line_num + 1,
                    error = %err,
                    "WAL: malformed record, skipping"
                );
                continue;
            }
        }
    }

    Ok(())
}

fn read_paths(path: &Path) -> StorageResult<Vec<PathBuf>> {
    let mut paths = live_segment_paths(path)?;
    if path.exists() {
        paths.push(path.to_path_buf());
    }
    Ok(paths)
}

fn live_segment_path(path: &Path, sequence: u64) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("graph.wal");
    path.with_file_name(format!("{file_name}.seg.{sequence:020}"))
}

fn live_segment_paths(path: &Path) -> StorageResult<Vec<PathBuf>> {
    segment_paths_in_dir(path, path.parent().unwrap_or_else(|| Path::new(".")))
}

#[cfg(test)]
fn archived_segment_paths(path: &Path) -> StorageResult<Vec<PathBuf>> {
    segment_paths_in_dir(path, &archive_dir(path))
}

fn segment_paths_in_dir(path: &Path, dir: &Path) -> StorageResult<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let Some(base) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(Vec::new());
    };
    let prefix = format!("{base}.seg.");
    let mut segments: Vec<(u64, PathBuf)> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(suffix) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Ok(sequence) = suffix.parse::<u64>() else {
            continue;
        };
        segments.push((sequence, entry.path()));
    }
    segments.sort_by_key(|(sequence, _)| *sequence);
    Ok(segments.into_iter().map(|(_, path)| path).collect())
}

fn archive_or_prune_live_segments(
    path: &Path,
    retained_archived_segments: usize,
) -> StorageResult<()> {
    let segments = live_segment_paths(path)?;
    if segments.is_empty() {
        return Ok(());
    }

    let retain_from = segments.len().saturating_sub(retained_archived_segments);
    let archive_dir = archive_dir(path);
    if retained_archived_segments > 0 {
        fs::create_dir_all(&archive_dir)?;
    }

    for (idx, segment) in segments.into_iter().enumerate() {
        if idx < retain_from {
            fs::remove_file(segment)?;
            continue;
        }

        let Some(name) = segment.file_name() else {
            fs::remove_file(segment)?;
            continue;
        };
        let archived = archive_dir.join(name);
        if archived.exists() {
            fs::remove_file(&archived)?;
        }
        fs::rename(segment, archived)?;
    }

    sync_parent_dir(path)?;
    Ok(())
}

fn archive_dir(path: &Path) -> PathBuf {
    path.parent()
        .unwrap_or_else(|| Path::new("."))
        .join("wal-archive")
}

fn sync_parent_dir(path: &Path) -> StorageResult<()> {
    if let Some(parent) = path.parent() {
        if parent.exists() {
            File::open(parent)?.sync_all()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn wal_line(entry: &WalEntry) -> String {
        let json = serde_json::to_string(entry).unwrap();
        let mut hasher = Hasher::new();
        hasher.update(json.as_bytes());
        format!("{}\t{}\n", hasher.finalize(), json)
    }

    #[test]
    fn wal_write_and_read() {
        let dir = TempDir::new().unwrap();
        let wal_path = dir.path().join("test.wal");

        {
            let mut writer = WalWriter::open(&wal_path).unwrap();
            writer
                .append(&WalEntry::AddVertex {
                    id: 0,
                    label: "Entity".into(),
                })
                .unwrap();
            writer
                .append(&WalEntry::SetVertexProperty {
                    vertex_id: 0,
                    key: "name".into(),
                    value: Value::String("Apple Inc.".into()),
                })
                .unwrap();
            writer
                .append(&WalEntry::AddEdge {
                    edge_id: 0,
                    source: 0,
                    target: 1,
                    label: "Discloses".into(),
                })
                .unwrap();
            writer.sync().unwrap();
        }

        let reader = WalReader::new(&wal_path);
        let entries = reader.read_all().unwrap();
        assert_eq!(entries.len(), 3);

        match &entries[0] {
            WalEntry::AddVertex { id, label } => {
                assert_eq!(*id, 0);
                assert_eq!(label, "Entity");
            }
            _ => panic!("expected AddVertex"),
        }
    }

    #[test]
    fn wal_checkpoint_and_recovery() {
        let dir = TempDir::new().unwrap();
        let wal_path = dir.path().join("test.wal");

        {
            let mut writer = WalWriter::open(&wal_path).unwrap();
            writer
                .append(&WalEntry::AddVertex {
                    id: 0,
                    label: "A".into(),
                })
                .unwrap();
            writer
                .append(&WalEntry::AddVertex {
                    id: 1,
                    label: "B".into(),
                })
                .unwrap();
            writer.checkpoint().unwrap();
            writer
                .append(&WalEntry::AddVertex {
                    id: 2,
                    label: "C".into(),
                })
                .unwrap();
            writer
                .append(&WalEntry::AddVertex {
                    id: 3,
                    label: "D".into(),
                })
                .unwrap();
            writer.sync().unwrap();
        }

        let reader = WalReader::new(&wal_path);
        let since_checkpoint = reader.read_since_last_checkpoint().unwrap();
        assert_eq!(since_checkpoint.len(), 2);

        match &since_checkpoint[0] {
            WalEntry::AddVertex { id, label } => {
                assert_eq!(*id, 2);
                assert_eq!(label, "C");
            }
            _ => panic!("expected AddVertex for C"),
        }
    }

    #[test]
    fn wal_truncate() {
        let dir = TempDir::new().unwrap();
        let wal_path = dir.path().join("test.wal");

        {
            let mut writer = WalWriter::open(&wal_path).unwrap();
            writer
                .append(&WalEntry::AddVertex {
                    id: 0,
                    label: "X".into(),
                })
                .unwrap();
            writer.sync().unwrap();
        }

        truncate_wal(&wal_path).unwrap();

        let reader = WalReader::new(&wal_path);
        let entries = reader.read_all().unwrap();
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn wal_append_resumes_sequence() {
        let dir = TempDir::new().unwrap();
        let wal_path = dir.path().join("test.wal");

        {
            let mut writer = WalWriter::open(&wal_path).unwrap();
            writer
                .append(&WalEntry::AddVertex {
                    id: 0,
                    label: "A".into(),
                })
                .unwrap();
            writer
                .append(&WalEntry::AddVertex {
                    id: 1,
                    label: "B".into(),
                })
                .unwrap();
            writer.sync().unwrap();
            assert_eq!(writer.sequence(), 2);
        }

        {
            let mut writer = WalWriter::open(&wal_path).unwrap();
            assert_eq!(writer.sequence(), 2);
            writer
                .append(&WalEntry::AddVertex {
                    id: 2,
                    label: "C".into(),
                })
                .unwrap();
            writer.sync().unwrap();
            assert_eq!(writer.sequence(), 3);
        }

        let reader = WalReader::new(&wal_path);
        assert_eq!(reader.read_all().unwrap().len(), 3);
    }

    #[test]
    fn wal_reader_skips_truncated_crc_tail_record() {
        let dir = TempDir::new().unwrap();
        let wal_path = dir.path().join("test.wal");
        let mut contents = wal_line(&WalEntry::AddVertex {
            id: 0,
            label: "A".into(),
        });
        contents.push_str("123\t{\"AddVertex\":{\"id\":1,\"label\":\"B\"");
        fs::write(&wal_path, contents).unwrap();

        let entries = WalReader::new(&wal_path).read_all().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0], WalEntry::AddVertex { id: 0, .. }));
    }

    #[test]
    fn wal_reader_skips_malformed_crc_valid_record() {
        let dir = TempDir::new().unwrap();
        let wal_path = dir.path().join("test.wal");
        let malformed = "{\"not_a_wal_entry\":true}";
        let mut hasher = Hasher::new();
        hasher.update(malformed.as_bytes());
        let contents = format!(
            "{}{}\t{}\n",
            wal_line(&WalEntry::AddVertex {
                id: 0,
                label: "A".into(),
            }),
            hasher.finalize(),
            malformed
        );
        fs::write(&wal_path, contents).unwrap();

        let entries = WalReader::new(&wal_path).read_all().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0], WalEntry::AddVertex { id: 0, .. }));
    }

    #[test]
    fn wal_compact_truncates_and_resets_sequence() {
        let dir = TempDir::new().unwrap();
        let wal_path = dir.path().join("test.wal");

        let mut writer = WalWriter::open(&wal_path).unwrap();
        writer
            .append(&WalEntry::AddVertex {
                id: 0,
                label: "A".into(),
            })
            .unwrap();
        writer
            .append(&WalEntry::AddVertex {
                id: 1,
                label: "B".into(),
            })
            .unwrap();
        writer.compact().unwrap();

        assert_eq!(writer.sequence(), 0);
        assert!(WalReader::new(&wal_path).read_all().unwrap().is_empty());

        writer
            .append(&WalEntry::AddVertex {
                id: 2,
                label: "C".into(),
            })
            .unwrap();
        writer.sync().unwrap();

        let entries = WalReader::new(&wal_path).read_all().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0], WalEntry::AddVertex { id: 2, .. }));
    }

    #[test]
    fn wal_rotation_reads_across_live_segments() {
        let dir = TempDir::new().unwrap();
        let wal_path = dir.path().join("test.wal");

        {
            let mut writer =
                WalWriter::open_with_options(&wal_path, WalOptions::with_rotation(1, 0)).unwrap();
            for id in 0..4 {
                writer
                    .append(&WalEntry::AddVertex {
                        id,
                        label: format!("V{id}"),
                    })
                    .unwrap();
            }
            writer.sync().unwrap();
            assert_eq!(writer.sequence(), 4);
        }

        assert!(
            !live_segment_paths(&wal_path).unwrap().is_empty(),
            "small segment limit should rotate the WAL"
        );
        let entries = WalReader::new(&wal_path).read_all().unwrap();
        assert_eq!(entries.len(), 4);
        assert!(matches!(entries[0], WalEntry::AddVertex { id: 0, .. }));
        assert!(matches!(entries[3], WalEntry::AddVertex { id: 3, .. }));

        let reopened =
            WalWriter::open_with_options(&wal_path, WalOptions::with_rotation(1, 0)).unwrap();
        assert_eq!(reopened.sequence(), 4);
    }

    #[test]
    fn wal_recovery_finds_checkpoint_across_rotated_segments() {
        let dir = TempDir::new().unwrap();
        let wal_path = dir.path().join("test.wal");

        {
            let mut writer =
                WalWriter::open_with_options(&wal_path, WalOptions::with_rotation(1, 0)).unwrap();
            writer
                .append(&WalEntry::AddVertex {
                    id: 0,
                    label: "A".into(),
                })
                .unwrap();
            writer
                .append(&WalEntry::AddVertex {
                    id: 1,
                    label: "B".into(),
                })
                .unwrap();
            writer.checkpoint().unwrap();
            writer
                .append(&WalEntry::AddVertex {
                    id: 2,
                    label: "C".into(),
                })
                .unwrap();
            writer
                .append(&WalEntry::AddVertex {
                    id: 3,
                    label: "D".into(),
                })
                .unwrap();
            writer.sync().unwrap();
        }

        let since_checkpoint = WalReader::new(&wal_path)
            .read_since_last_checkpoint()
            .unwrap();
        assert_eq!(since_checkpoint.len(), 2);
        assert!(matches!(
            since_checkpoint[0],
            WalEntry::AddVertex { id: 2, .. }
        ));
        assert!(matches!(
            since_checkpoint[1],
            WalEntry::AddVertex { id: 3, .. }
        ));
    }

    #[test]
    fn wal_compaction_archives_retained_rotated_segments() {
        let dir = TempDir::new().unwrap();
        let wal_path = dir.path().join("test.wal");

        let mut writer =
            WalWriter::open_with_options(&wal_path, WalOptions::with_rotation(1, 1)).unwrap();
        for id in 0..4 {
            writer
                .append(&WalEntry::AddVertex {
                    id,
                    label: format!("V{id}"),
                })
                .unwrap();
        }

        assert!(live_segment_paths(&wal_path).unwrap().len() >= 2);
        writer.compact().unwrap();

        assert!(live_segment_paths(&wal_path).unwrap().is_empty());
        assert_eq!(archived_segment_paths(&wal_path).unwrap().len(), 1);
        assert!(WalReader::new(&wal_path).read_all().unwrap().is_empty());

        writer
            .append(&WalEntry::AddVertex {
                id: 4,
                label: "V4".into(),
            })
            .unwrap();
        writer.sync().unwrap();

        let entries = WalReader::new(&wal_path).read_all().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0], WalEntry::AddVertex { id: 4, .. }));
    }
}
