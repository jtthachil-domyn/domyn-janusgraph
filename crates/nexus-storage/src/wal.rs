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

/// A single WAL entry representing an atomic mutation.
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
    Checkpoint {
        sequence: u64,
    },
}

/// Append-only WAL writer. Entries are fsynced in batches for durability.
pub struct WalWriter {
    path: PathBuf,
    writer: BufWriter<File>,
    sequence: u64,
    entries_since_sync: usize,
    sync_interval: usize,
}

impl WalWriter {
    pub fn open(path: impl AsRef<Path>) -> StorageResult<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new().create(true).append(true).open(&path)?;

        let existing_count = Self::count_entries(&path)?;

        Ok(Self {
            path,
            writer: BufWriter::new(file),
            sequence: existing_count as u64,
            entries_since_sync: 0,
            sync_interval: 1000,
        })
    }

    pub fn append(&mut self, entry: &WalEntry) -> StorageResult<u64> {
        let json =
            serde_json::to_string(entry).map_err(|e| StorageError::Serialization(e.to_string()))?;

        let mut hasher = Hasher::new();
        hasher.update(json.as_bytes());
        let crc = hasher.finalize();

        writeln!(self.writer, "{crc}\t{json}")?;
        self.sequence += 1;
        self.entries_since_sync += 1;

        if self.entries_since_sync >= self.sync_interval {
            self.sync()?;
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

    /// Truncate the WAL after a durable snapshot/checkpoint has made all prior
    /// entries redundant.
    pub fn compact(&mut self) -> StorageResult<()> {
        self.sync()?;

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
        self.entries_since_sync = 0;
        Ok(())
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    fn count_entries(path: &Path) -> StorageResult<usize> {
        if !path.exists() {
            return Ok(0);
        }
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        Ok(reader.lines().count())
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
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut entries = Vec::new();

        for (line_num, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }

            let (json, valid) = if let Some((crc_str, json_part)) = line.split_once('\t') {
                let stored_crc: u32 = match crc_str.parse() {
                    Ok(c) => c,
                    Err(_) => {
                        tracing::warn!(line = line_num + 1, "WAL: invalid CRC format, skipping");
                        continue;
                    }
                };
                let mut hasher = Hasher::new();
                hasher.update(json_part.as_bytes());
                let computed_crc = hasher.finalize();
                if stored_crc != computed_crc {
                    tracing::warn!(
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
                    line = line_num + 1,
                    "WAL: legacy entry without CRC, accepting as-is"
                );
            }

            match serde_json::from_str(json) {
                Ok(entry) => entries.push(entry),
                Err(err) => {
                    tracing::warn!(
                        line = line_num + 1,
                        error = %err,
                        "WAL: malformed record, skipping"
                    );
                    continue;
                }
            }
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
}
