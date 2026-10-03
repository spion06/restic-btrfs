//! Standalone `gc`: reclaim local btrfs snapshots left by past (or crashed) runs.

use anyhow::{Context, Result};

use crate::btrfs::LibBtrfsUtil;
use crate::config::Profile;
use crate::discover;
use crate::select;
use crate::snapshot::{self, GcReport, LocalRetention, StagingArea};

/// Delete local snapshot sets outside `retention`, per subvolume key.
/// `all_keys` also sweeps keys of subvolumes no longer selected by the profile.
pub fn run(profile: &Profile, retention: &LocalRetention, all_keys: bool) -> Result<GcReport> {
    let filesystems = discover::discover()?;
    let resolution = select::resolve(&filesystems, &profile.subvolumes)?;
    let btrfs = LibBtrfsUtil;
    let mut report = GcReport::default();

    for sel in &resolution.selections {
        let area = StagingArea::prepare(sel.fs, profile, &sel.selected)
            .with_context(|| format!("preparing staging for {}", sel.fs.source))?;
        let keys: Vec<String> = sel.selected.iter().map(|s| s.key.clone()).collect();
        let keys = (!all_keys).then_some(keys.as_slice());
        for root in area.gc_roots() {
            report.merge(snapshot::gc(&btrfs, root, keys, retention)?);
        }
    }
    Ok(report)
}
