//! Dock 图标角标：用「需要关注」的会话数当角标数字，跟总览页的状态徽章同一份数据源
//! （见 main.rs 的 AgentStatus），只是把提醒面挪到 Dock 上——切走 smelt 也能瞥见。

/// 设置 Dock 图标角标：`count == 0` 清空角标，否则显示数字。
/// `[[NSApplication sharedApplication] dockTile] setBadgeLabel:]`——跟应用是否在
/// 前台无关，全局唯一，不需要拿具体窗口。
#[cfg(target_os = "macos")]
pub fn set_badge(count: usize) {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};

    unsafe {
        let ns_app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        if ns_app.is_null() {
            return;
        }
        let dock_tile: *mut Object = msg_send![ns_app, dockTile];
        if dock_tile.is_null() {
            return;
        }
        let label: *mut Object = if count == 0 {
            std::ptr::null_mut() // nil 清空角标
        } else {
            let Ok(c_string) = std::ffi::CString::new(count.to_string()) else { return };
            msg_send![class!(NSString), stringWithUTF8String: c_string.as_ptr()]
        };
        let _: () = msg_send![dock_tile, setBadgeLabel: label];
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set_badge(_count: usize) {}
