//! Distributed-readiness primitives for Domyn Nexus.
//!
//! This crate intentionally does **not** implement Raft, sharding, or
//! cross-shard Cypher yet. It defines stable metadata and replication-log
//! shapes that let the single-node engine grow into leader-follower
//! replication without changing the durable write semantics later.

use nexus_storage::wal::WalEntry;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NodeId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ShardId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ReplicaId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplicationRole {
    SingleNode,
    Leader,
    Follower,
    Learner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadConsistency {
    Local,
    LeaderLinearizable,
    FollowerStale,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantPlacement {
    pub tenant: String,
    pub shard: ShardId,
    pub primary: ReplicaId,
    pub replicas: Vec<ReplicaId>,
}

impl TenantPlacement {
    pub fn single_shard(tenant: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            shard: ShardId(0),
            primary: ReplicaId(0),
            replicas: vec![ReplicaId(0)],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementMap {
    default_shard: ShardId,
    tenants: BTreeMap<String, TenantPlacement>,
}

impl Default for PlacementMap {
    fn default() -> Self {
        Self::single_shard()
    }
}

impl PlacementMap {
    pub fn single_shard() -> Self {
        Self {
            default_shard: ShardId(0),
            tenants: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, placement: TenantPlacement) -> Result<(), DistributedError> {
        if placement.replicas.is_empty() {
            return Err(DistributedError::InvalidPlacement(
                "tenant placement must include at least one replica".into(),
            ));
        }
        if !placement.replicas.contains(&placement.primary) {
            return Err(DistributedError::InvalidPlacement(
                "primary replica must be present in replica set".into(),
            ));
        }
        self.tenants.insert(placement.tenant.clone(), placement);
        Ok(())
    }

    pub fn resolve(&self, tenant: &str) -> Option<&TenantPlacement> {
        self.tenants.get(tenant)
    }

    pub fn resolve_or_default(&self, tenant: &str) -> TenantPlacement {
        self.resolve(tenant)
            .cloned()
            .unwrap_or_else(|| TenantPlacement {
                tenant: tenant.to_string(),
                shard: self.default_shard,
                primary: ReplicaId(0),
                replicas: vec![ReplicaId(0)],
            })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicationOp {
    pub tenant: Option<String>,
    pub shard: ShardId,
    pub wal_entry: WalEntry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitRecord {
    pub node: NodeId,
    pub shard: ShardId,
    pub role: ReplicationRole,
    pub log_index: u64,
    pub ops: Vec<ReplicationOp>,
}

impl CommitRecord {
    pub fn stable_json(&self) -> Result<String, DistributedError> {
        serde_json::to_string(self).map_err(|err| DistributedError::Serialization(err.to_string()))
    }
}

pub trait ShardLocalEngine {
    type Error;

    fn apply_commit_record(&self, record: &CommitRecord) -> Result<(), Self::Error>;
}

#[derive(Debug, Error)]
pub enum DistributedError {
    #[error("invalid placement: {0}")]
    InvalidPlacement(String),

    #[error("serialization error: {0}")]
    Serialization(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::types::Value;

    #[test]
    fn placement_map_routes_known_tenant() {
        let mut map = PlacementMap::single_shard();
        map.insert(TenantPlacement {
            tenant: "tenant-a".into(),
            shard: ShardId(7),
            primary: ReplicaId(70),
            replicas: vec![ReplicaId(70), ReplicaId(71)],
        })
        .unwrap();

        let placement = map.resolve_or_default("tenant-a");
        assert_eq!(placement.shard, ShardId(7));
        assert_eq!(placement.primary, ReplicaId(70));
    }

    #[test]
    fn placement_map_defaults_unknown_tenant_to_single_shard() {
        let map = PlacementMap::single_shard();
        let placement = map.resolve_or_default("missing");
        assert_eq!(placement.tenant, "missing");
        assert_eq!(placement.shard, ShardId(0));
        assert_eq!(placement.replicas, vec![ReplicaId(0)]);
    }

    #[test]
    fn placement_rejects_primary_outside_replica_set() {
        let mut map = PlacementMap::single_shard();
        let err = map
            .insert(TenantPlacement {
                tenant: "tenant-a".into(),
                shard: ShardId(1),
                primary: ReplicaId(10),
                replicas: vec![ReplicaId(11)],
            })
            .unwrap_err();
        assert!(err.to_string().contains("primary replica"));
    }

    #[test]
    fn commit_record_serialization_is_deterministic() {
        let record = CommitRecord {
            node: NodeId(1),
            shard: ShardId(2),
            role: ReplicationRole::Leader,
            log_index: 42,
            ops: vec![ReplicationOp {
                tenant: Some("tenant-a".into()),
                shard: ShardId(2),
                wal_entry: WalEntry::SetVertexProperty {
                    vertex_id: 7,
                    key: "name".into(),
                    value: Value::String("Alice".into()),
                },
            }],
        };

        assert_eq!(record.stable_json().unwrap(), record.stable_json().unwrap());
    }
}
