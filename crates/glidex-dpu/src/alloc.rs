//! VF allocation on the DPU's virtio-net PF (spec/31-dpu.md §5.3, §7.1
//! step 2): the lowest free VF, kept in netd's port records.

use crate::DpuError;
use crate::types::VfId;
use std::collections::BTreeMap;

/// The VFs of one PF and who holds each. An owner is a NIC, `<vm>/<nic>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfAllocator {
    pf: u16,
    total: u16,
    used: BTreeMap<u16, String>,
}

impl VfAllocator {
    /// `total` is the PF's VF count (`sriov_totalvfs` on the host).
    pub fn new(pf: u16, total: u16) -> Self {
        Self {
            pf,
            total,
            used: BTreeMap::new(),
        }
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
            .find(|n| !self.used.contains_key(n))
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

    pub fn free_count(&self) -> u16 {
        self.total - self.used.len() as u16
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
