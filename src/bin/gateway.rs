//! smelt 远程操作网关——独立进程版（见 docs/remote-ops-roadmap.md）。
//!
//! 实际的路由/handler/HTML 都在 `src/remote_gateway.rs`（这个文件和 smeltd 内嵌的
//! `remote_start` op 共用同一份，见那边的模块注释）。这个文件只负责命令行启动：
//! 解析 `--bind`/`--port`、生成 token、绑端口、打印分享链接。
//!
//! 用法：
//!   gateway [--bind 127.0.0.1] [--port 0] [--write]
//! 默认绑回环地址，不监听 `0.0.0.0`；跨机器访问交给用户自己的网
//! （Tailscale/SSH 隧道），网关自己不做中继、不做公网暴露。`--write` 开启后
//! 这条链接能 `input`（原始键盘）+ approve/deny/reply（见 smeltd「远程操控」），
//! 链接本身就是授权，不再额外要求当面确认。

#[path = "../remote_gateway.rs"]
mod remote_gateway;

use std::net::{IpAddr, Ipv4Addr};

fn parse_args() -> (IpAddr, u16, bool) {
    let mut bind_ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let mut port: u16 = 0;
    let mut write = false;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bind" => {
                if let Some(v) = args.next() {
                    match v.parse() {
                        Ok(ip) => bind_ip = ip,
                        Err(_) => eprintln!("非法 --bind 地址：{v}，回退到 127.0.0.1"),
                    }
                }
            }
            "--port" => {
                if let Some(v) = args.next() {
                    port = v.parse().unwrap_or(0);
                }
            }
            "--write" => write = true,
            _ => {}
        }
    }
    (bind_ip, port, write)
}

#[tokio::main]
async fn main() {
    let (bind_ip, port, write) = parse_args();

    // 128 位随机 token，一次性打印在 stdout；不落盘、不设过期。
    let token = uuid::Uuid::new_v4().simple().to_string();
    let app = remote_gateway::build_router(token.clone(), write);

    let listener = match tokio::net::TcpListener::bind((bind_ip, port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("绑定 {bind_ip}:{port} 失败：{e}");
            std::process::exit(1);
        }
    };
    let addr = listener.local_addr().unwrap();

    println!(
        "smelt 远程操作网关（{}）",
        if write { "可写：input + approve/deny/reply" } else { "只读观战" }
    );
    println!("绑定：{addr}（默认只回环，不监听 0.0.0.0；跨机器访问用你自己的网：Tailscale / SSH 隧道）");
    println!("分享链接（会话列表）：http://{addr}/?token={token}");

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("网关退出：{e}");
    }
}
