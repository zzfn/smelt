//! 通用 JSON 配置文件读写：把 appearance/launch_config/llm_config/pet_config 各自手写一遍的
//! 「path → 读（缺失/损坏回退默认）→ 写（失败静默忽略）」样板收口成两个泛型函数。

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::PathBuf;

/// 读取 JSON 配置；文件缺失、内容损坏都回退默认值，不 panic 不报错。
pub fn load_json<T: DeserializeOwned + Default>(path: Option<PathBuf>) -> T {
    path.and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 写回 JSON 配置（失败静默忽略：目录建不出来 / 序列化失败 / 写盘失败都不影响主流程）。
pub fn save_json<T: Serialize>(path: Option<PathBuf>, v: &T) {
    let Some(path) = path else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(v) {
        let _ = std::fs::write(path, json);
    }
}
