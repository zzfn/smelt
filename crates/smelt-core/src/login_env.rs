//! 交互式 login shell 环境探测：GUI 是 Finder/Dock 拉起的进程，读不到用户
//! `.zshrc` 里 `export` 的值——不止 PATH（nvm/homebrew 装的 CLI 找不到），
//! 各家 agent 自定义 workspace 目录的环境变量也是同一个坑：
//!
//! - Claude：`CLAUDE_CONFIG_DIR`（非官方文档但实测生效，默认 `~/.claude`）
//! - Codex：`CODEX_HOME`（官方文档，默认 `~/.codex`）
//! - Grok：`GROK_HOME`（写在 Grok CLI 自带的用户手册里，默认 `~/.grok`）
//! - Copilot：`COPILOT_HOME`（官方推荐用法，优先于遗留的 `--config-dir`）或
//!   `XDG_CONFIG_HOME`（改成 `$XDG_CONFIG_HOME/copilot`），默认 `~/.copilot`
//!
//! 一次 `zsh -ilc` 探测把这些变量连同 PATH 一起拿到，不为每个变量单开一次
//! 慢 shell（`-ilc` 要跑一遍 `.zshrc`，有感知延迟）。

use std::process::Command;
use std::sync::OnceLock;

#[derive(Default)]
struct LoginEnv {
    path: String,
    claude_config_dir: Option<String>,
    codex_home: Option<String>,
    grok_home: Option<String>,
    copilot_home: Option<String>,
    xdg_config_home: Option<String>,
}

/// 要探测的变量：(名字, marker 前缀)。PATH 永远有值（进程自身 PATH 兜底），
/// 其余四个探测不到/未设置时是 `None`，调用方回退各自的默认目录。
const VARS: &[&str] =
    &["PATH", "CLAUDE_CONFIG_DIR", "CODEX_HOME", "GROK_HOME", "COPILOT_HOME", "XDG_CONFIG_HOME"];

fn login_env() -> &'static LoginEnv {
    static ENV: OnceLock<LoginEnv> = OnceLock::new();
    ENV.get_or_init(probe)
}

fn probe() -> LoginEnv {
    let script: String = VARS
        .iter()
        .map(|v| format!(r#"printf "__V_{v}_B__%s__V_{v}_E__" "${v}";"#))
        .collect::<Vec<_>>()
        .join(" ");
    let raw = Command::new("/bin/zsh")
        .args(["-ilc", &script])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());

    let mut env = LoginEnv::default();
    if let Some(raw) = &raw {
        env.path = extract_marked(raw, "PATH").filter(|p| !p.is_empty()).unwrap_or_default();
        env.claude_config_dir = extract_marked(raw, "CLAUDE_CONFIG_DIR").filter(|s| !s.is_empty());
        env.codex_home = extract_marked(raw, "CODEX_HOME").filter(|s| !s.is_empty());
        env.grok_home = extract_marked(raw, "GROK_HOME").filter(|s| !s.is_empty());
        env.copilot_home = extract_marked(raw, "COPILOT_HOME").filter(|s| !s.is_empty());
        env.xdg_config_home = extract_marked(raw, "XDG_CONFIG_HOME").filter(|s| !s.is_empty());
    }
    if env.path.is_empty() {
        env.path = std::env::var("PATH").unwrap_or_default();
    }
    env
}

/// 从探测输出里抠出 `__V_<name>_B__…__V_<name>_E__` 之间的内容，丢掉交互式
/// shell 启动时 shell-integration 打的 OSC 转义噪音（形如
/// `\e]1337;…;shell=zsh`，会粘在第一个变量前面，见 acp_conn.rs 历史踩坑记录）。
/// 找不到标记（shell 没跑起来 / 输出被截断）返回 None。
fn extract_marked(raw: &str, name: &str) -> Option<String> {
    let begin = format!("__V_{name}_B__");
    let end = format!("__V_{name}_E__");
    let start = raw.find(&begin)? + begin.len();
    let rest = &raw[start..];
    let stop = rest.find(&end)?;
    Some(rest[..stop].trim().to_string())
}

/// login shell 里的 PATH（进程存活期只探一次）。终端会话不需要这个——那边
/// shell 由 smeltd 起，自带 login 环境；ACP 子进程是 GUI 直接 spawn 的，得
/// 自己补上 nvm/homebrew 这些用户级 PATH，不然 `npx`/`bunx` 直接 ENOENT。
pub fn login_path() -> &'static str {
    &login_env().path
}

/// 四个 workspace 覆盖变量的读取顺序：**先查当前进程自己的环境**，查不到再
/// 退回登录 shell 探测出来的缓存值。
///
/// 为什么不直接只用探测缓存：探测结果在整个进程生命周期只算一次
/// （`OnceLock`），但从终端 `cargo run`/`cargo test` 启动时，进程其实已经
/// 原样继承了当前 shell 的完整环境（包含用户在 `.zshrc` 里 export 的这些
/// 变量）——这种情况下直接查进程自身环境更快、更准，不用等一次 `zsh -ilc`；
/// 只有 Finder/Dock 拉起的 GUI 才会出现"进程环境里没有，得靠探测补"的落差
/// （跟 PATH 是同一个原理，但 PATH 那份要跟安装目录兜底合并成一条完整搜索
/// 路径，逻辑不同，没法共用这个直查优先的简单版本）。
/// 副作用：测试可以直接 `std::env::set_var`/`remove_var` 这几个变量来控制
/// 行为，不受 `OnceLock` 探测缓存污染——这也是选直查优先的实际原因，本仓库
/// 就真的踩过"开发机全局设了 CLAUDE_CONFIG_DIR，historia 测试假 HOME 沙盒
/// 被越过"这个坑。
fn resolve_override(var: &str, cached: &'static Option<String>) -> Option<String> {
    std::env::var(var).ok().filter(|s| !s.is_empty()).or_else(|| cached.clone())
}

/// Claude Code 自定义 workspace 目录（`CLAUDE_CONFIG_DIR`），没设就是 `None`
/// （调用方回退 `~/.claude`）。同一台机器可能想在不同 workspace 之间切换
/// （比如同时维护 `~/.claude` 和 `~/.claude-quant`）——这里只解决"GUI 进程
/// 能读到当前生效的那个值"，多 workspace 之间怎么选/怎么记还需要更上层的
/// 设置支持（见 acp_conn.rs 里 AcpLaunch 未来怎么带每次启动各自的覆盖值）。
pub fn claude_config_dir() -> Option<String> {
    resolve_override("CLAUDE_CONFIG_DIR", &login_env().claude_config_dir)
}

/// Codex CLI 自定义目录（`CODEX_HOME`），没设就是 `None`（回退 `~/.codex`）。
pub fn codex_home() -> Option<String> {
    resolve_override("CODEX_HOME", &login_env().codex_home)
}

/// Grok CLI 自定义目录（`GROK_HOME`），没设就是 `None`（回退 `~/.grok`）。
pub fn grok_home() -> Option<String> {
    resolve_override("GROK_HOME", &login_env().grok_home)
}

/// Copilot CLI 自定义目录：`COPILOT_HOME` 优先（官方推荐、整段替换默认路径），
/// 没设再看 `XDG_CONFIG_HOME`（此时基准目录是 `$XDG_CONFIG_HOME/copilot`，
/// 调用方需要自己拼这一段，不是直接可用的完整路径，所以这里分两个访问器）。
pub fn copilot_home() -> Option<String> {
    resolve_override("COPILOT_HOME", &login_env().copilot_home)
}

pub fn xdg_config_home() -> Option<String> {
    resolve_override("XDG_CONFIG_HOME", &login_env().xdg_config_home)
}

#[cfg(test)]
mod tests {
    use super::extract_marked;

    /// 交互式 zsh 会在第一个变量前粘一段 shell-integration 的 OSC 噪音（实测
    /// 形如 `\e]1337;…;shell=zsh`）。抠取逻辑必须只取标记之间的内容——这正是
    /// 从 acp_conn.rs 的 `extract_marked_path` 搬过来并泛化成多变量版本，
    /// 同一条回归锁死一起搬过来。
    #[test]
    fn strips_shell_integration_noise_before_first_var() {
        let raw = "\u{1b}]1337;RemoteHost=me@host\u{1b}\\\u{1b}]1337;ShellIntegrationVersion=14;shell=zsh\
                   __V_PATH_B__/Users/me/.grok/bin:/usr/bin:/bin__V_PATH_E__\
                   __V_CLAUDE_CONFIG_DIR_B____V_CLAUDE_CONFIG_DIR_E__";
        let path = extract_marked(raw, "PATH").expect("应能抠出 PATH");
        assert_eq!(path, "/Users/me/.grok/bin:/usr/bin:/bin");
        assert!(path.starts_with("/Users/me/.grok/bin"), "第一段目录不能被 OSC 噪音污染");
        // 没设置的变量应该是空字符串（"$VAR" 展开成空），不是 None——None 是
        // "没找到标记"这种更严重的失败，两者含义不同。
        assert_eq!(extract_marked(raw, "CLAUDE_CONFIG_DIR"), Some(String::new()));
    }

    #[test]
    fn returns_none_without_markers() {
        assert!(extract_marked("/usr/bin:/bin", "PATH").is_none());
        assert!(extract_marked("", "PATH").is_none());
    }

    #[test]
    fn returns_none_when_end_marker_missing() {
        assert!(extract_marked("noise__V_PATH_B__/usr/bin", "PATH").is_none());
    }

    #[test]
    fn multiple_vars_in_one_output_dont_bleed_into_each_other() {
        let raw = "__V_PATH_B__/bin__V_PATH_E__\
                   __V_CODEX_HOME_B__/custom/.codex__V_CODEX_HOME_E__\
                   __V_GROK_HOME_B____V_GROK_HOME_E__";
        assert_eq!(extract_marked(raw, "PATH").as_deref(), Some("/bin"));
        assert_eq!(extract_marked(raw, "CODEX_HOME").as_deref(), Some("/custom/.codex"));
        assert_eq!(extract_marked(raw, "GROK_HOME").as_deref(), Some(""));
    }
}
