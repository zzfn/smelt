//! 在线更新：检查 GitHub Release、静默下载新版 dmg、退出时把 `.app` 换成新版本。
//!
//! 不碰 GPUI，纯文件/网络操作，方便独立验证。跨线程跑网络请求的套路照抄
//! `agent.rs`/`pet.rs`：这里没有 tokio 运行时，`reqwest` 得在临时 current-thread
//! 运行时里 `block_on`（调用方负责套 `cx.background_executor().spawn`）。

use std::path::{Path, PathBuf};

const REPO: &str = "zzfn/smelt";
const BUNDLE_ID: &str = "com.zzfn.smelt";
const APP_NAME: &str = "Smelt";
const DMG_ASSET_NAME: &str = "Smelt.dmg";

/// 更新流程的状态机，展示在设置页。
#[derive(Clone)]
pub enum UpdateStatus {
    Idle,
    Checking,
    UpToDate,
    Downloading { version: String },
    ReadyToInstall { version: String, staged_app: PathBuf },
    Failed(String),
}

impl Default for UpdateStatus {
    fn default() -> Self {
        Self::Idle
    }
}

/// 版本号比较：去掉可能的 `v` 前缀，按 `.` 拆 3 段分别 parse 成 u32
/// （缺失/解析失败按 0 处理），逐段比较。`latest` 严格大于 `current` 才算有新版本。
pub fn is_newer(latest: &str, current: &str) -> bool {
    fn parts(v: &str) -> [u32; 3] {
        let v = v.trim().trim_start_matches('v');
        let mut out = [0u32; 3];
        for (i, seg) in v.split('.').take(3).enumerate() {
            out[i] = seg.trim().parse().unwrap_or(0);
        }
        out
    }
    parts(latest) > parts(current)
}

/// 查最新 Release：返回 (版本号去掉 v 前缀, dmg 下载直链)。
pub async fn fetch_latest() -> anyhow::Result<(String, String)> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = reqwest::Client::new()
        .get(&url)
        // GitHub API 没有 User-Agent 会直接 403。
        .header("User-Agent", "smelt-updater")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?;
    let v: serde_json::Value = resp.json().await?;
    let tag = v["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("release 响应缺 tag_name"))?
        .trim_start_matches('v')
        .to_string();
    let dmg_url = v["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str() == Some(DMG_ASSET_NAME))
        .and_then(|a| a["browser_download_url"].as_str())
        .ok_or_else(|| anyhow::anyhow!("最新 release 里没找到 {DMG_ASSET_NAME}"))?
        .to_string();
    Ok((tag, dmg_url))
}

fn cache_root() -> anyhow::Result<PathBuf> {
    let dir = dirs::cache_dir()
        .ok_or_else(|| anyhow::anyhow!("找不到系统缓存目录"))?
        .join(BUNDLE_ID)
        .join("update");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// 下载 dmg → 挂载 → 把里面的 `.app` 拷到缓存目录暂存 → 卸载。返回暂存的 `.app` 路径。
pub async fn download_and_stage(url: &str, version: &str) -> anyhow::Result<PathBuf> {
    let root = cache_root()?;
    let dmg_path = root.join(format!("{APP_NAME}-{version}.dmg"));

    let bytes = reqwest::Client::new()
        .get(url)
        .header("User-Agent", "smelt-updater")
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    std::fs::write(&dmg_path, &bytes)?;

    let (mount_point, device) = attach_dmg(&dmg_path)?;
    let mounted_app = mount_point.join(format!("{APP_NAME}.app"));
    let staged_app = root.join(format!("{APP_NAME}-{version}.app"));
    let _ = std::fs::remove_dir_all(&staged_app);
    let copy_result = run(
        "cp",
        &["-R", &mounted_app.to_string_lossy(), &staged_app.to_string_lossy()],
    );
    detach_dmg(&device);
    let _ = std::fs::remove_file(&dmg_path);
    copy_result?;

    if !staged_app.is_dir() {
        anyhow::bail!("拷贝新版 .app 失败：{}", staged_app.display());
    }
    Ok(staged_app)
}

/// `hdiutil attach` 一个 dmg，解析出挂载路径和设备号（文本 parse，不引入 plist 依赖）。
fn attach_dmg(dmg_path: &Path) -> anyhow::Result<(PathBuf, String)> {
    let out = std::process::Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-readonly"])
        .arg(dmg_path)
        .output()?;
    if !out.status.success() {
        anyhow::bail!("hdiutil attach 失败：{}", String::from_utf8_lossy(&out.stderr));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // 典型输出：/dev/disk4s1          Apple_HFS                      /Volumes/Smelt
    let line = text
        .lines()
        .find(|l| l.contains("/Volumes/"))
        .ok_or_else(|| anyhow::anyhow!("hdiutil attach 输出里没找到挂载路径：{text}"))?;
    let device = line
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("hdiutil attach 输出解析不出设备号：{line}"))?
        .to_string();
    let mount_point = line
        .split("/Volumes/")
        .nth(1)
        .map(|rest| format!("/Volumes/{}", rest.trim()))
        .ok_or_else(|| anyhow::anyhow!("hdiutil attach 输出解析不出挂载路径：{line}"))?;
    Ok((PathBuf::from(mount_point), device))
}

fn detach_dmg(device: &str) {
    let _ = std::process::Command::new("hdiutil").args(["detach", device]).output();
}

fn run(cmd: &str, args: &[&str]) -> anyhow::Result<()> {
    let out = std::process::Command::new(cmd).args(args).output()?;
    if !out.status.success() {
        anyhow::bail!("{cmd} {args:?} 失败：{}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

/// 从当前可执行文件路径反推 `Smelt.app` 的位置：
/// `<App>.app/Contents/MacOS/smelt` 往上 3 层就是 `<App>.app`。
/// 非 `.app` 环境（比如 `cargo run`）直接报错，不做任何文件操作。
fn current_app_bundle() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let bundle = exe
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or_else(|| anyhow::anyhow!("可执行文件路径层级不足：{}", exe.display()))?
        .to_path_buf();
    if bundle.extension().and_then(|e| e.to_str()) != Some("app") {
        anyhow::bail!("不在 .app 包里运行（{}），跳过自更新", bundle.display());
    }
    Ok(bundle)
}

fn backup_path(app_bundle: &Path) -> PathBuf {
    app_bundle.with_extension("app.bak")
}

/// 退出前调用：把当前 `Smelt.app` 换成暂存好的新版本。
/// 成功后下次手动打开就是新版本；失败静默降级（保留旧版本，不阻塞退出）。
pub fn finalize_pending_update(staged_app: &Path) -> anyhow::Result<()> {
    let app_bundle = current_app_bundle()?;
    let backup = backup_path(&app_bundle);
    let _ = std::fs::remove_dir_all(&backup);
    std::fs::rename(&app_bundle, &backup)?;

    if std::fs::rename(staged_app, &app_bundle).is_err() {
        // 跨设备等 rename 失败场景，退化为拷贝。
        if let Err(e) = run("cp", &["-R", &staged_app.to_string_lossy(), &app_bundle.to_string_lossy()])
        {
            // 拷贝也失败：把旧版本挪回去，别把用户晾在一个空目录里。
            let _ = std::fs::rename(&backup, &app_bundle);
            return Err(e);
        }
        let _ = std::fs::remove_dir_all(staged_app);
    }
    let _ = std::fs::remove_dir_all(&backup);
    Ok(())
}

/// 启动时调用：清掉上次 `finalize_pending_update` 没删干净的 `.bak`（自愈，静默忽略失败）。
pub fn cleanup_stale_backup() {
    if let Ok(app_bundle) = current_app_bundle() {
        let _ = std::fs::remove_dir_all(backup_path(&app_bundle));
    }
}
