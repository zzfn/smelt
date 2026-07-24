//! 终端 / markdown 渲染共用的等宽字体配置：一个全局可写、随处可读的字体名，
//! 用户在设置里改一次，终端网格测量和 markdown 代码块渲染都得用同一个字体
//! 来源——否则两边各自 fallback 到不同字体，量出来的字宽和实际渲染对不上。

use std::sync::RwLock;

/// 内嵌进二进制的默认等宽字体（见 GUI 侧 `add_fonts`），多数用户不用改。
pub const DEFAULT_FONT_FAMILY: &str = "JetBrainsMono Nerd Font Mono";

static FONT_FAMILY_CONF: RwLock<Option<String>> = RwLock::new(None);

/// 设置页改字体时调用。
pub fn set_font_family(name: &str) {
    let name = name.trim();
    if let Ok(mut g) = FONT_FAMILY_CONF.write() {
        *g = (!name.is_empty()).then(|| name.to_string());
    }
}

/// 当前生效的等宽字体名；没设置过就是内嵌默认字体。
pub fn font_family() -> String {
    FONT_FAMILY_CONF.read().ok().and_then(|g| g.clone()).unwrap_or_else(|| DEFAULT_FONT_FAMILY.to_string())
}
