//! smelt：Mac 上的个人知识蒸馏引擎。

mod db;
mod digest;
mod install;
mod merge;
mod model;
mod observe;
mod render;

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
    /// 对已有 instincts 做语义去重合并
    Merge,
    /// 打印当前 instincts
    Show,
    /// 注册 Mac LaunchAgent 开机自启
    Install,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Observe => observe::run().await?,
        Command::Digest => digest::run().await?,
        Command::Merge => merge::run().await?,
        Command::Show => show()?,
        Command::Install => install::run()?,
    }
    Ok(())
}

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
