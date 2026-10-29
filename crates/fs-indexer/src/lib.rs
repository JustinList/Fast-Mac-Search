//! `fms-fs-indexer` — Kernel-speed macOS filesystem crawler (`getattrlistbulk(2)`),
//! real-time `FSEvents` checkpoint watcher, and hardware Volume UUID external-drive manager.

pub mod fsevents;
pub mod macos_bulk;
pub mod volume;

pub use fsevents::{FSEVENT_SINCE_NOW, FsDeltaEvent, FsEventAction, FsEventsWatcher};
pub use macos_bulk::{
    CrawlConfig, CrawlStats, CrawledEntry, EntryKind, crawl_volume_root,
    crawl_volume_root_with_progress, is_artifact_dir_name, is_system_junk_name, read_dir_bulk,
    should_skip_internal_or_noisy_path,
};
pub use volume::{ShardStorageMode, VolumeInfo, format_uuid};

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_getattrlistbulk_matches_real_filesystem_and_gitignore() {
        let mut root = std::env::temp_dir();
        let id: u64 = ((std::process::id() as u64) << 32) ^ 0xA5A5_1234;
        root.push(format!("fms_bulk_test_{id:016x}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();

        fs::write(root.join(".gitignore"), "ignored_secret.log\n").unwrap();
        fs::write(root.join("README.md"), "# Fast Mac Search\nHello world!").unwrap();
        fs::write(root.join("src/engine.rs"), "pub fn run() {}").unwrap();
        fs::write(root.join("ignored_secret.log"), "should be ignored").unwrap();
        fs::write(root.join("._README.md"), "appledouble junk").unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "module.exports = {}").unwrap();

        let (entries, stats) = crawl_volume_root(&root, &CrawlConfig::default()).unwrap();
        assert!(stats.files_scanned >= 3);

        let names: Vec<String> = entries
            .iter()
            .filter_map(|e| e.path.file_name().and_then(|s| s.to_str()).map(String::from))
            .collect();

        assert!(names.contains(&"README.md".to_string()));
        assert!(names.contains(&"engine.rs".to_string()));
        assert!(!names.contains(&"ignored_secret.log".to_string()));
        assert!(!names.contains(&"._README.md".to_string()));
        assert!(!names.contains(&"index.js".to_string()));

        let readme = entries
            .iter()
            .find(|e| e.path.ends_with("README.md"))
            .unwrap();
        assert_eq!(readme.size_bytes, "# Fast Mac Search\nHello world!".len() as u64);
        assert!(readme.inode > 0);
        assert!(readme.mtime_secs > 1_600_000_000);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_volume_uuid_and_fsevents_checkpoint() {
        let vol = VolumeInfo::for_path(std::path::Path::new("/")).unwrap();
        assert_ne!(vol.uuid, [0u8; 16]);
        assert!(!vol.is_external);

        let evt_id = FsEventsWatcher::current_system_event_id();
        assert!(evt_id > 0);
    }
}
