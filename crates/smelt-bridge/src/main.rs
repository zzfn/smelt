//! smelt-bridge：跨网 host。
//!
//! 1. `POST {signal_http}/v1/rooms` 建房  
//! 2. WebSocket `hello` role=host  
//! 3. 收 client offer → answer + ICE（webrtc-rs）  
//! 4. DataChannel `smelt` 上跑业务帧，转发到本机 remote_gateway  
//!
//! 环境变量见 `--help` 输出 / docs/webrtc-edge.md。

mod dc;
mod gateway;
mod rtc;
mod signal;

use std::env;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
pub struct Config {
    /// 用户自部署的信令 HTTP 根，如 https://signal.example.com（必填，无默认）
    pub signal_http: String,
    /// 如 wss://signal.example.com/ws（空则从 signal_http 推导）
    pub signal_ws: String,
    /// 本机网关，如 http://127.0.0.1:18765
    pub gateway_base: String,
    /// 网关 token
    pub gateway_token: String,
    /// 写权限（与网关 --write 一致）
    pub write: bool,
    /// 分享页前缀（可选），打印完整跨网 URL
    pub share_base: Option<String>,
}

impl Config {
    fn from_env() -> Result<Self> {
        let signal_http = env::var("SMELT_SIGNAL_HTTP")
            .unwrap_or_default()
            .trim()
            .trim_end_matches('/')
            .to_string();
        if signal_http.is_empty()
            || !(signal_http.starts_with("https://") || signal_http.starts_with("http://"))
        {
            bail!(
                "请设置 SMELT_SIGNAL_HTTP=https://你的信令域名（自部署 smelt-signal，无内置默认）"
            );
        }
        let signal_ws = env::var("SMELT_SIGNAL_WS").unwrap_or_else(|_| {
            let u = signal_http
                .replacen("https://", "wss://", 1)
                .replacen("http://", "ws://", 1);
            format!("{u}/ws")
        });
        let gateway_base = env::var("SMELT_GATEWAY")
            .unwrap_or_else(|_| "http://127.0.0.1:18765".into())
            .trim_end_matches('/')
            .to_string();
        let gateway_token = env::var("SMELT_GATEWAY_TOKEN").unwrap_or_default();
        if gateway_token.is_empty() {
            bail!("请设置 SMELT_GATEWAY_TOKEN=（本机 gateway 启动时打印的 token）");
        }
        let write = env::var("SMELT_WRITE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(true);
        // 默认 = 信令 HTTPS 根（SPA 已嵌进 smelt-signal，同域打开即可）
        let share_base = env::var("SMELT_SHARE_BASE")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| Some(format!("{}/", signal_http.trim_end_matches('/'))));
        Ok(Self {
            signal_http,
            signal_ws,
            gateway_base,
            gateway_token,
            write,
            share_base,
        })
    }
}

fn main() -> Result<()> {
    // GUI 拉起这个子进程时，spawn 发生在它后台执行器的某个线程上；POSIX 的
    // execve() 只重置信号 handler，不重置信号掩码（pthread_sigmask 里被
    // block 的信号照样传给子进程），如果那条线程恰好把 SIGTERM 挡住了，这个
    // 子进程就会继承同样的掩码——GUI 那边 kill(pid, SIGTERM) 发是发出去了，
    // 但这个进程永远收不到，只能 SIGKILL 才杀得掉（实测确认过：GUI 拉起的
    // 子进程对 SIGTERM 完全没反应，同一个二进制自己在普通 shell 里跑则完全
    // 正常）。在起 tokio 运行时（会创建 worker 线程，线程创建按 POSIX 语义
    // 继承掩码）之前先解锁，不管父进程那边线程掩码什么样，自己先正常。
    #[cfg(unix)]
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?
        .block_on(async_main())
}

async fn async_main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    if env::args().any(|a| a == "-h" || a == "--help") {
        print_help();
        return Ok(());
    }

    let cfg = Arc::new(Config::from_env()?);
    info!(
        signal = %cfg.signal_http,
        gateway = %cfg.gateway_base,
        write = cfg.write,
        "smelt-bridge starting"
    );

    // 1) 建房：可被 GUI 预建（SMELT_ROOM + SMELT_SECRET），否则自建
    let (room_id, secret) = match (
        env::var("SMELT_ROOM").ok().filter(|s| !s.is_empty()),
        env::var("SMELT_SECRET").ok().filter(|s| !s.is_empty()),
    ) {
        (Some(r), Some(s)) => {
            info!(room = %r, "using pre-created room");
            (r, s)
        }
        _ => {
            let room = signal::create_room(&cfg.signal_http)
                .await
                .context("POST /v1/rooms")?;
            info!(room = %room.room, "room created");
            (room.room, room.secret)
        }
    };

    let share = build_share_url(&cfg, &room_id, &secret);
    println!();
    println!("========== 跨网链接（手机打开） ==========");
    println!("{share}");
    println!("room={room_id}  secret={secret}");
    println!("signal={}", cfg.signal_ws);
    println!("==========================================");
    println!();

    // 2) 信令 + RTC（阻塞直到结束）
    signal::run_host(cfg, room_id, secret).await?;
    Ok(())
}

fn build_share_url(cfg: &Config, room: &str, secret: &str) -> String {
    // 默认：https://signal…/ （SPA 与信令同域）；手机只开这一条链接
    let base = cfg
        .share_base
        .clone()
        .unwrap_or_else(|| format!("{}/", cfg.signal_http.trim_end_matches('/')));
    let mut u = url::Url::parse(&base).unwrap_or_else(|_| {
        url::Url::parse("https://example.invalid/").expect("static")
    });
    {
        let mut q = u.query_pairs_mut();
        q.append_pair("room", room);
        q.append_pair("k", secret);
        q.append_pair("signal", &cfg.signal_ws);
        q.append_pair("token", &cfg.gateway_token);
    }
    u.to_string()
}

fn print_help() {
    eprintln!(
        "\
smelt-bridge — Mac 跨网桥（host）

环境变量：
  SMELT_SIGNAL_HTTP     信令 HTTP 根（必填，无默认），如 https://signal.example.com
  SMELT_SIGNAL_WS       信令 WS，默认由 HTTP 推导 …/ws
  SMELT_GATEWAY         本机网关，默认 http://127.0.0.1:18765
  SMELT_GATEWAY_TOKEN   网关 token（必填）
  SMELT_WRITE           true/1 允许写（默认 true）
  SMELT_SHARE_BASE      分享页根 URL（可选，拼 ?room&k&signal&token）

用法：
  # 先起可写网关
  cargo run -p smeltd --bin gateway -- --port 18765 --write
  # 另开终端
  SMELT_SIGNAL_HTTP=https://signal.example.com \
  SMELT_GATEWAY_TOKEN=<token> cargo run -p smelt-bridge
"
    );
}
