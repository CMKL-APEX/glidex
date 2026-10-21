//! glidex's schema for the replicated store (spec/clustering.md §6.1): the
//! tables of `glidex.db` and who writes. The store itself, its write path and
//! its Raft integration are in `glidex-raftstore`; the names below are
//! glidex's instances of its types.

use glidex_raftstore::store as rs;
use serde::{Deserialize, Serialize};

pub use rs::{
    Bell, Consistency, Def, Forwarder, LAST_APPLIED, LAST_MEMBERSHIP, MAX_WRITE_SET_BYTES,
    MIRROR_REVISION, Mirror, Replicator, StoreError, Submitted, WRITE_SET_FORMAT, new_bell, ring,
};

/// glidex's [`rs::Schema`]: the tables of `glidex.db`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Glidex;

pub type Db = rs::Db<Glidex>;
pub type Tx<'db> = rs::Tx<'db, Glidex>;
pub type RecTable<'tx> = rs::RecTable<'tx, Glidex>;
pub type Op = rs::Op<Glidex>;
pub type WriteSet = rs::WriteSet<Glidex>;
pub type Applied = rs::Applied<Glidex>;
pub type SnapshotView = rs::SnapshotView<Glidex>;

macro_rules! tables {
    ($($variant:ident = $id:literal => $name:literal),+ $(,)?) => {
        /// Every table of `glidex.db`. **Append-only** (§6.6): a table can be
        /// retired, but its id is never reused.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[repr(u16)]
        pub enum TableId { $($variant = $id),+ }

        impl TableId {
            pub const ALL: &'static [TableId] = &[$(TableId::$variant),+];
            pub fn name(self) -> &'static str { match self { $(TableId::$variant => $name),+ } }
            pub fn from_name(name: &str) -> Option<TableId> { match name { $($name => Some(TableId::$variant),)+ _ => None } }
            pub fn from_id(id: u16) -> Option<TableId> { match id { $($id => Some(TableId::$variant),)+ _ => None } }
            /// Not replicated: each replica's own bookkeeping, left out of
            /// snapshots and database comparisons.
            pub fn is_local(self) -> bool { matches!(self, TableId::RaftMeta) }
            pub fn definition(self) -> Def { rs::definition::<Glidex>(self) }
        }
    };
}

tables! {
    Vms = 1 => "vms",
    Events = 2 => "events",
    Meta = 3 => "meta",
    Audit = 4 => "audit",
    Projects = 5 => "projects",
    Networks = 6 => "networks",
    Credentials = 7 => "credentials",
    Images = 8 => "images",
    Disks = 9 => "disks",
    ImageMeta = 10 => "image_meta",
    Users = 11 => "users",
    Identities = 12 => "identities",
    Teams = 13 => "teams",
    PolicyLinks = 14 => "policy_links",
    Sessions = 15 => "sessions",
    ApiTokens = 16 => "api_tokens",
    SitePolicies = 17 => "site_policies",
    SitePolicyVersions = 18 => "site_policy_versions",
    MeterCursors = 19 => "meter_cursors",
    MeterOpen = 20 => "meter_open",
    UsageHourly = 21 => "usage_hourly",
    MeterMeta = 22 => "meter_meta",
    Rate5m = 23 => "rate_5m",
    UsageDaily = 24 => "usage_daily",
    UsageMonthlyRates = 25 => "usage_monthly_rates",
    Nodes = 26 => "nodes",
    // Local to each replica: last_applied, last_membership (§6.2).
    RaftMeta = 27 => "raft_meta",
    JoinTokens = 28 => "join_tokens",
    NodeDenylist = 29 => "node_denylist",
    IssuedCerts = 30 => "issued_certs",
    CaBundle = 31 => "ca_bundle",
    ImageCaches = 32 => "image_caches",
    LedgerInbox = 33 => "ledger_inbox",
    IpamSubnets = 34 => "ipam_subnets",
    IpamReservations = 35 => "ipam_reservations",
    Routers = 36 => "routers",
    IpamExternal = 37 => "ipam_external",
    // Tenants, directories and admissions (spec/tenancy.md §4).
    Directories = 38 => "directories",
    Tenants = 39 => "tenants",
    Admissions = 40 => "admissions",
    TenantMembers = 41 => "tenant_members",
    // Directory connectors and their sealed secrets (spec/tenancy.md §5, §13).
    Connectors = 42 => "connectors",
    ConnectorSecrets = 43 => "connector_secrets",
    DirectoryKeys = 44 => "directory_keys",
    // Invitations to tenants (spec/tenancy.md §6.1).
    Invitations = 45 => "invitations",
    // SCIM bearer tokens, by hash (spec/tenancy.md §8.1).
    ScimTokens = 46 => "scim_tokens",
    // Groups an IdP pushed over SCIM (spec/tenancy.md D43).
    ScimGroups = 47 => "scim_groups",
    // Each pulled connector's last run (spec/tenancy.md §8.2).
    ConnectorSync = 48 => "connector_sync",
    // Datastores and what each node finds at them (spec/storage.md §4.1).
    Datastores = 49 => "datastores",
    DatastoreNodes = 50 => "datastore_nodes",
    // Tenants' shares of cluster device pools (spec/host-topology.md §9.1a).
    TenantDevicePools = 51 => "tenant_device_pools",
    // Workload federations (spec/tenancy.md §9).
    Federations = 52 => "federations",
    // The tokens exchanges gave, and the workload tokens they used (§9, D48).
    FederatedTokens = 53 => "federated_tokens",
    // Passkeys and users' WebAuthn handles (spec/passkeys.md).
    Passkeys = 54 => "passkeys",
    // Each directory's token signing keys, the private half sealed (D53).
    SigningKeys = 55 => "signing_keys",
    // The directory's refresh tokens, by hash (D55).
    RefreshTokens = 56 => "refresh_tokens",
    // This cluster's own key, the private half sealed (D63).
    ClusterKeys = 57 => "cluster_keys",
    // Admissions offered to other directories' tenants (D64).
    AdmissionOffers = 58 => "admission_offers",
    // Directories of other organizations this cluster trusts (D65).
    TrustedDirectories = 59 => "trusted_directories",
    // Clusters of other organizations a directory's tenants use (D65).
    TrustedClusters = 60 => "trusted_clusters",
    // What each trusted directory's feed last wrote here (D68).
    FeedState = 61 => "feed_state",
    // The directory's chains ended in the last 12 h, for feeds (D67).
    EndedSids = 62 => "ended_sids",
}

/// Whose a table's records are (spec/tenancy.md §4): a directory's (they
/// follow the organization), this cluster's (they follow the hardware), or
/// both (role links: on a tenant or project, the directory's; on the
/// cluster or a host, the cluster's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Directory,
    Cluster,
    Both,
}

impl TableId {
    pub fn scope(self) -> Scope {
        use TableId::*;
        match self {
            Users | Identities | Teams | ApiTokens | Projects | Credentials | Directories
            | Tenants | TenantMembers | Connectors | ConnectorSecrets | DirectoryKeys
            | Invitations | ScimTokens | ScimGroups | ConnectorSync | Federations
            | FederatedTokens | Passkeys | SigningKeys | RefreshTokens | TrustedClusters
            | EndedSids => Scope::Directory,
            PolicyLinks | Audit | Meta => Scope::Both,
            _ => Scope::Cluster,
        }
    }
}

impl Serialize for TableId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u16(*self as u16)
    }
}

impl<'de> Deserialize<'de> for TableId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let id = u16::deserialize(d)?;
        TableId::from_id(id)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown table id {id}")))
    }
}

/// Who asked for a write; kept in the log for audit and debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Origin {
    Api,
    Controller,
    Auth,
    Metering,
    Images,
    Network,
    Migration,
    System,
}

impl Origin {
    /// Writes a node makes about its own work, which a server that isn't
    /// the leader sends to the leader (spec/clustering.md §8.3). The
    /// leader decides what the node may write (`check_status_write`).
    pub fn is_node_status(self) -> bool {
        matches!(self, Origin::Controller | Origin::Images | Origin::Network)
    }
}

impl rs::Schema for Glidex {
    type Table = TableId;
    type Origin = Origin;
    const TABLES: &'static [TableId] = TableId::ALL;
    const LOCAL: TableId = TableId::RaftMeta;
    const SYSTEM: Origin = Origin::System;
    const SNAPSHOT_MAGIC: &'static [u8; 8] = b"GXSNAP1\n";
    const CHECK_ENV: &'static str = "GLIDEX_CHECK_WRITE_SETS";

    fn table_name(t: TableId) -> &'static str {
        t.name()
    }
    fn table_id(t: TableId) -> u16 {
        t as u16
    }
    fn table_from_id(id: u16) -> Option<TableId> {
        TableId::from_id(id)
    }
    fn table_from_name(name: &str) -> Option<TableId> {
        TableId::from_name(name)
    }
    fn forwarded(origin: Origin) -> bool {
        origin.is_node_status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_ids_are_stable() {
        // Append-only (§6.6): these numbers are on disk in Raft logs.
        assert_eq!(
            (
                TableId::Vms as u16,
                TableId::Audit as u16,
                TableId::UsageMonthlyRates as u16,
                TableId::Nodes as u16
            ),
            (1, 4, 25, 26)
        );
        for &t in TableId::ALL {
            assert_eq!(TableId::from_name(t.name()), Some(t));
            assert_eq!(TableId::from_id(t as u16), Some(t));
        }
    }

    /// Write sets, Raft log entries and snapshots are on disk in existing
    /// clusters: their bytes must be what glidex wrote before the store
    /// moved into `glidex-raftstore`. The expected values were produced by
    /// that code.
    #[test]
    fn formats_on_disk_are_unchanged() {
        let ws = WriteSet {
            format: WRITE_SET_FORMAT,
            origin: Origin::Controller,
            ops: vec![
                Op::Put {
                    table: TableId::Events,
                    key: b"vm/1".to_vec(),
                    value: vec![0, 255, 7],
                },
                Op::Delete {
                    table: TableId::Audit,
                    key: b"k".to_vec(),
                },
            ],
        };
        assert_eq!(
            serde_json::to_string(&ws).unwrap(),
            r#"{"format":1,"origin":"Controller","ops":[{"Put":{"table":2,"key":"dm0vMQ","value":"AP8H"}},{"Delete":{"table":4,"key":"aw"}}]}"#
        );
        let e: openraft::Entry<crate::cluster::raft::TypeConfig> = openraft::Entry {
            log_id: openraft::LogId::new(openraft::CommittedLeaderId::new(3, 7), 42),
            payload: openraft::EntryPayload::Normal(ws),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(
            json,
            r#"{"log_id":{"leader_id":{"term":3,"node_id":7},"index":42},"payload":{"Normal":{"format":1,"origin":"Controller","ops":[{"Put":{"table":2,"key":"dm0vMQ","value":"AP8H"}},{"Delete":{"table":4,"key":"aw"}}]}}}"#
        );
        let back: openraft::Entry<crate::cluster::raft::TypeConfig> =
            serde_json::from_str(&json).unwrap();
        assert_eq!(back.log_id.index, 42);

        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("g.db")).unwrap();
        db.write(Origin::Api, |tx| -> Result<(), StoreError> {
            tx.open_table(TableId::Vms.definition())?
                .insert("a", b"1")?;
            tx.open_table(TableId::Users.definition())?
                .insert("u", b"xy")?;
            Ok(())
        })
        .unwrap();
        let mut buf = Vec::new();
        db.snapshot_view().unwrap().write_to(&mut buf).unwrap();
        let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "4758534e4150310a0100010000000100000061310b000100000002000000757879ffff"
        );
    }
}
