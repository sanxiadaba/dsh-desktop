//! 快照存储：`~/.dsh/desk/snapshots/<sid>.json`。
//!
//! 每次 snap 落一个文件；ref 里带 sid，所以旧 ref 不会串到新快照。

use crate::error::{DeskError, Result};
use crate::uia::Node;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
pub struct SnapshotFile {
    pub id: String,
    pub window_id: String,
    pub title: String,
    pub created_ms: u64,
    pub nodes: Vec<Node>,
}

impl SnapshotFile {
    pub fn find(&self, ref_id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.ref_id == ref_id)
    }
}

pub fn snapshots_dir() -> PathBuf {
    crate::state_dir().join("snapshots")
}

pub fn new_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    // 36 进制，短且单调
    let mut n = ms;
    let mut s = String::new();
    while n > 0 {
        let d = (n % 36) as u8;
        s.push(if d < 10 {
            (b'0' + d) as char
        } else {
            (b'a' + d - 10) as char
        });
        n /= 36;
    }
    format!("s{}", s.chars().rev().collect::<String>())
}

pub fn save(snap: &SnapshotFile) -> Result<PathBuf> {
    let dir = snapshots_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| DeskError::new("STORE_FAILED", format!("创建快照目录失败：{e}")))?;
    let path = dir.join(format!("{}.json", snap.id));
    let text = serde_json::to_string(snap)
        .map_err(|e| DeskError::new("STORE_FAILED", format!("序列化快照失败：{e}")))?;
    std::fs::write(&path, text)
        .map_err(|e| DeskError::new("STORE_FAILED", format!("写快照失败：{e}")))?;
    // 顺手清理 30 个之前的老快照，避免目录无限增长。
    prune(30);
    Ok(path)
}

pub fn load(sid: &str) -> Result<SnapshotFile> {
    let path = snapshots_dir().join(format!("{sid}.json"));
    let text = std::fs::read_to_string(&path).map_err(|_| {
        DeskError::new("SNAPSHOT_NOT_FOUND", format!("快照不存在：{sid}"))
            .with_suggestion("重新运行 `desk snap`")
    })?;
    serde_json::from_str(&text)
        .map_err(|e| DeskError::new("STORE_FAILED", format!("快照损坏：{e}")))
}

fn prune(keep: usize) {
    let Ok(entries) = std::fs::read_dir(snapshots_dir()) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let t = e.metadata().ok()?.modified().ok()?;
            Some((t, e.path()))
        })
        .collect();
    if files.len() <= keep {
        return;
    }
    files.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    for (_, p) in files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(p);
    }
}
