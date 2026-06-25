//! smelt：Mac 上的个人知识蒸馏引擎。

mod db;
mod digest;
mod install;
mod model;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "smelt", about = "个人知识蒸馏引擎", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 启动后台采集守护进程
    Observe,
    /// 手动触发一次蒸馏
    Digest,
    /// 打印当前 instincts
    Show,
    /// 注册 Mac LaunchAgent 开机自启
    Install,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Observe => {
            println!("observe：守护进程尚未实现（第二阶段）。");
        }
        Command::Digest => {
            digest::run().await?;
        }
        Command::Show => {
            show()?;
        }
        Command::Install => {
            install::run()?;
        }
    }
    Ok(())
}

/// 从 DB 读出 instinct，按 confidence 排序打印。
fn show() -> Result<()> {
    let conn = db::open()?;
    let items = db::list_by_confidence(&conn)?;
    if items.is_empty() {
        println!("还没有 instinct。先跑 `smelt digest`。");
        return Ok(());
    }
    for it in items {
        println!(
            "[{:.2}] ({}) {}  <{}> x{}",
            it.confidence,
            it.scope.as_str(),
            it.content,
            it.domain.join(","),
            it.evidence_count
        );
    }
    Ok(())
}
