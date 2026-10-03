//! Minimal `/proc/self/mountinfo` parser.
//!
//! Format (one line per mount), see `proc(5)`:
//!
//! ```text
//! 36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
//! (1)(2)(3)   (4)   (5)      (6)      (7)   (8)(9)   (10)         (11)
//! ```
//!
//! Fields 7 are zero-or-more optional tags terminated by field 8, the literal `-`.

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct MountInfoEntry {
    pub mount_id: u32,
    pub parent_id: u32,
    /// `major:minor` of the device backing this mount. For btrfs, all subvolume
    /// mounts of one filesystem share this value on current kernels, which makes
    /// it a usable filesystem key.
    pub dev: String,
    /// Path of the mount root within the filesystem (field 4), e.g. `/@home`.
    pub root: PathBuf,
    /// Mount point on the host (field 5).
    pub mount_point: PathBuf,
    /// Per-mount options (field 6).
    pub mount_options: String,
    pub fs_type: String,
    /// Mount source (field 10), e.g. `/dev/nvme0n1p2`.
    pub source: String,
    /// Superblock options (field 11), e.g. `...,subvolid=257,subvol=/@home`.
    pub super_options: String,
}

impl MountInfoEntry {
    /// Value of a `key=value` pair in the superblock options, if present.
    pub fn super_opt(&self, key: &str) -> Option<&str> {
        opt_value(&self.super_options, key)
    }

    /// `subvol=` from superblock options (btrfs), unescaped and normalized to
    /// start with `/`.
    pub fn btrfs_subvol(&self) -> Option<String> {
        let raw = self.super_opt("subvol")?;
        let s = unescape(raw).to_string_lossy().into_owned();
        Some(if s.starts_with('/') { s } else { format!("/{s}") })
    }

    /// `subvolid=` from superblock options (btrfs).
    pub fn btrfs_subvolid(&self) -> Option<u64> {
        self.super_opt("subvolid").and_then(|s| s.parse().ok())
    }
}

fn opt_value<'a>(opts: &'a str, key: &str) -> Option<&'a str> {
    opts.split(',').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// Unescape the octal escapes the kernel applies to path fields (`\040` space,
/// `\011` tab, `\012` newline, `\134` backslash).
fn unescape(field: &str) -> PathBuf {
    if !field.contains('\\') {
        return PathBuf::from(field);
    }
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            if let Ok(s) = std::str::from_utf8(&bytes[i + 1..i + 4]) {
                if let Ok(n) = u8::from_str_radix(s, 8) {
                    out.push(n);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

/// Parse mountinfo text into entries. Malformed lines are skipped.
pub fn parse(text: &str) -> Vec<MountInfoEntry> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<MountInfoEntry> {
    let (pre, post) = line.split_once(" - ")?;
    let pre: Vec<&str> = pre.split_whitespace().collect();
    let post: Vec<&str> = post.split_whitespace().collect();
    if pre.len() < 6 || post.len() < 3 {
        return None;
    }
    Some(MountInfoEntry {
        mount_id: pre[0].parse().ok()?,
        parent_id: pre[1].parse().ok()?,
        dev: pre[2].to_string(),
        root: unescape(pre[3]),
        mount_point: unescape(pre[4]),
        mount_options: pre[5].to_string(),
        fs_type: post[0].to_string(),
        source: post[1].to_string(),
        super_options: post[2].to_string(),
    })
}

/// Read and parse `/proc/self/mountinfo`.
pub fn read() -> std::io::Result<Vec<MountInfoEntry>> {
    Ok(parse(&std::fs::read_to_string("/proc/self/mountinfo")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
40 1 0:34 /@ / rw,noatime shared:1 - btrfs /dev/nvme0n1p2 rw,compress=zstd:1,ssd,subvolid=256,subvol=/@
57 40 0:34 /@srv /srv rw,noatime shared:160 - btrfs /dev/nvme0n1p2 rw,ssd,subvolid=259,subvol=/@srv
99 40 0:34 /@home /home rw,noatime shared:2 - btrfs /dev/nvme0n1p2 rw,ssd,subvolid=257,subvol=/@home
1 2 8:1 / /boot rw - ext4 /dev/sda1 rw";

    #[test]
    fn parses_btrfs_rows() {
        let e = parse(SAMPLE);
        assert_eq!(e.len(), 4);
        let home = e.iter().find(|m| m.mount_point.as_path() == std::path::Path::new("/home")).unwrap();
        assert_eq!(home.fs_type, "btrfs");
        assert_eq!(home.dev, "0:34");
        assert_eq!(home.btrfs_subvol().as_deref(), Some("/@home"));
        assert_eq!(home.btrfs_subvolid(), Some(257));
        assert_eq!(home.source, "/dev/nvme0n1p2");
    }

    #[test]
    fn unescapes_spaces() {
        let line = "42 40 0:34 /@d /mnt/a\\040b rw - btrfs /dev/x rw,subvol=/@d";
        let e = parse(line);
        assert_eq!(e[0].mount_point, PathBuf::from("/mnt/a b"));
    }
}
