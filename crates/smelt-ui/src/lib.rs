//! 共享 GPUI UI 基建：主 GUI 与未来的 `smelt-acp-view` 渲染层共用。跟
//! `smelt-core` 的分工——那边不许引 GPUI，这边允许，但只放「不专属某个具体
//! 页面、被多处复用」的东西，别把整个应用的 UI 都塞进来。

pub mod acp_completion;
pub mod agent_ui_config;
pub mod daemon_states_global;
pub mod markdown_mermaid;
pub mod ui_theme;
