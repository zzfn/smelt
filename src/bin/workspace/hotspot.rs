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

#[cfg(test)]
mod tests {
    use super::*;

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
