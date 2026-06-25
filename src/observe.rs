//! observe：数字分身的后台采集守护进程。
//! 多路监听用户活动（shell 历史 + 与 AI 的对话），并以定时器兜底捕获
//! 文件改动 / git 提交这类没有单一文件可盯的活动；变化时按节流触发 digest。

use crate::digest;
use anyhow::{Context, Result};
use notify::{RecursiveMode, Watcher};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// 两次蒸馏之间的最小间隔（节流），避免频繁调用 API。
const THROTTLE: Duration = Duration::from_secs(30 * 60);
/// 定时兜底间隔：即便没有文件事件，也每隔这么久蒸馏一次（捕获文件/git 活动）。
const PERIODIC: Duration = Duration::from_secs(2 * 3600);

/// 监听的活动信号源（不存在的会自动跳过）。
fn watch_targets() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    vec![
        home.join(".zsh_history"),       // shell（zsh）
        home.join(".bash_history"),      // shell（bash）
        home.join(".claude/projects"),   // 与 Claude 的对话（递归）
    ]
}

/// 启动监听循环（前台阻塞；由 LaunchAgent 托管为后台守护进程）。
pub async fn run() -> Result<()> {
    // notify 的回调在非 async 上下文，用 unbounded_channel 桥接到 async。
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            let _ = tx.send(ev);
        }
    })
    .context("创建文件监听器失败")?;

    // 逐个挂载监听目标，不存在的跳过（如没装 bash 就没有 .bash_history）。
    let mut watched: Vec<String> = Vec::new();
    for t in watch_targets() {
        let mode = if t.is_dir() {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        if watcher.watch(&t, mode).is_ok() {
            watched.push(t.display().to_string());
        }
    }
    if watched.is_empty() {
        anyhow::bail!("没有可监听的活动源（home 下未找到 shell 历史或 ~/.claude）");
    }

    println!(
        "observe: 正在监听 {} 个活动源（节流 {} 分钟，定时兜底 {} 小时）",
        watched.len(),
        THROTTLE.as_secs() / 60,
        PERIODIC.as_secs() / 3600,
    );
    for w in &watched {
        println!("  - {w}");
    }

    let mut last_digest: Option<Instant> = None;
    let mut periodic = tokio::time::interval(PERIODIC);
    periodic.tick().await; // 第一次立即返回，跳过（避免启动即调 API）。

    loop {
        tokio::select! {
            ev = rx.recv() => {
                if ev.is_none() {
                    break; // 监听器关闭。
                }
                // 去抖：清空积压的连续事件。
                while rx.try_recv().is_ok() {}
                maybe_digest(&mut last_digest, "活动变化").await;
            }
            _ = periodic.tick() => {
                maybe_digest(&mut last_digest, "定时兜底").await;
            }
        }
    }
    Ok(())
}

/// 受节流约束地触发一次蒸馏。
async fn maybe_digest(last_digest: &mut Option<Instant>, reason: &str) {
    let now = Instant::now();
    let due = last_digest.map_or(true, |t| now.duration_since(t) >= THROTTLE);
    if !due {
        return;
    }
    println!("observe: 触发蒸馏（{reason}）");
    match digest::run().await {
        Ok(()) => *last_digest = Some(now),
        Err(e) => eprintln!("digest 失败: {e:#}"),
    }
}
