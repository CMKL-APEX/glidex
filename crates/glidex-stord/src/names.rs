//! What stord accepts as names (spec/storage.md S17): it refuses anything
//! else before running a command, so a caller can only reach glidex's own
//! volumes, in the volume groups the operator allowed.

use crate::proto::StordError;

/// Prefix of a disk's volume: `gx-d-<disk-id>`.
pub const DISK_PREFIX: &str = "gx-d-";
/// Prefix of an image's volume: `gx-i-<image-id>`.
pub const IMAGE_PREFIX: &str = "gx-i-";

/// A plain LVM name: letters, digits, `.`, `_`, `-`, `+`, not starting with
/// `-`, not `.` or `..`, at most 127 bytes (lvm(8) "VALID NAMES").
pub fn lvm_name(kind: &str, s: &str) -> Result<(), StordError> {
    let ok = !s.is_empty()
        && s.len() <= 127
        && !s.starts_with('-')
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'));
    if ok {
        Ok(())
    } else {
        Err(StordError::Invalid(format!(
            "not a valid {kind} name: {s:?}"
        )))
    }
}

/// A glidex volume: `gx-d-…` or `gx-i-…`, then a plain name.
pub fn volume(s: &str) -> Result<(), StordError> {
    lvm_name("volume", s)?;
    let rest = s
        .strip_prefix(DISK_PREFIX)
        .or_else(|| s.strip_prefix(IMAGE_PREFIX))
        .ok_or_else(|| {
            StordError::Invalid(format!(
                "{s:?} is not a glidex volume (gx-d-<disk> or gx-i-<image>)"
            ))
        })?;
    if rest.is_empty() {
        return Err(StordError::Invalid(format!("{s:?} names no disk or image")));
    }
    Ok(())
}

/// An image volume (`gx-i-…`), as a snapshot's origin.
pub fn image_volume(s: &str) -> Result<(), StordError> {
    volume(s)?;
    if !s.starts_with(IMAGE_PREFIX) {
        return Err(StordError::Invalid(format!(
            "{s:?} is not an image volume (gx-i-<image>)"
        )));
    }
    Ok(())
}

/// A VM id, as it appears in `glidex-vm@<id>.service`.
pub fn vm_id(s: &str) -> Result<(), StordError> {
    let ok =
        !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(StordError::Invalid(format!("not a VM id: {s:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_glidex_volumes_pass() {
        assert!(volume("gx-d-3f2a1c9e-0000-4000-8000-000000000001").is_ok());
        assert!(volume("gx-i-ubuntu.24").is_ok());
        for bad in [
            "root",
            "ubuntu-lv",
            "gx-d-",
            "gx-x-1",
            "gx-d-a/b",
            "gx-d-a b",
            "-gx-d-1",
            "gx-d-../x",
            "",
        ] {
            assert!(volume(bad).is_err(), "{bad}");
        }
        assert!(image_volume("gx-i-1").is_ok());
        assert!(image_volume("gx-d-1").is_err());
        assert!(lvm_name("vg", "vg-glidex").is_ok());
        assert!(lvm_name("vg", "vg/x").is_err());
        assert!(lvm_name("vg", "..").is_err());
        assert!(vm_id("1b4e28ba-2fa1-11d2-883f-0016d3cca427").is_ok());
        assert!(vm_id("a/../b").is_err());
        assert!(vm_id("a.service").is_err());
    }
}
