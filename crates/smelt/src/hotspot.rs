//! hotspot：从 git 历史提炼「改动热力」——哪些文件改得又多又频繁（经典 hotspot
//! 分析：Adam Tornhill 的 code-as-a-crime-scene 思路）。只读 `git log`，不碰文件内容。
//!
//! 分数 = 每次改动按时间指数衰减后求和：改得越勤 + 改得越近，分数越高，
//! 天然把「改动频率」和「最近改动」糅进同一个热度值，供 Git 视角的热力图上色。

use std::collections::HashMap;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// 只统计最近这么多天的 git 历史。
pub const WINDOW_DAYS: i64 = 90;
/// 衰减时间常数（天）：改动权重 = exp(-age_days / TAU)，越大衰减越慢。
const TAU_DAYS: f64 = 30.0;
/// 记录分隔符：commit 头行用它打头，几乎不会出现在真实文本里。
const REC_SEP: char = '\u{1e}';

/// 一个文件在统计窗口内的热力数据。
#[derive(Clone)]
pub struct HotspotEntry {
    /// 相对仓库根的路径。
    pub rel_path: String,
    /// 窗口内改动次数。
    pub commits: u32,
    /// 时间衰减后的热力分数（频率 + 时近性）。
    pub score: f64,
    /// 距最近一次改动过去的天数。
    pub days_since: f64,
}

/// 统计某仓库的改动热力，按分数降序返回；非 git 仓库或无历史时返回空列表。
pub fn compute(root: &str) -> Vec<HotspotEntry> {
    let out = Command::new("git")
        .args([
            "-C",
            root,
            "log",
            &format!("--since={WINDOW_DAYS}.days"),
            &format!("--pretty=format:{REC_SEP}%ct"),
            "--name-only",
        ])
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut per_file: HashMap<String, (u32, f64, i64)> = HashMap::new();
    let mut cur_ts: Option<i64> = None;
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(REC_SEP) {
            cur_ts = rest.trim().parse::<i64>().ok();
            continue;
        }
        let path = line.trim();
        if path.is_empty() {
            continue;
        }
        let Some(ts) = cur_ts else { continue };
        let age_days = ((now - ts) as f64 / 86400.0).max(0.0);
        let weight = (-age_days / TAU_DAYS).exp();
        let entry = per_file.entry(path.to_string()).or_insert((0, 0.0, ts));
        entry.0 += 1;
        entry.1 += weight;
        if ts > entry.2 {
            entry.2 = ts;
        }
    }

    // 只保留仍然存在的文件——已删除的历史路径画进热力图没有意义。
    let mut entries: Vec<HotspotEntry> = per_file
        .into_iter()
        .filter(|(path, _)| std::path::Path::new(root).join(path).is_file())
        .map(|(rel_path, (commits, score, last_ts))| HotspotEntry {
            rel_path,
            commits,
            score,
            days_since: ((now - last_ts) as f64 / 86400.0).max(0.0),
        })
        .collect();
    entries.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    entries
}

/// 单位矩形 [0,1]x[0,1] 内的一块布局结果。
#[derive(Clone, Copy, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Squarified treemap：把一组权重铺进单位正方形，尽量让每块接近正方形（Bruls 算法）。
/// `weights` 须为正数且按调用方期望的顺序排列（通常已按分数降序）；返回值与输入等长同序。
pub fn squarify(weights: &[f64]) -> Vec<Rect> {
    let total: f64 = weights.iter().sum();
    if weights.is_empty() || total <= 0.0 {
        return vec![Rect::default(); weights.len()];
    }
    // 归一化到面积 = 1（单位正方形），下面全部在这个尺度里计算。
    let sizes: Vec<f64> = weights.iter().map(|w| w / total).collect();
    let mut out = vec![Rect::default(); sizes.len()];
    layout(&sizes, &mut out, 0.0, 0.0, 1.0, 1.0);
    out
}

/// 递归铺完剩余矩形区域：每轮贪心攒一「行」（沿短边），行内摆满就切掉已用区域递归。
fn layout(sizes: &[f64], out: &mut [Rect], x: f32, y: f32, w: f32, h: f32) {
    if sizes.is_empty() || w <= 0.0 || h <= 0.0 {
        return;
    }
    if sizes.len() == 1 {
        out[0] = Rect { x, y, w, h };
        return;
    }
    let side = w.min(h) as f64;
    // 贪心扩大行：只要加入下一个元素能让「最差长宽比」变得更好就继续加。
    let mut row_end = 1;
    let mut row_sum = sizes[0];
    while row_end < sizes.len() {
        let next_sum = row_sum + sizes[row_end];
        if worst_ratio(&sizes[..row_end], row_sum, side) >= worst_ratio(&sizes[..=row_end], next_sum, side)
        {
            row_sum = next_sum;
            row_end += 1;
        } else {
            break;
        }
    }

    let row = &sizes[..row_end];
    let rest = &sizes[row_end..];
    let (mut out_row, out_rest) = out.split_at_mut(row_end);

    if w >= h {
        // 宽边：切一列，行内元素竖直堆叠。
        let col_w = (row_sum / h as f64) as f32;
        let mut cy = y;
        for (i, &s) in row.iter().enumerate() {
            let item_h = (s / row_sum) as f32 * h;
            out_row[i] = Rect { x, y: cy, w: col_w, h: item_h };
            cy += item_h;
        }
        layout(rest, out_rest, x + col_w, y, (w - col_w).max(0.0), h);
    } else {
        // 高边：切一行，行内元素水平排列。
        let row_h = (row_sum / w as f64) as f32;
        let mut cx = x;
        for (i, &s) in row.iter().enumerate() {
            let item_w = (s / row_sum) as f32 * w;
            out_row[i] = Rect { x: cx, y, w: item_w, h: row_h };
            cx += item_w;
        }
        layout(rest, out_rest, x, y + row_h, w, (h - row_h).max(0.0));
    }
    let _ = &mut out_row;
}

/// 一行内「最差长宽比」（越接近 1 越接近正方形）：max(w²·max/s², s²/(w²·min))。
fn worst_ratio(row: &[f64], row_sum: f64, side: f64) -> f64 {
    if row.is_empty() || row_sum <= 0.0 || side <= 0.0 {
        return f64::INFINITY;
    }
    let max = row.iter().cloned().fold(f64::MIN, f64::max);
    let min = row.iter().cloned().fold(f64::MAX, f64::min);
    let side2 = side * side;
    let sum2 = row_sum * row_sum;
    ((side2 * max) / sum2).max(sum2 / (side2 * min))
}

// ===================== GPUI 面板 =====================
//
// 以上是纯逻辑（无 GPUI 依赖，好单测）；以下是从 main.rs 拆过来的面板部分——
// `impl Workspace` 方法 + 渲染函数，字段仍然声明在 main.rs 的 `Workspace` struct 里。

use gpui::*;
use gpui_component::*;
use std::rc::Rc;
use std::time::Instant;

use crate::{placeholder_view, MainView, Workspace};

/// 冷→热配色：t∈[0,1]（由排名百分位归一化，见 hotspot_view）从冷蓝经琥珀到警示红。
fn heat_color(t: f32) -> Hsla {
    let t = t.clamp(0.0, 1.0);
    let stops: [(u8, u8, u8); 3] = [(0x2a, 0x41, 0x5c), (0xd9, 0x8a, 0x2e), (0xe0, 0x38, 0x38)];
    let (lo, hi, local_t) = if t < 0.5 { (stops[0], stops[1], t / 0.5) } else { (stops[1], stops[2], (t - 0.5) / 0.5) };
    let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * local_t).round() as u32;
    let packed = (lerp(lo.0, hi.0) << 16) | (lerp(lo.1, hi.1) << 8) | lerp(lo.2, hi.2);
    rgb(packed).into()
}

/// 热力图视图：squarified treemap——每个矩形是一个近 90 天内改动过的文件。
/// 面积 = 热力分数（改动频率 × 时间衰减，见 hotspot::compute）；颜色则按热力排名百分位
/// 取色（而非分数原始值直接映射）——分数分布是指数衰减的长尾，直接按分数取色会导致只有
/// 前一两名亮红/亮橙、其余瞬间跌成同一片暗色，按排名取色才能让整张图有连续的冷暖梯度。
/// 右上角小圆点额外标出「最近改动」（2 天内）的文件；点击某块直接在文件树里打开对应文件。
pub fn hotspot_view(
    cwd: Option<String>,
    data: Option<Rc<Vec<HotspotEntry>>>,
    cx: &mut Context<Workspace>,
) -> Div {
    let (muted, c_bg) = {
        let t = cx.theme();
        (t.muted_foreground, t.background)
    };
    let Some(root) = cwd else {
        return placeholder_view("无项目目录", muted);
    };
    let Some(entries) = data else {
        return placeholder_view("计算改动热力中…", muted);
    };
    if entries.is_empty() {
        return placeholder_view(
            &format!(
                "近 {} 天无改动记录（非 git 仓库，或近期无改动）",
                WINDOW_DAYS
            ),
            muted,
        );
    }

    // 只画热力最高的一批：太多小方块既放不下标签也没有辨识度。
    const MAX_TILES: usize = 80;
    let total = entries.len();
    let shown: Vec<&HotspotEntry> = entries.iter().take(MAX_TILES).collect();
    let weights: Vec<f64> = shown.iter().map(|e| e.score.max(1e-6)).collect();
    let rects = squarify(&weights);
    let last_ix = shown.len().saturating_sub(1).max(1) as f32;

    let this = cx.entity();
    let tiles: Vec<AnyElement> = shown
        .iter()
        .zip(rects.iter())
        .enumerate()
        .map(|(i, (entry, rect))| {
            // 排名百分位（0 = 最热）取色，与面积（真实分数）解耦，避免长尾把色阶压平。
            let heat = 1.0 - (i as f32 / last_ix);
            let recent = entry.days_since < 2.0;
            let name = std::path::Path::new(&entry.rel_path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| entry.rel_path.clone());
            let abs_path = std::path::Path::new(&root).join(&entry.rel_path).to_string_lossy().into_owned();
            let this = this.clone();
            let show_label = rect.w > 0.05 && rect.h > 0.06;

            let mut tile = div()
                .id(("hotspot-tile", i))
                .absolute()
                .left(relative(rect.x))
                .top(relative(rect.y))
                .w(relative(rect.w))
                .h(relative(rect.h))
                .overflow_hidden()
                .cursor_pointer()
                .rounded(px(4.))
                .border_2()
                .border_color(c_bg)
                .bg(heat_color(heat))
                .hover(|d| d.border_color(rgb(0x4a9eff)))
                .on_click(move |_ev, window, cx| {
                    this.update(cx, |ws, cx| {
                        ws.view = MainView::Files;
                        ws.view_file(abs_path.clone(), window, cx);
                    });
                    // 见总览入口同款注释：文件树页没有可聚焦元素，focus 显式
                    // 认领到根节点，不然全局快捷键在这页会收不到事件。
                    let h = this.read(cx).focus_handle.clone();
                    window.focus(&h, cx);
                });

            // 太小的方块放不下文字，索性留白，靠颜色传达信息即可。
            if show_label {
                tile = tile
                    // 底部暗角渐变：不管方块本身冷暖，文字永远压在深色底上，保证可读。
                    .child(
                        div().absolute().inset_0().bg(linear_gradient(
                            180.,
                            linear_color_stop(rgba(0x00000000), 0.0),
                            linear_color_stop(rgba(0x00000099), 1.0),
                        )),
                    )
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom_0()
                            .p(px(4.))
                            .flex()
                            .flex_col()
                            .gap(px(1.))
                            .text_xs()
                            .text_color(rgb(0xf3f5f8))
                            .child(div().overflow_hidden().whitespace_nowrap().child(name))
                            .child(
                                div()
                                    .text_color(rgba(0xffffffa8))
                                    .child(format!("×{} · {:.0}d", entry.commits, entry.days_since)),
                            ),
                    );
            }
            // 最近改动：右上角一颗小圆点，不影响整体边框/网格的干净观感。
            if recent {
                tile = tile.child(
                    div()
                        .absolute()
                        .top(px(4.))
                        .right(px(4.))
                        .size(px(6.))
                        .rounded_full()
                        .bg(rgb(0x4a9eff))
                        .shadow_sm(),
                );
            }
            tile.into_any_element()
        })
        .collect();

    let caption = if total > MAX_TILES {
        format!(
            "改动热力 · 近 {} 天 · 显示热力最高的 {} / 共 {} 个文件 · 🔵 圆点 = 最近 2 天内改动",
            WINDOW_DAYS, MAX_TILES, total
        )
    } else {
        format!(
            "改动热力 · 近 {} 天 · 共 {} 个文件 · 🔵 圆点 = 最近 2 天内改动",
            WINDOW_DAYS, total
        )
    };

    div()
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .child(
            div()
                .px_3()
                .py_2()
                .text_xs()
                .text_color(muted)
                .child(caption),
        )
        .child(
            div()
                .id("hotspot-canvas")
                .flex_1()
                .min_h_0()
                .relative()
                .m_2()
                .children(tiles),
        )
}

impl Workspace {
    /// 确保某 root 的热力图数据缓存新鲜（>20s 或缺失就后台刷新）。`git log --since=90.days`
    /// 比 `git status` 慢得多，缓存窗口相应拉长，避免切换到热力图页就反复重算。
    pub fn ensure_hotspot(&mut self, root: String, cx: &mut Context<Self>) {
        let fresh = self
            .hotspot_data
            .get(&root)
            .is_some_and(|(t, _)| t.elapsed() < std::time::Duration::from_secs(20));
        if fresh || self.hotspot_inflight.contains(&root) {
            return;
        }
        self.hotspot_inflight.insert(root.clone());
        cx.spawn(async move |this, cx| {
            let r = root.clone();
            let entries = cx
                .background_executor()
                .spawn(async move { compute(&r) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.hotspot_inflight.remove(&root);
                this.hotspot_data.insert(root, (Instant::now(), Rc::new(entries)));
                cx.notify();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    // 不用 `use super::*;`：本文件后面会加入 gpui/gpui_component 的 glob 导入，
    // 带进这个测试模块会让 trait 解析图爆炸式增长，`cargo test` 编译期会撞
    // rustc 的递归限制甚至直接崩溃——只导入测试真正用到的这一个名字就够了。
    use super::squarify;

    #[test]
    fn squarify_covers_full_area_and_preserves_order() {
        let weights = vec![4.0, 3.0, 2.0, 1.0];
        let rects = squarify(&weights);
        assert_eq!(rects.len(), 4);
        // 面积总和应约等于 1（浮点误差容忍）。
        let total_area: f32 = rects.iter().map(|r| r.w * r.h).sum();
        assert!((total_area - 1.0).abs() < 0.01, "总面积应约为 1，实际 {total_area}");
        // 权重最大的第一块面积也应最大。
        let areas: Vec<f32> = rects.iter().map(|r| r.w * r.h).collect();
        assert!(areas[0] >= areas[1] && areas[1] >= areas[2] && areas[2] >= areas[3]);
    }

    #[test]
    fn squarify_empty_is_empty() {
        assert!(squarify(&[]).is_empty());
    }

    #[test]
    fn squarify_single_fills_whole_square() {
        let rects = squarify(&[1.0]);
        assert_eq!(rects.len(), 1);
        assert!((rects[0].w - 1.0).abs() < 1e-6);
        assert!((rects[0].h - 1.0).abs() < 1e-6);
    }
}
