//! macOS 菜单栏右上角常驻图标（`NSStatusItem`）：显示「需要关注」角标数字，点开是一个
//! 下拉菜单——按状态优先级列出所有会话（状态点 + 名字 + 状态文字），点某一项跳过去；
//! 菜单底部固定一项「打开 smelt 主窗口」，对应原来「点图标就唤出/前置窗口」的行为。
//!
//! 跟 `pet.rs` 那套「绕开 GPUI 直接摸 AppKit」是同一路数——GPUI 本身完全没有
//! status item 这个概念。这里比 pet.rs 多一道坎：要**响应点击**，而 AppKit 的
//! 按钮/菜单项只认 target-action（一个 Objective-C 对象 + 一个 selector），不认 Rust
//! 闭包或 block，所以必须用 `objc::declare::ClassDecl` 声明一个最小的 Objective-C 类当
//! "靶子"。这个类的实例、菜单栏图标、下拉菜单本身都常驻到进程退出，不需要考虑释放；
//! 但下拉菜单里的会话条目会随会话状态变化反复重建，那些临时对象在交给菜单持有后就
//! 显式 release 掉，避免每次重建都攒一份泄漏。

/// 菜单栏点击事件：菜单里点了某个会话条目 → 跳过去；点了菜单底部固定项 → 前置/唤出主窗口。
pub enum StatusItemEvent {
    ActivateMain,
    JumpToSession(usize),
}

/// 下拉菜单里一个会话条目的渲染数据：GPUI 主循环每帧把最新会话快照喂给
/// `update_menu`，本文件负责把它翻译成 AppKit 菜单项。
#[derive(Clone, PartialEq)]
pub struct SessionEntry {
    /// 真实会话下标（`self.sessions` 里的位置）。菜单是按状态优先级排过序的，条目在菜单里
    /// 的位置≠会话下标，所以点击要跳到的目标必须显式带上这个原始下标，不能用菜单位置当 tag。
    pub session_ix: usize,
    pub title: String,
    pub status_text: &'static str,
    pub color: (u8, u8, u8),
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{SessionEntry, StatusItemEvent};
    use objc::declare::ClassDecl;
    use objc::runtime::{Class, Object, Sel};
    use objc::{class, msg_send, sel, sel_impl};
    use std::sync::OnceLock;

    /// 应用图标母图（`scripts/make-icon.sh` 产出），直接编进二进制当菜单栏图标用——
    /// 不用 SF Symbol 占位符了，用户要的是自己的 logo。彩色原样显示（不走
    /// `setTemplate:`，那个只吃 alpha 通道，会把带颜色的方形 logo 拍成纯色剪影）。
    const APP_ICON_PNG: &[u8] = include_bytes!("../../../assets/icon-1024.png");

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NSSize {
        width: f64,
        height: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NSPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NSRect {
        origin: NSPoint,
        size: NSSize,
    }

    /// 点击/选中菜单项只能拿到 Objective-C 层的 self/selector/sender，没法直接闭包
    /// 捕获；用一个全局单例 channel 把事件转发出去，由调用方在 GPUI 事件循环里 drain
    /// （这个文件全程原始 AppKit 调用，不能在这里直接摸 GPUI 的 `Context`）。
    static CLICK_TX: OnceLock<smol::channel::Sender<StatusItemEvent>> = OnceLock::new();

    /// AppKit 对象只能在主线程摸，但 `setup()` 建好之后，`update_menu`/`set_badge`
    /// 这些后续调用要复用同一个 NSMenu / NSStatusBarButton / target 实例；把指针存成
    /// 整数绕开裸指针不是 `Send`/`Sync` 的限制——本文件所有访问都发生在主线程
    /// （AppKit 和 GPUI 事件循环共用同一条主线程），等价于原始指针的正常用法。
    static MENU_PTR: OnceLock<usize> = OnceLock::new();
    static BUTTON_PTR: OnceLock<usize> = OnceLock::new();
    static TARGET_PTR: OnceLock<usize> = OnceLock::new();

    extern "C" fn on_activate(_this: &Object, _cmd: Sel, _sender: *mut Object) {
        if let Some(tx) = CLICK_TX.get() {
            let _ = tx.try_send(StatusItemEvent::ActivateMain);
        }
    }

    /// 会话菜单项的 action：菜单项的 `tag` 就是会话下标（`update_menu` 建条目时按序设的）。
    extern "C" fn on_jump(_this: &Object, _cmd: Sel, sender: *mut Object) {
        let tag: i64 = unsafe { msg_send![sender, tag] };
        if tag >= 0 {
            if let Some(tx) = CLICK_TX.get() {
                let _ = tx.try_send(StatusItemEvent::JumpToSession(tag as usize));
            }
        }
    }

    /// 注册（仅一次）并返回点击靶子类：一个只有两个方法的 `NSObject` 子类。
    fn target_class() -> &'static Class {
        static CLASS: OnceLock<&'static Class> = OnceLock::new();
        *CLASS.get_or_init(|| {
            let mut decl = ClassDecl::new("SmeltStatusItemTarget", class!(NSObject))
                .expect("SmeltStatusItemTarget 类重复注册");
            unsafe {
                decl.add_method(
                    sel!(smeltStatusItemActivate:),
                    on_activate as extern "C" fn(&Object, Sel, *mut Object),
                );
                decl.add_method(
                    sel!(smeltStatusItemJump:),
                    on_jump as extern "C" fn(&Object, Sel, *mut Object),
                );
            }
            decl.register()
        })
    }

    /// `&str` → 临时 `NSString*`（autorelease，仅供本次调用内当参数用，不外泄）。
    unsafe fn nsstring(s: &str) -> *mut Object {
        let c = std::ffi::CString::new(s).unwrap_or_default();
        msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()]
    }

    /// 生成一枚指定颜色的实心小圆点图标，当菜单项左侧的状态灯——`NSBezierPath` 现画
    /// 现填，不需要预先切图。调用方用完（挂到菜单项上）后自己 release 这一份。
    unsafe fn dot_image(color: (u8, u8, u8)) -> *mut Object {
        let size = NSSize {
            width: 10.0,
            height: 10.0,
        };
        let image: *mut Object = msg_send![class!(NSImage), alloc];
        let image: *mut Object = msg_send![image, initWithSize: size];
        let _: () = msg_send![image, lockFocus];
        let ns_color: *mut Object = msg_send![class!(NSColor),
            colorWithSRGBRed: color.0 as f64 / 255.0
            green: color.1 as f64 / 255.0
            blue: color.2 as f64 / 255.0
            alpha: 1.0f64];
        let _: () = msg_send![ns_color, set];
        let rect = NSRect {
            origin: NSPoint { x: 1.0, y: 1.0 },
            size: NSSize {
                width: 8.0,
                height: 8.0,
            },
        };
        let path: *mut Object = msg_send![class!(NSBezierPath), bezierPathWithOvalInRect: rect];
        let _: () = msg_send![path, fill];
        let _: () = msg_send![image, unlockFocus];
        image
    }

    /// 建菜单栏图标 + 空下拉菜单：应用 icon 母图缩到菜单栏尺寸，取不到（理论上不会，
    /// PNG 编进二进制里的，兜底而已）就退化成文字。菜单内容留给 `update_menu` 按会话
    /// 状态填充。图标、菜单、点击靶子实例都常驻到进程退出，故意不释放。
    pub fn setup(tx: smol::channel::Sender<StatusItemEvent>) {
        let _ = CLICK_TX.set(tx);
        unsafe {
            let bar: *mut Object = msg_send![class!(NSStatusBar), systemStatusBar];
            // NSVariableStatusItemLength == -1.0，让系统按内容自适应宽度。
            let item: *mut Object = msg_send![bar, statusItemWithLength: -1.0f64];
            let _: () = msg_send![item, retain]; // 常驻单例：必须自己按住，不然出了这个
            // 作用域就被 autorelease 池收走。

            let button: *mut Object = msg_send![item, button];
            let _: () = msg_send![button, retain];
            let _ = BUTTON_PTR.set(button as usize);

            let data: *mut Object = msg_send![
                class!(NSData),
                dataWithBytes: APP_ICON_PNG.as_ptr() as *const std::ffi::c_void
                length: APP_ICON_PNG.len()
            ];
            let image: *mut Object = msg_send![class!(NSImage), alloc];
            let image: *mut Object = msg_send![image, initWithData: data];
            if !image.is_null() {
                // 母图是 1024×1024，菜单栏图标按 18pt 显示（跟系统自带图标的观感尺寸
                // 对齐），NSImage 自己插值缩小，不用额外裁剪/预生成小图。
                let _: () = msg_send![image, setSize: NSSize { width: 18.0, height: 18.0 }];
                let _: () = msg_send![button, setImage: image];
                let _: () = msg_send![button, setImagePosition: 2u64]; // NSImageLeft：图标靠左，角标数字（若有）跟在右边
            } else {
                let _: () = msg_send![button, setTitle: nsstring("smelt")];
            }

            let target: *mut Object = msg_send![target_class(), new]; // +1，永不 release
            let _ = TARGET_PTR.set(target as usize);

            let menu: *mut Object = msg_send![class!(NSMenu), new]; // +1，永不 release
            let _ = MENU_PTR.set(menu as usize);
            let _: () = msg_send![item, setMenu: menu]; // 挂了菜单后，点按钮直接弹菜单，不再走 button 的 target/action
        }
    }

    /// 设置图标角标：`count == 0` 清空，否则在图标右侧显示数字（跟截图里其它状态栏
    /// 图标旁的数字角标同一种做法——就是按钮的 `title`，不需要额外画徽标）。
    pub fn set_badge(count: usize) {
        let Some(&ptr) = BUTTON_PTR.get() else { return };
        unsafe {
            let button = ptr as *mut Object;
            let title = if count == 0 {
                String::new()
            } else {
                count.to_string()
            };
            let _: () = msg_send![button, setTitle: nsstring(&title)];
        }
    }

    /// 按会话快照重建下拉菜单：先清空，逐个会话建一个「状态点 + 标题 + 状态文字」的
    /// 菜单项（`tag` 记会话下标，点击经 `on_jump` 转发），最后加一条分隔线 + 固定的
    /// 「打开 smelt 主窗口」项。只在会话快照真的变化时被调用（见 main.rs），不是每帧都建。
    pub fn update_menu(entries: &[SessionEntry]) {
        let (Some(&menu_ptr), Some(&target_ptr)) = (MENU_PTR.get(), TARGET_PTR.get()) else {
            return;
        };
        unsafe {
            let menu = menu_ptr as *mut Object;
            let target = target_ptr as *mut Object;
            let _: () = msg_send![menu, removeAllItems];

            for entry in entries.iter() {
                let title = format!("{} — {}", entry.title, entry.status_text);
                let item: *mut Object = msg_send![class!(NSMenuItem), alloc];
                let item: *mut Object = msg_send![item,
                    initWithTitle: nsstring(&title)
                    action: sel!(smeltStatusItemJump:)
                    keyEquivalent: nsstring("")];
                // tag 记真实会话下标（不是菜单位置——菜单排过序），点击经 on_jump 原样带回。
                let _: () = msg_send![item, setTag: entry.session_ix as i64];
                let _: () = msg_send![item, setTarget: target];
                let dot = dot_image(entry.color);
                let _: () = msg_send![item, setImage: dot];
                let _: () = msg_send![dot, release]; // setImage: 会 copy 一份，这份原件用不着了
                let _: () = msg_send![menu, addItem: item];
                let _: () = msg_send![item, release]; // addItem: 会 retain 一份，这份原件用不着了
            }

            if !entries.is_empty() {
                let sep: *mut Object = msg_send![class!(NSMenuItem), separatorItem];
                let _: () = msg_send![menu, addItem: sep];
            }

            let open_item: *mut Object = msg_send![class!(NSMenuItem), alloc];
            let open_item: *mut Object = msg_send![open_item,
                initWithTitle: nsstring("打开 smelt 主窗口")
                action: sel!(smeltStatusItemActivate:)
                keyEquivalent: nsstring("")];
            let _: () = msg_send![open_item, setTarget: target];
            let _: () = msg_send![menu, addItem: open_item];
            let _: () = msg_send![open_item, release];
        }
    }

    /// 点「打开 smelt 主窗口」时若主窗口已经活着：把 app 前置（smelt 目前只有一扇主
    /// 窗口，前置整个 app 等价于前置它，不需要单独找出那扇窗口）。
    pub fn activate_app() {
        unsafe {
            let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
            let _: () = msg_send![app, activateIgnoringOtherApps: objc::runtime::YES];
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::{SessionEntry, StatusItemEvent};

    pub fn setup(_tx: smol::channel::Sender<StatusItemEvent>) {}
    pub fn activate_app() {}
    pub fn set_badge(_count: usize) {}
    pub fn update_menu(_entries: &[SessionEntry]) {}
}

pub use imp::{activate_app, set_badge, setup, update_menu};
