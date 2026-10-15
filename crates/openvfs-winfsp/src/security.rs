//! A tiny, permissive, self-relative security descriptor.
//!
//! `openvfs` backends do not model users or ACLs, so every node reports the
//! same descriptor — SDDL equivalent `O:BAG:BAD:P(A;;FA;;;WD)`:
//! owner and group = `BUILTIN\Administrators`, DACL = `Everyone: Full Control`.
//! That matches what most WinFsp file systems ship to Windows when the volume
//! doesn't really have NT security.
//!
//! The bytes are assembled by hand instead of calling
//! `ConvertStringSecurityDescriptorToSecurityDescriptorW`, which keeps the
//! crate free of an advapi32 dependency *and* makes the layout testable on any
//! platform. Only used by `GetSecurity`/`GetSecurityByName` when a caller
//! insists on a descriptor; the access-check path normally short-circuits on
//! an empty descriptor (see `host.rs`).

// Self-relative SECURITY_DESCRIPTOR header (winnt.h):
//   Revision:u8=1, Sbz1:u8=0, Control:u16, then four little-endian u32 offsets.
const SE_DACL_PRESENT: u16 = 0x0004;
const SE_SELF_RELATIVE: u16 = 0x8000;

const ACL_REVISION: u8 = 2;
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

const FILE_ALL_ACCESS: u32 = 0x001F_01FF;

/// Encodes a SID: `S-1-<id_auth>-<sub0>-<sub1>...`.
fn sid(id_authority: u8, sub_authorities: &[u32]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + 4 * sub_authorities.len());
    v.push(1); // revision
    v.push(sub_authorities.len() as u8);
    v.extend_from_slice(&[0, 0, 0, 0, 0, id_authority]); // 48-bit big-endian identifier authority
    for sa in sub_authorities {
        v.extend_from_slice(&sa.to_le_bytes());
    }
    v
}

/// Builds the self-relative security descriptor bytes.
///
/// Layout (all multi-byte fields little-endian):
/// - 20-byte `SECURITY_DESCRIPTOR` header,
/// - owner SID `S-1-5-32-544` (`BUILTIN\Administrators`),
/// - group SID (same),
/// - DACL containing one `ACCESS_ALLOWED` ACE: `Everyone` (`S-1-1-0`) /
///   `FILE_ALL_ACCESS`.
#[must_use]
pub fn permissive_security_descriptor() -> Vec<u8> {
    let owner = sid(5, &[32, 544]); // SECURITY_NT_AUTHORITY / Administrators
    let group = sid(5, &[32, 544]);
    let everyone = sid(1, &[0]); // SECURITY_WORLD_SID_AUTHORITY

    // ACL { revision, sbz1, size:u16, ace_count:u16, sbz2:u16 } + one ACE.
    // ACE = ACE_HEADER(type:u8, flags:u8, size:u16) + mask:u32 + SID.
    let ace_size = 8 + everyone.len() as u16;
    let acl_size = 8 + ace_size;
    let mut dacl = Vec::with_capacity(acl_size as usize);
    dacl.push(ACL_REVISION);
    dacl.push(0);
    dacl.extend_from_slice(&acl_size.to_le_bytes());
    dacl.extend_from_slice(&1u16.to_le_bytes());
    dacl.extend_from_slice(&0u16.to_le_bytes());
    // ACCESS_ALLOWED_ACE { header(type,flags,size:u16), mask:u32, sid }
    dacl.push(ACCESS_ALLOWED_ACE_TYPE);
    dacl.push(0); // ace flags
    dacl.extend_from_slice(&ace_size.to_le_bytes());
    dacl.extend_from_slice(&FILE_ALL_ACCESS.to_le_bytes());
    dacl.extend_from_slice(&everyone);

    let owner_off = 20u32;
    let group_off = owner_off + owner.len() as u32;
    let dacl_off = group_off + group.len() as u32;

    let mut sd = Vec::with_capacity(dacl_off as usize + dacl.len());
    sd.push(1); // SECURITY_DESCRIPTOR_REVISION
    sd.push(0); // Sbz1
    sd.extend_from_slice(&(SE_DACL_PRESENT | SE_SELF_RELATIVE).to_le_bytes());
    sd.extend_from_slice(&owner_off.to_le_bytes());
    sd.extend_from_slice(&group_off.to_le_bytes());
    sd.extend_from_slice(&0u32.to_le_bytes()); // no SACL
    sd.extend_from_slice(&dacl_off.to_le_bytes());
    sd.extend_from_slice(&owner);
    sd.extend_from_slice(&group);
    sd.extend_from_slice(&dacl);
    sd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16le(b: &[u8], off: usize) -> u16 {
        u16::from_le_bytes([b[off], b[off + 1]])
    }
    fn u32le(b: &[u8], off: usize) -> u32 {
        u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
    }

    #[test]
    fn descriptor_layout_is_well_formed() {
        let sd = permissive_security_descriptor();
        assert_eq!(sd.len(), 80);
        assert_eq!(sd[0], 1); // revision
        assert_eq!(u16le(&sd, 2), SE_DACL_PRESENT | SE_SELF_RELATIVE);
        assert_eq!(u32le(&sd, 4), 20); // owner offset
        assert_eq!(u32le(&sd, 8), 36); // group offset
        assert_eq!(u32le(&sd, 12), 0); // sacl
        assert_eq!(u32le(&sd, 16), 52); // dacl offset

        // Owner SID S-1-5-32-544 at offset 20 (16 bytes).
        let owner = &sd[20..36];
        assert_eq!(owner[..8], [1, 2, 0, 0, 0, 0, 0, 5]);
        assert_eq!(u32le(owner, 8), 32);
        assert_eq!(u32le(owner, 12), 544);

        // DACL at 52: acl size 28, one ace.
        assert_eq!(u16le(&sd, 54), 28);
        assert_eq!(u16le(&sd, 56), 1);
        // ACE at 60: type=ACCESS_ALLOWED, size=20, mask=FILE_ALL_ACCESS, sid S-1-1-0.
        assert_eq!(sd[60], 0);
        assert_eq!(u16le(&sd, 62), 20);
        assert_eq!(u32le(&sd, 64), FILE_ALL_ACCESS);
        // SID: rev=1, subauth count=1, id authority {0..0,1}, subauth[0]=0.
        assert_eq!(&sd[68..76], &[1, 1, 0, 0, 0, 0, 0, 1]);
        assert_eq!(u32le(&sd, 76), 0);
    }
}
