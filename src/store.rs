//! 快照存储：`~/.dsh/desk/snapshots/<sid>.json`。
//!
//! 每次 snap 落一个文件；ref 里带 sid，所以旧 ref 不会串到新快照。
//!
//! 安全约束（这几个都不是可选的）：
//! - sid 必须是 `s` + 小写字母数字，挡掉 `@..\..\x:e1` 这类路径穿越和 UNC；
//! - 读取时 `canonicalize` 一次，确认仍在快照目录内；
//! - 文件大小设上限，junction 指向超大文件也读不爆内存；
//! - 写入走"临时文件 + rename"，避免读到半截文件；
//! - 清理只删本工具自己的 `s*.json`。

use crate::error::{DeskError, Result};
use crate::uia::Node;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MAX_SNAPSHOT_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub struct SnapshotFile {
    pub id: String,
    pub window_id: String,
    pub title: String,
    /// 窗口身份：HWND 会被回收，解析 ref 时用 pid+class 复核。
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub class: String,
    pub created_ms: u64,
    pub nodes: Vec<Node>,
    /// 生成这棵树时用的是 RawView 还是 ControlView。解析 ref 必须用同一种。
    #[serde(default)]
    pub raw_view: bool,
}

impl SnapshotFile {
    pub fn find(&self, ref_id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.ref_id == ref_id)
    }
}

pub fn snapshots_dir() -> PathBuf {
    crate::state_dir().join("snapshots")
}

/// sid 只允许 `s` + 1~24 位小写字母数字。
pub fn is_valid_sid(sid: &str) -> bool {
    sid.len() >= 2
        && sid.len() <= 25
        && sid.starts_with('s')
        && sid[1..]
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: AtomicU32 = AtomicU32::new(0);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    // 同一进程内靠 seq 保证唯一；跨进程靠 nanos^pid 的低位 tail 区分。
    let mix =
        (now.subsec_nanos() as u64) ^ ((std::process::id() as u64) << 8) ^ ((seq as u64) << 1);
    let mut n = now.as_millis() as u64;
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
    let mut tail = String::new();
    let mut m = mix;
    for _ in 0..5 {
        let d = (m % 36) as u8;
        tail.push(if d < 10 {
            (b'0' + d) as char
        } else {
            (b'a' + d - 10) as char
        });
        m /= 36;
    }
    format!("s{}{}{:x}", s.chars().rev().collect::<String>(), tail, seq)
}

pub fn save(snap: &SnapshotFile) -> Result<PathBuf> {
    // 写入也校验 id：只有 load 校验过，内部误用带 `..\` 的 id 仍会写穿目录。
    if !is_valid_sid(&snap.id) {
        return Err(DeskError::new(
            "STORE_FAILED",
            format!("快照 id 不合法，拒绝写入：{}", snap.id),
        ));
    }
    let dir = snapshots_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| DeskError::new("STORE_FAILED", format!("创建快照目录失败：{e}")))?;
    let path = dir.join(format!("{}.json", snap.id));
    let text = serde_json::to_string(snap)
        .map_err(|e| DeskError::new("STORE_FAILED", format!("序列化快照失败：{e}")))?;
    // 临时文件 + rename：别的进程不会读到半截 JSON。
    let tmp = dir.join(format!("{}.json.tmp", snap.id));
    std::fs::write(&tmp, text)
        .map_err(|e| DeskError::new("STORE_FAILED", format!("写快照失败：{e}")))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        DeskError::new("STORE_FAILED", format!("提交快照失败：{e}"))
    })?;
    prune(30);
    Ok(path)
}

pub fn load(sid: &str) -> Result<SnapshotFile> {
    if !is_valid_sid(sid) {
        return Err(DeskError::new("BAD_REF", format!("快照 id 不合法：{sid}"))
            .with_suggestion("ref 必须来自 `desk snap` 的输出"));
    }
    let dir = snapshots_dir();
    let path = dir.join(format!("{sid}.json"));
    // 先区分"没这个快照"和"路径有问题"：不存在是常态（被轮转掉了），
    // 路径解析失败才是异常。
    if !path.exists() {
        return Err(
            DeskError::new("SNAPSHOT_NOT_FOUND", format!("快照不存在：{sid}"))
                .with_suggestion("重新运行 `desk snap`；快照只保留最近 30 份"),
        );
    }
    ensure_inside(&dir, &path)?;
    if let Ok(meta) = std::fs::metadata(&path)
        && meta.len() > MAX_SNAPSHOT_BYTES
    {
        return Err(DeskError::new(
            "STORE_FAILED",
            format!("快照文件过大（{} 字节），拒绝读取", meta.len()),
        ));
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(
                DeskError::new("SNAPSHOT_NOT_FOUND", format!("快照不存在：{sid}"))
                    .with_suggestion("重新运行 `desk snap`；快照只保留最近 30 份"),
            );
        }
        Err(e) => {
            return Err(DeskError::new("STORE_FAILED", format!("读取快照失败：{e}")));
        }
    };
    serde_json::from_str(&text)
        .map_err(|e| DeskError::new("STORE_FAILED", format!("快照损坏：{e}")))
}

/// 最近一份属于某个窗口的快照：动作前的基线。
///
/// 一个进程最多留 30 份，逐份解析一次（约几十 KB/份）比让调用方再 snap 一次便宜。
pub fn latest_for_window(window_id: &str) -> Option<SnapshotFile> {
    let dir = snapshots_dir();
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<SnapshotFile> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(sid) = name.strip_suffix(".json") else {
            continue;
        };
        if !is_valid_sid(sid) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(snap) = serde_json::from_str::<SnapshotFile>(&text) else {
            continue;
        };
        if snap.window_id != window_id {
            continue;
        }
        if best
            .as_ref()
            .map(|b| snap.created_ms > b.created_ms)
            .unwrap_or(true)
        {
            best = Some(snap);
        }
    }
    best
}

/// 解析后必须仍落在快照目录内：挡住 `..\..\x`、junction 重定向与 UNC。
///
/// canonicalize 失败按拒绝处理（fail-closed）：路径不存在时 load 会给出
/// SNAPSHOT_NOT_FOUND，不应该被这里放行成"读别的文件"。
fn ensure_inside(dir: &Path, path: &Path) -> Result<()> {
    let dir_canon = std::fs::canonicalize(dir)
        .map_err(|e| DeskError::new("STORE_FAILED", format!("快照目录不可解析：{e}")))?;
    let file_canon = std::fs::canonicalize(path)
        .map_err(|e| DeskError::new("STORE_FAILED", format!("快照路径不可解析：{e}")))?;
    if !file_canon.starts_with(&dir_canon) {
        return Err(DeskError::new(
            "STORE_FAILED",
            "快照路径越出了快照目录，已拒绝读取",
        ));
    }
    Ok(())
}

fn prune(keep: usize) {
    let Ok(entries) = std::fs::read_dir(snapshots_dir()) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            // 只删自己的快照：s + 字母数字 + .json
            name.strip_suffix(".json")
                .map(is_valid_sid)
                .unwrap_or(false)
        })
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
