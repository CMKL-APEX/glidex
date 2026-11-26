//! VF allocation on the DPU's virtio-net PF (spec/31-dpu.md §5.3, §7.1
//! step 2): the lowest free VF, kept in netd's port records.

use crate::DpuError;
use crate::types::VfId;
use std::collections::{BTreeMap, BTreeSet};

/// The VFs of one PF and who holds each. An owner is a NIC, `<vm>/<nic>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfAllocator {
    pf: u16,
    total: u16,
    used: BTreeMap<u16, String>,
    /// Each VF's IOMMU group, where known.
    groups: BTreeMap<u16, String>,
}

/// The VM of an owner `<vm>/<nic>`.
fn vm_of(owner: &str) -> &str {
    owner.split_once('/').map_or(owner, |(vm, _)| vm)
}

impl VfAllocator {
    /// `total` is the PF's VF count (`sriov_totalvfs` on the host).
    pub fn new(pf: u16, total: u16) -> Self {
        Self {
            pf,
            total,
            used: BTreeMap::new(),
            groups: BTreeMap::new(),
        }
    }

    /// The VFs' IOMMU groups (§5.3). VFIO opens a whole group, so VFs that
    /// share one are a single unit: one VM may hold any of them, another
    /// none. A VF whose group isn't known is taken to have its own.
    pub fn with_groups(mut self, groups: impl IntoIterator<Item = (u16, String)>) -> Self {
        self.groups = groups.into_iter().collect();
        self
    }

    /// Whether some VFs share an IOMMU group: VFs created while ACS was off
    /// on their root port (finding 41).
    pub fn groups_shared(&self) -> bool {
        let distinct: BTreeSet<&String> = self.groups.values().collect();
        distinct.len() < self.groups.len()
    }

    /// Whether `owner` can take VF `n`: free, and no other VM holds a VF
    /// of its group.
    fn usable_by(&self, n: u16, owner: &str) -> bool {
        if self.used.contains_key(&n) {
            return false;
        }
        let Some(group) = self.groups.get(&n) else {
            return true;
        };
        self.used
            .iter()
            .all(|(m, o)| self.groups.get(m) != Some(group) || vm_of(o) == vm_of(owner))
    }

    /// Rebuild from the records (`dpu_vf` of each port), at netd start.
    /// A record beyond `total` or a VF held twice is an error: the records
    /// and the hardware disagree and nothing should be guessed.
    pub fn from_records<'a>(
        pf: u16,
        total: u16,
        records: impl IntoIterator<Item = (VfId, &'a str)>,
    ) -> Result<Self, DpuError> {
        let mut a = Self::new(pf, total);
        for (id, owner) in records {
            if id.pf != pf || id.vf >= total {
                return Err(DpuError::invalid(format!(
                    "{owner} holds VF {}/{}, outside PF {pf}'s {total}",
                    id.pf, id.vf
                )));
            }
            if let Some(other) = a.used.insert(id.vf, owner.to_string())
                && other != owner
            {
                return Err(DpuError::invalid(format!(
                    "VF {}/{} is held by both {other} and {owner}",
                    id.pf, id.vf
                )));
            }
        }
        Ok(a)
    }

    /// The VF `owner` holds, or the lowest free one. Idempotent: a retried
    /// attach gets the VF it already has.
    pub fn allocate(&mut self, owner: &str) -> Result<VfId, DpuError> {
        if let Some(vf) = self.held_by(owner) {
            return Ok(vf);
        }
        let vf = (0..self.total)
            .find(|n| self.usable_by(*n, owner))
            .ok_or(DpuError::DpuCapacity { total: self.total })?;
        self.used.insert(vf, owner.to_string());
        Ok(VfId { pf: self.pf, vf })
    }

    pub fn held_by(&self, owner: &str) -> Option<VfId> {
        self.used
            .iter()
            .find(|(_, o)| o.as_str() == owner)
            .map(|(vf, _)| VfId {
                pf: self.pf,
                vf: *vf,
            })
    }

    /// Free `owner`'s VF; returns it. Freeing nothing is fine (§7.2).
    pub fn free(&mut self, owner: &str) -> Option<VfId> {
        let vf = self.held_by(owner)?;
        self.used.remove(&vf.vf);
        Some(vf)
    }

    /// The VFs `owner` could take now: free ones of groups no other VM
    /// holds.
    pub fn free_for(&self, owner: &str) -> u16 {
        (0..self.total)
            .filter(|n| self.usable_by(*n, owner))
            .count() as u16
    }

    /// The VFs a new VM can take, one per IOMMU group with no VF held.
    pub fn free_count(&self) -> u16 {
        let held: BTreeSet<&String> = self
            .used
            .keys()
            .filter_map(|n| self.groups.get(n))
            .collect();
        let mut seen = BTreeSet::new();
        (0..self.total)
            .filter(|n| !self.used.contains_key(n))
            .filter(|n| match self.groups.get(n) {
                None => true,
                Some(g) => !held.contains(g) && seen.insert(g),
            })
            .count() as u16
    }

    pub fn total(&self) -> u16 {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowest_free_first_and_reused_after_free() {
        let mut a = VfAllocator::new(0, 3);
        assert_eq!(a.allocate("vm1/0").unwrap(), VfId { pf: 0, vf: 0 });
        assert_eq!(a.allocate("vm1/1").unwrap(), VfId { pf: 0, vf: 1 });
        assert_eq!(a.allocate("vm2/0").unwrap(), VfId { pf: 0, vf: 2 });
        assert_eq!(a.free_count(), 0);
        assert_eq!(
            a.allocate("vm3/0").unwrap_err(),
            DpuError::DpuCapacity { total: 3 }
        );
        // A retry gets the same VF, not a new one.
        assert_eq!(a.allocate("vm1/1").unwrap().vf, 1);

        assert_eq!(a.free("vm1/0"), Some(VfId { pf: 0, vf: 0 }));
        assert_eq!(a.free("vm1/0"), None);
        assert_eq!(a.allocate("vm3/0").unwrap().vf, 0);
    }

    #[test]
    fn vfs_sharing_an_iommu_group_go_to_one_vm() {
        // VFs 0-2 share group 7 (created while ACS was off); VF 3 is alone.
        let groups = [(0, "7"), (1, "7"), (2, "7"), (3, "8")].map(|(n, g)| (n, g.to_string()));
        let mut a = VfAllocator::new(0, 4).with_groups(groups.clone());
        assert!(a.groups_shared());
        assert_eq!(a.free_count(), 2, "one per group");
        assert_eq!(a.allocate("vm1/0").unwrap().vf, 0);
        assert_eq!((a.free_for("vm1/1"), a.free_for("vm2/0")), (3, 1));
        // The same VM may take another VF of its group; another VM gets
        // the next group's.
        assert_eq!(a.allocate("vm1/1").unwrap().vf, 1);
        assert_eq!(a.free_count(), 1);
        assert_eq!(a.allocate("vm2/0").unwrap().vf, 3);
        assert_eq!(a.free_count(), 0);
        assert_eq!(
            a.allocate("vm3/0").unwrap_err(),
            DpuError::DpuCapacity { total: 4 }
        );
        // Once vm1 lets go of both, group 7 is free again.
        a.free("vm1/0");
        assert_eq!(a.free_count(), 0, "vm1 still holds VF 1");
        a.free("vm1/1");
        assert_eq!(a.free_count(), 1);
        assert_eq!(a.allocate("vm3/0").unwrap().vf, 0);

        // A group each: as without groups.
        let own = (0..4).map(|n| (n, format!("g{n}")));
        let a = VfAllocator::new(0, 4).with_groups(own);
        assert!(!a.groups_shared());
        assert_eq!(a.free_count(), 4);
    }

    #[test]
    fn rebuilt_from_records_and_disagreement_refused() {
        let a = VfAllocator::from_records(
            0,
            16,
            [
                (VfId { pf: 0, vf: 2 }, "vm1/0"),
                (VfId { pf: 0, vf: 0 }, "vm2/0"),
            ],
        )
        .unwrap();
        assert_eq!(a.held_by("vm1/0"), Some(VfId { pf: 0, vf: 2 }));
        assert_eq!(a.free_count(), 14);
        let mut a = a;
        assert_eq!(a.allocate("vm3/0").unwrap().vf, 1);

        assert!(VfAllocator::from_records(0, 2, [(VfId { pf: 0, vf: 2 }, "x")]).is_err());
        assert!(VfAllocator::from_records(0, 2, [(VfId { pf: 1, vf: 0 }, "x")]).is_err());
        assert!(
            VfAllocator::from_records(
                0,
                2,
                [(VfId { pf: 0, vf: 0 }, "x"), (VfId { pf: 0, vf: 0 }, "y")]
            )
            .is_err()
        );
    }
}
