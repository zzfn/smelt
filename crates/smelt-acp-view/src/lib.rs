//! ACP 会话的消息流视图：第二种会话类型的 GPUI 渲染层，独立成 crate（原
//! `crates/smelt/src/acp_view.rs`）。连接层是 `smelt_core::acp_conn`（不含
//! GPUI，为 smeltd 未来托管 ACP 会话铺路）；这里持 `AcpHandle` 消费事件，画
//! 消息流 + 权限审批卡片 + 输入框，共享 UI 基建（色板/markdown/补全）来自
//! `smelt-ui`。

pub mod acp_view;
pub use acp_view::*;
