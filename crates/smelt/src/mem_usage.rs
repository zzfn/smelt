//! 当前进程常驻内存（RSS）采样，供调试 HUD 显示。

/// 读取本进程 RSS（字节）。失败时返回 `None`（HUD 里显示为 `—`）。
pub fn current_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        return macos_rss();
    }
    #[cfg(target_os = "linux")]
    {
        return linux_rss();
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// 把字节格式化成 HUD 用的短字符串（MB / GB）。
pub fn format_rss(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else {
        format!("{:.0} MB", b / MB)
    }
}

#[cfg(target_os = "macos")]
#[allow(deprecated)] // libc::mach_task_self 标了 deprecated，但本项目已依赖 libc，不必为 HUD 再加 mach2。
fn macos_rss() -> Option<u64> {
    use std::mem::MaybeUninit;

    use libc::{
        KERN_SUCCESS, MACH_TASK_BASIC_INFO, MACH_TASK_BASIC_INFO_COUNT, mach_task_basic_info,
        mach_task_self, task_info, task_info_t,
    };

    let mut info = MaybeUninit::<mach_task_basic_info>::uninit();
    let mut count = MACH_TASK_BASIC_INFO_COUNT;
    let kr = unsafe {
        task_info(
            mach_task_self(),
            MACH_TASK_BASIC_INFO,
            info.as_mut_ptr() as task_info_t,
            &mut count,
        )
    };
    if kr != KERN_SUCCESS {
        return None;
    }
    let info = unsafe { info.assume_init() };
    Some(info.resident_size as u64)
}

#[cfg(target_os = "linux")]
fn linux_rss() -> Option<u64> {
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return None;
    }
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let rss_pages = statm.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    Some(rss_pages * page_size as u64)
}
