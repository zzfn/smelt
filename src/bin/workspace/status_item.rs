//! macOS 菜单栏右上角常驻图标（`NSStatusItem`）：点一下唤出/前置主窗口。
//!
//! 跟 `pet.rs` 那套「绕开 GPUI 直接摸 AppKit」是同一路数——GPUI 本身完全没有
//! status item 这个概念。但这里比 pet.rs 多一道坎：pet.rs 只调用现成对象的方法
//! （`NSWindow`/`NSEvent`），这次需要**响应点击**，而 AppKit 的按钮/菜单项只认
//! target-action（一个 Objective-C 对象 + 一个 selector），不认 Rust 闭包或 block，
//! 所以必须用 `objc::declare::ClassDecl` 声明一个最小的 Objective-C 类当"靶子"。
//! 这个类和它的一个实例常驻到进程退出，不需要考虑释放。

#[cfg(target_os = "macos")]
mod imp {
    use objc::declare::ClassDecl;
    use objc::runtime::{Class, Object, Sel, YES};
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

    /// 点击回调只能拿到 Objective-C 层的 self/selector/sender，没法直接闭包捕获；
    /// 用一个全局单例 channel 把点击事件转发出去，由调用方在 GPUI 事件循环里 drain
    /// （这个文件全程原始 AppKit 调用，不能在这里直接摸 GPUI 的 `Context`）。
    static CLICK_TX: OnceLock<smol::channel::Sender<()>> = OnceLock::new();

    extern "C" fn on_click(_this: &Object, _cmd: Sel, _sender: *mut Object) {
        if let Some(tx) = CLICK_TX.get() {
            let _ = tx.try_send(());
        }
    }

    /// 注册（仅一次）并返回点击靶子类：一个只有一个方法的 `NSObject` 子类。
    fn target_class() -> &'static Class {
        static CLASS: OnceLock<&'static Class> = OnceLock::new();
        *CLASS.get_or_init(|| {
            let mut decl = ClassDecl::new("SmeltStatusItemTarget", class!(NSObject))
                .expect("SmeltStatusItemTarget 类重复注册");
            unsafe {
                decl.add_method(
                    sel!(smeltStatusItemClicked:),
                    on_click as extern "C" fn(&Object, Sel, *mut Object),
                );
            }
            decl.register()
        })
    }

    /// `&str` → 临时 `NSString*`（autorelease，仅供本次调用内当参数用，不外泄）。
    unsafe fn nsstring(s: &str) -> *mut Object {
        let c = std::ffi::CString::new(s).expect("状态栏字符串不含 NUL");
        msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()]
    }

    /// 建菜单栏图标：应用 icon 母图缩到菜单栏尺寸，取不到（理论上不会，PNG 编进二进制
    /// 里的，兜底而已）就退化成文字。点击发一条消息到 `tx`。图标本身、点击靶子实例都
    /// 常驻到进程退出，故意不释放。
    pub fn setup(tx: smol::channel::Sender<()>) {
        let _ = CLICK_TX.set(tx);
        unsafe {
            let bar: *mut Object = msg_send![class!(NSStatusBar), systemStatusBar];
            // NSVariableStatusItemLength == -1.0，让系统按内容自适应宽度。
            let item: *mut Object = msg_send![bar, statusItemWithLength: -1.0f64];
            let _: () = msg_send![item, retain]; // 常驻单例：必须自己按住，不然出了这个
            // 作用域就被 autorelease 池收走。

            let button: *mut Object = msg_send![item, button];
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
            } else {
                let _: () = msg_send![button, setTitle: nsstring("smelt")];
            }

            let target: *mut Object = msg_send![target_class(), new]; // +1，永不 release
            let _: () = msg_send![button, setTarget: target];
            let _: () = msg_send![button, setAction: sel!(smeltStatusItemClicked:)];
        }
    }

    /// 点图标时若主窗口已经活着：把 app 前置（smelt 目前只有一扇主窗口，前置整个
    /// app 等价于前置它，不需要单独找出那扇窗口）。
    pub fn activate_app() {
        unsafe {
            let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
            let _: () = msg_send![app, activateIgnoringOtherApps: YES];
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn setup(_tx: smol::channel::Sender<()>) {}
    pub fn activate_app() {}
}

pub use imp::{activate_app, setup};
