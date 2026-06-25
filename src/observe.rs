//! observe：监听 ~/.zsh_history 变化，按节流间隔触发 digest（digest 内部会更新 global.md）。

use crate::digest;
use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};
use std::time::{Duration, Instant};

/// 两次蒸馏之间的最小间隔（节流），避免频繁调用 API。
const THROTTLE: Duration = Duration::from_secs(30 * 60);

/// 启动监听循环（前台阻塞；由 LaunchAgent 托管为后台守护进程）。
pub async fn run() -> Result<()> {
    let hist = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("无法定位 home 目录"))?
        .join(".zsh_history");

    // notify 的回调在非 async 上下文，用 unbounded_channel 的同步 send 桥接到 async。
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            let _ = tx.send(ev);
        }
    })
    .context("创建文件监听器失败")?;
    watcher
        .watch(&hist, RecursiveMode::NonRecursive)
        .with_context(|| format!("监听 {:?} 失败", hist))?;

    println!(
        "observe: 正在监听 {:?}（节流 {} 分钟）",
        hist,
        THROTTLE.as_secs() / 60
    );

    let mut last_digest: Option<Instant> = None;
    while let Some(_ev) = rx.recv().await {
        // 去抖：清空积压的连续事件。
        while rx.try_recv().is_ok() {}

        let now = Instant::now();
        let due = last_digest.map_or(true, |t| now.duration_since(t) >= THROTTLE);
        if !due {
            continue;
        }
        match digest::run().await {
            Ok(()) => last_digest = Some(now),
            Err(e) => eprintln!("digest 失败: {e:#}"),
        }
    }
    Ok(())
}
