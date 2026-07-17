//! 终端网格 → 文本行 —— GUI 与 smeltd 共用（`#[path]` 引入，同 title_spinner.rs）。
//!
//! 拆出来的原因和 permission_menu.rs 一样：两端都要「把可视区读成文本」才能扫权限
//! 菜单，各写一份迟早漂。逐格拼行有几处容易写错的细节，一份实现就只用对一次：
//!   - 宽字符（CJK / emoji）在网格里占两格，第二格是 `WIDE_CHAR_SPACER` 占位，
//!     必须跳过，否则每个中文后面都会多一个空格；
//!   - 零宽字符（组合音标、变体选择符）挂在所属格上，得跟着一起取；
//!   - 行尾空白要裁掉，否则「末尾空行」判断和 trim 逻辑全乱。

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions; // Term::columns() 由它提供
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::Term;

/// 可视区逐格拼成文本行（一行一个 String，行尾空白已裁）。
pub fn text_lines<T: EventListener>(term: &Term<T>) -> Vec<String> {
    let cols = term.columns().max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut count = 0usize;
    for indexed in term.renderable_content().display_iter {
        let cell = indexed.cell;
        if !cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            cur.push(cell.c);
            if let Some(zw) = cell.zerowidth() {
                cur.extend(zw);
            }
        }
        count += 1;
        if count % cols == 0 {
            lines.push(std::mem::take(&mut cur).trim_end().to_string());
        }
    }
    if !cur.is_empty() {
        lines.push(cur.trim_end().to_string());
    }
    lines
}

/// 末尾 n 行（先丢掉尾部空行）——权限菜单只出现在屏幕底部那一段，
/// 往上多扫只会把历史里的旧菜单也扫进来。
pub fn last_lines<T: EventListener>(term: &Term<T>, n: usize) -> Vec<String> {
    let mut lines = text_lines(term);
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let start = lines.len().saturating_sub(n);
    lines[start..].to_vec()
}
