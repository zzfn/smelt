//! 文件树 + 文件内容查看/编辑面板：目录浏览、展开/收起、打开文件、Cmd+S 保存、
//! 项目内搜索（文件名 + 内容）。
//!
//! 跟 git_panel.rs 同一个套路：从 main.rs 拆出来的 `impl Workspace` 方法 + 独立
//! 渲染/搜索函数，字段仍然声明在 main.rs 的 `Workspace` struct 里。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

use gpui::*;
use gpui::prelude::FluentBuilder;
use gpui::InteractiveElement;
use gpui_component::input::Input;
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::scroll::ScrollableElement;
use gpui_component::text::TextView;
use gpui_component::tooltip::Tooltip;
use gpui_component::*;

use crate::{placeholder_view, SendSelectionToTerminal, Workspace};

// ===================== 类型 =====================

/// 打开查看的文件：路径 + 可编辑的代码编辑器状态（gpui-component 的 Editor：
/// tree-sitter 语法高亮 + 行号 + 搜索，直接可编辑，不再是只读预览）。
pub struct OpenFile {
    pub path: String,
    editor: Entity<gpui_component::input::InputState>,
    /// 磁盘上（或最近一次保存后）的内容快照，跟编辑器当前内容一比就知道是否有未保存
    /// 改动——不用额外订阅 InputEvent::Change 维护一个脏标记，render 时比一下字符串就行。
    saved_content: Rc<String>,
    /// 最近一次保存失败 / 不允许保存的原因；成功保存或重新打开文件后清空。
    save_error: Option<String>,
    /// 文件是否按文本成功读取过。读取完成前 / 读取失败（比如二进制文件）时为 false，
    /// 禁止保存——避免误按 Cmd+S 把「无法读取」占位文案写回去覆盖了原文件。
    readable: bool,
    /// 上次保存时检测到磁盘内容跟 saved_content 对不上（外部改过）。为 true 时
    /// 再按一次 Cmd+S 会跳过冲突检查强制覆盖——用"再按一次"当作用户已确认覆盖。
    conflict_pending: bool,
    /// markdown 文件的「预览」开关（仅 .md 生效，见 file_content_pane）；切换打开的
    /// 文件不带过去，open_file_now 每次都重置为 false（默认进编辑视图）。
    preview: bool,
}

/// 保存一次的结果：分 Saved / 检测到外部改动的 Conflict / 其它 IO 错误。
enum SaveOutcome {
    Saved,
    Conflict,
    Error(String),
}

/// 文件树搜索的一条命中。
struct SearchHit {
    /// 命中文件的绝对路径（点击时用它 view_file）。
    path: String,
    /// 相对项目根的展示路径。
    rel: String,
    /// 内容命中时的首个匹配行：(行号从 1 起, 该行文本预览)；仅文件名命中时为 None。
    line: Option<(usize, String)>,
}

/// 文件树搜索的一次结果快照。后台遍历项目填充，render 只读。
pub struct SearchState {
    /// 触发本次结果的查询串（用于判断是否需要重跑）。
    query: String,
    /// 后台遍历是否已跑完（false 时列表顶部显示「搜索中…」）。
    done: bool,
    /// 命中列表（文件名命中在前、内容命中在后，各自按路径序）。
    hits: Vec<SearchHit>,
    /// 是否因命中数触顶而截断（列表底部提示还有更多）。
    truncated: bool,
}

/// 搜索命中数上限：触顶即停并标记截断，避免超大仓遍历/渲染失控。
const SEARCH_HIT_LIMIT: usize = 200;
/// 内容搜索跳过的单文件大小上限（512KB）：更大的多半是数据/构建产物，逐行扫不划算。
const SEARCH_MAX_FILE_BYTES: u64 = 512 * 1024;

// ===================== 搜索 =====================

/// 后台遍历项目搜索 query（大小写不敏感）：文件名命中或文件内容逐行命中。
/// 跳过 .git/node_modules/target/.DS_Store、隐藏目录、大文件与二进制（含 NUL 字节）。
/// 返回 (命中列表, 是否因触顶截断)；文件名命中排在内容命中前。绝不在此之外做 UI 调用。
fn search_project(root: &str, query: &str) -> (Vec<SearchHit>, bool) {
    let needle = query.to_lowercase();
    let mut name_hits: Vec<SearchHit> = Vec::new();
    let mut content_hits: Vec<SearchHit> = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = vec![std::path::PathBuf::from(root)];
    let root_path = std::path::Path::new(root);
    let mut truncated = false;

    'outer: while let Some(dir) = stack.pop() {
        let mut entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(&dir) {
            Ok(rd) => rd.flatten().collect(),
            Err(_) => continue,
        };
        // 目录序稳定：按名字排序，命中列表才不会每次遍历顺序抖动。
        entries.sort_by_key(|e| e.file_name().to_string_lossy().to_lowercase());
        for e in entries {
            let name = e.file_name().to_string_lossy().to_string();
            // 排除规则与 ensure_dir_listing 对齐，另跳过所有隐藏文件/目录。
            if matches!(name.as_str(), ".git" | "node_modules" | "target" | ".DS_Store")
                || name.starts_with('.')
            {
                continue;
            }
            let path = e.path();
            let is_dir = path.is_dir();
            if is_dir {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(root_path)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            let abs = path.to_string_lossy().to_string();
            // 文件名命中：直接记一条（不再看内容），命中行留空。
            if name.to_lowercase().contains(&needle) {
                name_hits.push(SearchHit { path: abs, rel, line: None });
                if name_hits.len() + content_hits.len() >= SEARCH_HIT_LIMIT {
                    truncated = true;
                    break 'outer;
                }
                continue;
            }
            // 内容命中：跳过大文件；读文本失败（二进制/非 UTF-8）则跳过。
            if e.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > SEARCH_MAX_FILE_BYTES {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            // 含 NUL 视为二进制，不逐行扫。
            if text.as_bytes().contains(&0) {
                continue;
            }
            if let Some((no, line)) = text
                .lines()
                .enumerate()
                .find(|(_, l)| l.to_lowercase().contains(&needle))
            {
                // 预览行去掉首尾空白并截断，避免超长行撑爆列表。
                let preview: String = line.trim().chars().take(200).collect();
                content_hits.push(SearchHit {
                    path: abs,
                    rel,
                    line: Some((no + 1, preview)),
                });
                if name_hits.len() + content_hits.len() >= SEARCH_HIT_LIMIT {
                    truncated = true;
                    break 'outer;
                }
            }
        }
    }

    name_hits.extend(content_hits);
    (name_hits, truncated)
}

/// 文件树搜索结果视图：扁平命中列表（替代 query 非空时的树形浏览）。
/// 每项显示相对路径 + 内容命中行预览，点击用 view_file 打开该文件。
pub fn search_results_view(
    state: &SearchState,
    scroll: &ScrollHandle,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let (muted, fg, hover, accent) = {
        let t = cx.theme();
        (t.muted_foreground, t.foreground, t.accent, t.primary)
    };
    // 顶栏状态：搜索中 / 无结果 / N 项命中(是否截断)。
    let status = if !state.done {
        "搜索中…".to_string()
    } else if state.hits.is_empty() {
        "无匹配".to_string()
    } else if state.truncated {
        format!("命中 {}+ 项（已截断）", state.hits.len())
    } else {
        format!("命中 {} 项", state.hits.len())
    };

    let this = cx.entity();
    let rows: Vec<AnyElement> = state
        .hits
        .iter()
        .enumerate()
        .map(|(i, hit)| {
            let this = this.clone();
            let p = hit.path.clone();
            // 拆出目录前缀与文件名：文件名高亮、目录弱化，便于扫读。
            let (dir_part, name_part) = match hit.rel.rfind('/') {
                Some(idx) => (hit.rel[..=idx].to_string(), hit.rel[idx + 1..].to_string()),
                None => (String::new(), hit.rel.clone()),
            };
            let preview = hit.line.clone();
            div()
                .id(("search-hit", i))
                .flex()
                .flex_col()
                .gap(px(1.0))
                .px_2()
                .py(px(2.0))
                .hover(move |s| s.bg(hover))
                .on_click(move |_ev, window, cx| {
                    this.update(cx, |ws, cx| ws.view_file(p.clone(), window, cx));
                })
                // 第一行：目录（弱）+ 文件名（强）。
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_sm()
                        .child(Icon::new(IconName::File).size(px(13.)).text_color(muted))
                        .child(div().text_color(muted).child(dir_part))
                        .child(div().text_color(fg).child(name_part)),
                )
                // 第二行：内容命中的行号 + 行预览（仅内容命中时有）。
                .children(preview.map(|(no, text)| {
                    div()
                        .flex()
                        .gap_1()
                        .pl(px(18.))
                        .text_xs()
                        .text_color(muted)
                        .child(div().text_color(accent).child(format!("{no}")))
                        .child(div().min_w_0().child(text))
                }))
                .into_any_element()
        })
        .collect();

    div()
        .id("search-results")
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .child(
            div()
                .px_2()
                .py_1()
                .text_xs()
                .text_color(muted)
                .child(status),
        )
        .child(
            div()
                .id("search-results-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .pb_1()
                .track_scroll(scroll)
                .vertical_scrollbar(scroll)
                .children(rows),
        )
        .into_any_element()
}

// ===================== 目录树 =====================

/// 只读缓存的递归收集目录条目（仅进入已展开且已缓存的文件夹）；绝不做任何 fs 调用。
/// 展开了但尚未缓存的目录会被跳过——render 每帧检查并后台补齐，下一帧自动出现。
fn walk_dir_cached(
    dir: &str,
    dir_cache: &HashMap<String, (Instant, Rc<Vec<(String, bool)>>)>,
    expanded: &HashSet<String>,
    depth: usize,
    out: &mut Vec<(usize, String, bool, String, bool)>,
) {
    let Some((_, entries)) = dir_cache.get(dir) else { return };
    for (name, is_dir) in entries.iter() {
        let path = Path::new(dir).join(name).to_string_lossy().to_string();
        let is_expanded = expanded.contains(&path);
        out.push((depth, name.clone(), *is_dir, path.clone(), is_expanded));
        if *is_dir && is_expanded {
            walk_dir_cached(&path, dir_cache, expanded, depth + 1, out);
        }
    }
}

/// 文件树视图：只读目录列表缓存渲染（ensure_dir_listing 后台刷新，绝不在这里碰
/// 文件系统），已展开的文件夹递归显示，点击文件夹展开/收起、点击文件打开。
///
/// 未用 uniform_list 虚拟滚动：实测它对这里的行内容（含 Icon）的孤立测量会算出
/// 异常偏大的行高，导致可视区间被判定只能塞下 1 行——已用隔离实验定位到具体是
/// uniform_list 的度量逻辑而非容器高度链的问题。文件树条目量级远小于 git diff，
/// 虚拟滚动只是锦上添花而非必需，故改走普通可滚动列表（与 git-files 同款写法），
/// 优先保证正确显示；虚拟滚动作为后续可选优化记在 docs/roadmap.md。
pub fn file_tree(
    cwd: Option<String>,
    expanded: &HashSet<String>,
    dir_cache: &HashMap<String, (Instant, Rc<Vec<(String, bool)>>)>,
    scroll: &ScrollHandle,
    open_path: Option<&str>,
    // 当前 git status 的改动文件列表（(porcelain 状态码, 相对 root 的路径)）：不认识
    // GitStatusData 本身，main.rs 转手把 `&d.files` 传进来即可，只在这一处标红点用。
    changed_files: Option<&[(String, String)]>,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let (muted, fg, hover, active_bg, warning) = {
        let t = cx.theme();
        (t.muted_foreground, t.foreground, t.accent, t.border, t.warning)
    };
    let Some(root) = cwd else {
        return placeholder_view("无项目目录", muted).into_any_element();
    };
    if !dir_cache.contains_key(&root) {
        // 首次进入该项目：ensure_dir_listing 已在 render 顶部触发，下一帧就有数据。
        return placeholder_view("加载中…", muted).into_any_element();
    }

    // 每行预先算好展开状态。
    let mut flat: Vec<(usize, String, bool, String, bool)> = Vec::new();
    walk_dir_cached(&root, dir_cache, expanded, 0, &mut flat);

    let this = cx.entity();
    let rows: Vec<AnyElement> = flat
        .into_iter()
        .enumerate()
        .map(|(i, (depth, name, is_dir, path, is_expanded))| {
            let indent = px(8.0 + depth as f32 * 14.0);
            // 展开箭头：目录用 chevron（展开朝下 / 收起朝右），文件留等宽占位对齐。
            let arrow = if is_dir {
                div()
                    .w(px(14.))
                    .flex()
                    .justify_center()
                    .child(
                        Icon::new(if is_expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(px(12.))
                        .text_color(muted),
                    )
                    .into_any_element()
            } else {
                div().w(px(14.)).into_any_element()
            };
            // 类型图标：目录（展开 / 收起用不同文件夹图标）与文件区分。
            let type_icon = Icon::new(if is_dir {
                if is_expanded {
                    IconName::FolderOpen
                } else {
                    IconName::Folder
                }
            } else {
                IconName::File
            })
            .size(px(14.))
            .text_color(if is_dir { fg } else { muted });
            let this = this.clone();
            let p = path.clone();
            let this_menu = this.clone();
            let p_menu = p.clone();
            // 当前在右侧内容面板打开的文件：文件树里对应行常驻高亮，不用靠记忆去找。
            let is_open = !is_dir && open_path == Some(path.as_str());
            // 有未提交改动的文件标个小红点：path 是 "{root}/..." 绝对路径，git status
            // 记的是相对 root 的路径，去掉前缀比一下就知道。只标文件，不往目录上冒泡
            // （冒泡要额外一趟遍历，且文件树本来就是按需展开，价值不大）。
            let is_changed = !is_dir
                && changed_files.is_some_and(|files| {
                    Path::new(&path)
                        .strip_prefix(&root)
                        .ok()
                        .and_then(|rel| rel.to_str())
                        .is_some_and(|rel| files.iter().any(|(_, p)| p == rel))
                });
            let name_tip: SharedString = name.clone().into();
            div()
                .id(("file", i))
                .flex()
                .items_center()
                .gap_1()
                .pl(indent)
                .pr_2()
                .py(px(1.0))
                .text_sm()
                .text_color(if is_dir { fg } else { muted })
                .when(is_open, |el| el.bg(active_bg))
                .hover(move |s| s.bg(hover))
                .on_click(move |_ev, window, cx| {
                    this.update(cx, |ws, cx| {
                        if is_dir {
                            ws.toggle_expand(p.clone(), cx);
                        } else {
                            ws.view_file(p.clone(), window, cx);
                        }
                    });
                })
                // 文件名可能比这一列宽：截断加省略号，hover 用 tooltip 补全名（否则超长
                // 文件名会视觉溢出到右侧内容面板，既不好看也读不全）。必须在
                // context_menu 之前挂：context_menu 把元素包成 ContextMenu<E>，
                // 不再实现 tooltip 所在的 StatefulInteractiveElement。
                .tooltip(move |window, cx| Tooltip::new(name_tip.clone()).build(window, cx))
                .context_menu(move |menu, _window, _cx| {
                    let this = this_menu.clone();
                    let p = p_menu.clone();
                    menu.item(
                        PopupMenuItem::new("发送到终端").on_click(move |_ev, _window, cx| {
                            this.update(cx, |ws, cx| ws.send_path_to_terminal(p.clone(), cx));
                        }),
                    )
                })
                .child(arrow)
                .child(type_icon)
                .child(div().flex_1().min_w_0().truncate().child(name))
                .when(is_changed, |el| {
                    el.child(div().flex_none().size(px(6.)).rounded_full().bg(warning))
                })
                .into_any_element()
        })
        .collect();

    div()
        .id("file-tree")
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .py_1()
        .track_scroll(scroll)
        .vertical_scrollbar(scroll)
        .children(rows)
        .into_any_element()
}

// ===================== 文件内容面板 =====================

/// 文件扩展名 → Editor 的语法高亮语言名。gpui-component 的 `Language::from_name`
/// 本身就认常见扩展名（"rs"/"py"/"md" 等），这里只需把扩展名传过去；识别不了的
/// 名字组件会自动回退成纯文本，不会 panic。没有扩展名的文件（Makefile 等）退而
/// 用文件名本身（能命中 "makefile" 这类按文件名匹配的语言）。
fn editor_language_for_path(path: &str) -> String {
    let p = Path::new(path);
    match p.extension().and_then(|e| e.to_str()) {
        Some(ext) => ext.to_lowercase(),
        None => p.file_name().and_then(|n| n.to_str()).unwrap_or("text").to_lowercase(),
    }
}

/// 文件内容查看/编辑面板：直接用 gpui-component 的 Editor（InputState code_editor
/// 模式），自带语法高亮、行号、搜索、大文件下的增量编辑，不用再自己管虚拟滚动。
pub fn file_content_pane(open_file: &Option<OpenFile>, cx: &mut Context<Workspace>) -> Div {
    let (muted, fg, border, warning, accent) = {
        let t = cx.theme();
        (t.muted_foreground, t.foreground, t.border, t.warning, t.accent)
    };
    match open_file {
        None => placeholder_view("← 从左侧选择文件查看内容", muted),
        Some(of) => {
            let name = of.path.rsplit('/').next().unwrap_or(of.path.as_str()).to_string();
            let dirty = of.editor.read(cx).value().to_string() != *of.saved_content;
            // 只有 markdown 才给「编辑 / 预览」切换，其它文件类型没有预览这一说。
            let is_md = editor_language_for_path(&of.path) == "md";
            let preview = of.preview && is_md;
            let header = h_flex()
                .items_center()
                .justify_between()
                .gap_2()
                .px_3()
                .py_1()
                .border_b_1()
                .border_color(border)
                .child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .child(div().text_sm().text_color(muted).child(name))
                        // 未保存改动：文件名后一个小圆点，Cmd+S 保存后消失。
                        .when(dirty, |el| {
                            el.child(div().size(px(6.)).rounded_full().bg(warning))
                        })
                        // 保存失败 / 不支持保存的提示。
                        .children(of.save_error.clone().map(|msg| {
                            div().text_xs().text_color(warning).child(msg)
                        })),
                )
                .when(is_md, |el| {
                    let seg = |label: &'static str, active: bool, target: bool| {
                        div()
                            .id(label)
                            .px_2()
                            .py(px(2.))
                            .rounded(px(6.))
                            .text_xs()
                            .cursor_pointer()
                            .when(active, |el| el.bg(accent.opacity(0.15)).text_color(fg))
                            .when(!active, |el| el.text_color(muted))
                            .child(label)
                            .on_click(cx.listener(move |ws, _ev, _window, cx| {
                                ws.set_file_preview(target, cx)
                            }))
                    };
                    el.child(
                        h_flex()
                            .gap_1()
                            .p(px(2.))
                            .rounded(px(8.))
                            .bg(border.opacity(0.3))
                            .child(seg("编辑", !preview, false))
                            .child(seg("预览", preview, true)),
                    )
                });
            let body: AnyElement = if preview {
                div()
                    .id("md-preview")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_3()
                    .child(
                        div().text_sm().text_color(fg).child(TextView::markdown(
                            "md-preview-body",
                            of.editor.read(cx).value().to_string(),
                        )),
                    )
                    .into_any_element()
            } else {
                div()
                    .flex_1()
                    .min_h_0()
                    .child(
                        // 自定义 context_menu 在 InputState 自身的右键事件回调里执行，此时
                        // 该 entity 正处于 update 中——绝不能在这里 editor.read(cx)，否则
                        // 触发 gpui 的重入借用 panic（在 FFI 边界不可 unwind，直接 abort
                        // 崩整个 App）。剪切/复制/发送都在真正执行时（Cut/Copy 的默认实现、
                        // send_open_file_selection）各自判空早退，这里不需要提前查询选中状态
                        // 来控制 disabled，牺牲一点「没选中时置灰」的观感换取不崩。
                        Input::new(&of.editor).h_full().context_menu(move |menu, _window, cx| {
                            let has_paste = cx.read_from_clipboard().is_some();
                            menu.menu("剪切", Box::new(gpui_component::input::Cut))
                                .menu("复制", Box::new(gpui_component::input::Copy))
                                .menu_with_disabled(
                                    "粘贴",
                                    !has_paste,
                                    Box::new(gpui_component::input::Paste),
                                )
                                .separator()
                                .menu("全选", Box::new(gpui_component::input::SelectAll))
                                .separator()
                                .menu("发送选中内容到终端", Box::new(SendSelectionToTerminal))
                        }),
                    )
                    .into_any_element()
            };
            div()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .flex_col()
                .child(header)
                .child(body)
        }
    }
}

// ===================== Workspace 方法 =====================

impl Workspace {
    /// 文件内容面板右上角「编辑 / 预览」切换（仅 markdown 生效）。
    fn set_file_preview(&mut self, preview: bool, cx: &mut Context<Self>) {
        if let Some(of) = self.open_file.as_mut() {
            of.preview = preview;
        }
        cx.notify();
    }

    /// 文件树：展开/收起一个文件夹。
    pub fn toggle_expand(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&path) {
            self.expanded.insert(path);
        }
        cx.notify();
    }

    /// 文件树：打开一个文件查看/编辑内容。当前文件有未保存改动时不直接切换——先弹
    /// 确认弹窗（见 pending_file_switch / render_unsaved_file_confirm），用户选了
    /// "不保存"或"保存并切换"才真正调用 open_file_now。
    pub fn view_file(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = self
            .open_file
            .as_ref()
            .is_some_and(|of| of.editor.read(cx).value().to_string() != *of.saved_content);
        if dirty {
            self.pending_file_switch = Some(path);
            cx.notify();
            return;
        }
        self.open_file_now(path, window, cx);
    }

    /// 实际打开文件：用 gpui-component 的 Editor（InputState 的 code_editor 模式）：
    /// tree-sitter 语法高亮 + 行号 + 搜索，直接可编辑，Cmd+S（见 save_open_file）能
    /// 存回磁盘。读文件本身放到后台线程跑（大文件不卡 UI），读完回主线程灌进编辑器；
    /// 用自增 file_gen 丢弃过期结果（期间又切了别的文件）。
    pub fn open_file_now(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        use gpui_component::input::InputState;

        self.file_gen = self.file_gen.wrapping_add(1);
        let gen = self.file_gen;

        let language = editor_language_for_path(&path);
        let editor = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor(language)
                .line_number(true)
                .searchable(true)
                // 超长行横向滚动而不是自动换行——代码这种东西换行会破坏缩进对齐，
                // 多行输入默认开软换行，这里显式关掉。
                .soft_wrap(false)
        });
        self.open_file = Some(OpenFile {
            path: path.clone(),
            editor: editor.clone(),
            saved_content: Rc::new(String::new()),
            save_error: None,
            readable: false, // 读完确认是文本才翻真，防止读取完成前误按 Cmd+S
            conflict_pending: false,
            preview: false,
        });
        cx.notify();

        cx.spawn(async move |this, cx| {
            let p = path.clone();
            let read = cx.background_executor().spawn(async move { std::fs::read_to_string(&p) }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                // 只有当前仍是这次打开的文件才写入，避免旧任务覆盖新文件。
                if this.file_gen != gen {
                    return;
                }
                let Some(of) = this.open_file.as_mut() else { return };
                match read {
                    Ok(content) => {
                        editor.update(cx, |state, cx| {
                            state.set_value(content.clone(), window, cx);
                        });
                        of.saved_content = Rc::new(content);
                        of.readable = true;
                    }
                    Err(_) => {
                        editor.update(cx, |state, cx| {
                            state.set_value(
                                "（无法以文本方式读取：可能是二进制文件）",
                                window,
                                cx,
                            );
                        });
                        of.readable = false;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Cmd+S：把当前打开文件的编辑器内容写回磁盘（仅 Files 页触发，见 on_key_down）。
    /// 写之前先读一次磁盘现状跟 saved_content 比对——不一样说明文件被外部改过，
    /// 这次先不写、把 conflict_pending 置位提示用户；用户再按一次 Cmd+S 就当作
    /// 已确认覆盖，跳过这次检查直接写。写文件本身放后台线程；成功后把
    /// saved_content 同步成刚写的内容（清掉"未保存"标记 + 错误提示），并且如果这
    /// 次保存是「保存并切换」触发的，顺带打开 pending_switch_after_save 里存的目标
    /// 文件；保存失败或起冲突则放弃这次切换，留在当前文件上让用户处理。
    pub fn save_open_file(&mut self, cx: &mut Context<Self>) {
        let Some(of) = &self.open_file else { return };
        if !of.readable {
            if let Some(of) = self.open_file.as_mut() {
                of.save_error = Some("此文件未能正常读取为文本，不支持保存".to_string());
            }
            self.pending_switch_after_save = None;
            cx.notify();
            return;
        }
        let path = of.path.clone();
        let content = of.editor.read(cx).value().to_string();
        // Rc<String> 不是 Send，进不了 background_executor；克隆成普通 String 再带过去。
        let expected_on_disk = (*of.saved_content).clone();
        let force = of.conflict_pending;
        let gen = self.file_gen;

        cx.spawn(async move |this, cx| {
            let check_path = path.clone();
            let write_content = content.clone();
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    if !force {
                        if let Ok(current) = std::fs::read_to_string(&check_path) {
                            if current != expected_on_disk {
                                return SaveOutcome::Conflict;
                            }
                        }
                    }
                    match std::fs::write(&check_path, write_content) {
                        Ok(()) => SaveOutcome::Saved,
                        Err(e) => SaveOutcome::Error(e.to_string()),
                    }
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.file_gen != gen {
                    return; // 写盘期间又切了别的文件，这次结果不再相关
                }
                let switch_target = this.pending_switch_after_save.take();
                let Some(of) = this.open_file.as_mut() else { return };
                match outcome {
                    SaveOutcome::Saved => {
                        of.saved_content = Rc::new(content);
                        of.save_error = None;
                        of.conflict_pending = false;
                        if let Some(target) = switch_target {
                            this.open_file_now(target, window, cx);
                        }
                    }
                    SaveOutcome::Conflict => {
                        of.conflict_pending = true;
                        of.save_error =
                            Some("文件已被外部修改；再按一次 Cmd+S 会强制覆盖磁盘上的改动".to_string());
                    }
                    SaveOutcome::Error(e) => of.save_error = Some(format!("保存失败：{e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 文件树搜索：按 query 匹配文件名 + 文件内容，后台遍历项目、命中写回 search_results。
    /// 与 view_file 同款「background_executor + 自增 gen 丢弃过期结果」模式，绝不阻塞 render。
    /// query 未变（已有对应结果或正在跑同一 query）就跳过，避免每帧重扫。
    pub fn ensure_search(&mut self, root: String, query: String, cx: &mut Context<Self>) {
        // 已有本 query 的结果、或正有一次针对本 query 的遍历在跑，就不重复触发。
        if self.search_results.as_ref().is_some_and(|s| s.query == query) {
            return;
        }
        self.search_gen = self.search_gen.wrapping_add(1);
        let gen = self.search_gen;
        // 先占位：done=false 让列表顶部显示「搜索中…」，遍历完成后替换。
        self.search_results = Some(SearchState {
            query: query.clone(),
            done: false,
            hits: Vec::new(),
            truncated: false,
        });
        cx.notify();

        cx.spawn(async move |this, cx| {
            let (r, q) = (root.clone(), query.clone());
            let (hits, truncated) = cx
                .background_executor()
                .spawn(async move { search_project(&r, &q) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // 只有仍是最新一次搜索才写入，丢弃期间被新查询取代的过期结果。
                if this.search_gen == gen {
                    this.search_results = Some(SearchState {
                        query,
                        done: true,
                        hits,
                        truncated,
                    });
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 确保某目录的直接子项列表缓存新鲜（>2s 或缺失就后台刷新）。
    /// 绝不阻塞 render：此前 file_tree 在 render 里同步 fs::read_dir，大目录会
    /// 像 git status 那样掉帧，这里挪到后台执行器 + 缓存，render 只读。
    pub fn ensure_dir_listing(&mut self, dir: String, cx: &mut Context<Self>) {
        let fresh = self
            .dir_cache
            .get(&dir)
            .is_some_and(|(t, _)| t.elapsed() < std::time::Duration::from_millis(2000));
        if fresh || self.dir_inflight.contains(&dir) {
            return;
        }
        self.dir_inflight.insert(dir.clone());
        cx.spawn(async move |this, cx| {
            let d = dir.clone();
            let entries = cx
                .background_executor()
                .spawn(async move {
                    let mut items: Vec<std::fs::DirEntry> = match std::fs::read_dir(&d) {
                        Ok(rd) => rd.flatten().collect(),
                        Err(_) => return Vec::new(),
                    };
                    items.sort_by_key(|e| {
                        (
                            !e.path().is_dir(),
                            e.file_name().to_string_lossy().to_lowercase(),
                        )
                    });
                    items
                        .into_iter()
                        .filter_map(|e| {
                            let name = e.file_name().to_string_lossy().to_string();
                            if matches!(name.as_str(), ".git" | "node_modules" | "target" | ".DS_Store")
                            {
                                return None;
                            }
                            Some((name, e.path().is_dir()))
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.dir_inflight.remove(&dir);
                this.dir_cache.insert(dir, (Instant::now(), Rc::new(entries)));
                cx.notify();
            });
        })
        .detach();
    }

    /// 文件树右键「发送到终端」：把路径转成相对当前 cwd 的 @提及，写进当前激活终端
    /// 的 PTY（不带回车，同 send_diff_comments 的做法）。
    pub fn send_path_to_terminal(&mut self, path: String, cx: &mut Context<Self>) {
        let root = self.cur().and_then(|s| s.cwd(cx));
        let rel = root
            .and_then(|root| {
                Path::new(&path).strip_prefix(&root).ok().map(|p| p.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| path.clone());
        let msg = format!("@{rel} ");
        if let Some(view) = self.cur().map(|s| s.active.clone()) {
            view.update(cx, |tv, cx| tv.send_text(&msg, cx));
        }
    }

    /// 文件内容框选右键「发送选中内容到终端」：带上文件名 + 选中文字，写进当前激活
    /// 终端的 PTY（不带回车）。
    pub fn send_open_file_selection(&mut self, cx: &mut Context<Self>) {
        let Some(of) = &self.open_file else { return };
        let selected = of.editor.read(cx).selected_value().to_string();
        if selected.trim().is_empty() {
            return;
        }
        let root = self.cur().and_then(|s| s.cwd(cx));
        let rel = root
            .and_then(|root| {
                Path::new(&of.path).strip_prefix(&root).ok().map(|p| p.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| of.path.clone());
        let msg = format!("{rel} 里选中的这段：\n```\n{selected}\n```\n");
        if let Some(view) = self.cur().map(|s| s.active.clone()) {
            view.update(cx, |tv, cx| tv.send_text(&msg, cx));
        }
    }
}
