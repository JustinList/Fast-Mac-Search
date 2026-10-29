use std::ffi::{CStr, CString};
use std::fmt;
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Storage strategy for a volume's encrypted index shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShardStorageMode {
    /// Default: Encrypted shard lives on internal NVMe (`~/Library/Application Support/FastMacSearch/shards/<UUID>`).
    /// Allows searching external drives even while unplugged ("Offline - Connect Drive X").
    HostCached,
    /// Portable: Encrypted shard lives directly on the external drive (`<MountPoint>/.fastindex/`).
    /// Instantly mountable on another Mac running Fast Mac Search without re-indexing.
    Portable,
}

/// Hardware and mount metadata for a macOS APFS / HFS+ / ExFAT volume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeInfo {
    /// 128-bit hardware filesystem Volume UUID (`ATTR_VOL_UUID`, or deterministic mount fallback).
    pub uuid: [u8; 16],
    /// Formatted UUID string (`XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`).
    pub uuid_string: String,
    /// Human-readable volume name (e.g., `"Macintosh HD"` or `"Crucial X9"`).
    pub name: String,
    /// Canonical mount point (e.g., `"/"` or `"/Volumes/Crucial X9"`).
    pub mount_point: PathBuf,
    /// Underlying filesystem type (e.g., `"apfs"`, `"exfat"`, `"hfs"`).
    pub fs_type: String,
    /// True if mounted as an external / detachable volume.
    pub is_external: bool,
    /// True if the volume's mount point is currently online and accessible.
    pub is_online: bool,
}

impl VolumeInfo {
    /// Queries macOS kernel `statfs(2)` and `getattrlist(2)` (`ATTR_VOL_UUID`) for the volume
    /// containing `target_path`.
    pub fn for_path(target_path: &Path) -> io::Result<Self> {
        let canonical = target_path
            .canonicalize()
            .unwrap_or_else(|_| target_path.to_path_buf());

        #[cfg(target_os = "macos")]
        {
            macos_vol::query_volume_info(&canonical)
        }

        #[cfg(not(target_os = "macos"))]
        {
            let hash = blake3::hash(canonical.as_os_str().as_bytes());
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(&hash.as_bytes()[..16]);
            Ok(Self {
                uuid,
                uuid_string: format_uuid(&uuid),
                name: "LocalVolume".into(),
                mount_point: PathBuf::from("/"),
                fs_type: "posix".into(),
                is_external: false,
                is_online: true,
            })
        }
    }

    /// Resolves the directory where this volume's encrypted shard should be stored.
    pub fn shard_directory(&self, mode: ShardStorageMode, host_base_dir: &Path) -> PathBuf {
        match mode {
            ShardStorageMode::HostCached => host_base_dir.join("shards").join(&self.uuid_string),
            ShardStorageMode::Portable => self.mount_point.join(".fastindex"),
        }
    }
}

impl fmt::Display for VolumeInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}, {}, uuid={})",
            self.name,
            self.fs_type,
            if self.is_external { "external" } else { "internal" },
            self.uuid_string
        )
    }
}

pub fn format_uuid(u: &[u8; 16]) -> String {
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        u[0], u[1], u[2], u[3], u[4], u[5], u[6], u[7],
        u[8], u[9], u[10], u[11], u[12], u[13], u[14], u[15]
    )
}

#[cfg(target_os = "macos")]
mod macos_vol {
    use super::*;

    const ATTR_BIT_MAP_COUNT: u16 = 5;
    const ATTR_VOL_INFO: u32 = 0x8000_0000;
    const ATTR_VOL_UUID: u32 = 0x0004_0000;
    const MNT_REMOVABLE: u32 = 0x0000_0200;

    #[repr(C)]
    struct AttrList {
        bitmapcount: u16,
        reserved: u16,
        commonattr: u32,
        volattr: u32,
        dirattr: u32,
        fileattr: u32,
        forkattr: u32,
    }

    #[repr(C)]
    struct VolUuidAttrBuf {
        length: u32,
        uuid: [u8; 16],
    }

    unsafe extern "C" {
        fn getattrlist(
            path: *const libc::c_char,
            attr_list: *mut AttrList,
            attr_buf: *mut libc::c_void,
            attr_buf_size: libc::size_t,
            options: libc::c_ulong,
        ) -> libc::c_int;
    }

    pub fn query_volume_info(path: &Path) -> io::Result<VolumeInfo> {
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "nul byte in path"))?;

        let mut st = MaybeUninit::<libc::statfs>::zeroed();
        let rc = unsafe { libc::statfs(c_path.as_ptr(), st.as_mut_ptr()) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        let st = unsafe { st.assume_init() };

        let mount_str = unsafe { CStr::from_ptr(st.f_mntonname.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let fs_type = unsafe { CStr::from_ptr(st.f_fstypename.as_ptr()) }
            .to_string_lossy()
            .into_owned();

        let mount_point = PathBuf::from(&mount_str);
        let is_external = (st.f_flags & MNT_REMOVABLE) != 0
            || (mount_str.starts_with("/Volumes/") && mount_str != "/Volumes/Macintosh HD");

        let name = if mount_str == "/" {
            "Macintosh HD".to_string()
        } else {
            mount_point
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("External Volume")
                .to_string()
        };

        // Query hardware Volume UUID on the mount root via getattrlist(ATTR_VOL_INFO | ATTR_VOL_UUID)
        let c_mount = CString::new(mount_point.as_os_str().as_bytes())
            .unwrap_or_else(|_| c_path.clone());
        let mut alist = AttrList {
            bitmapcount: ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: 0,
            volattr: ATTR_VOL_INFO | ATTR_VOL_UUID,
            dirattr: 0,
            fileattr: 0,
            forkattr: 0,
        };
        let mut vbuf = VolUuidAttrBuf {
            length: 0,
            uuid: [0u8; 16],
        };
        let attr_rc = unsafe {
            getattrlist(
                c_mount.as_ptr(),
                &mut alist,
                (&raw mut vbuf).cast(),
                std::mem::size_of::<VolUuidAttrBuf>(),
                0,
            )
        };

        let uuid = if attr_rc == 0 && vbuf.uuid != [0u8; 16] {
            vbuf.uuid
        } else {
            // Deterministic fallback from f_fsid + mount point
            let mut h = blake3::Hasher::new();
            let fsid_bytes = unsafe {
                std::slice::from_raw_parts(
                    (&raw const st.f_fsid).cast::<u8>(),
                    std::mem::size_of::<libc::fsid_t>(),
                )
            };
            h.update(fsid_bytes);
            h.update(mount_str.as_bytes());
            let mut fallback = [0u8; 16];
            fallback.copy_from_slice(&h.finalize().as_bytes()[..16]);
            fallback
        };

        Ok(VolumeInfo {
            uuid,
            uuid_string: format_uuid(&uuid),
            name,
            mount_point: mount_point.clone(),
            fs_type,
            is_external,
            is_online: mount_point.exists(),
        })
    }
}
