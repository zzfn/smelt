//! GUI（workspace）与守护（smeltd / gateway）共用的无 UI 逻辑。
//!
//! 历史上这些模块散在根 crate 的 src/ 下、靠 `#[path]` 编进各二进制，代价是每个
//! bin 重复编译一遍、dead_code 误报、依赖边界全靠自觉。收进 lib 后由编译器守边界：
//! 本 crate 不许出现 GPUI 依赖。

pub mod acp_chat;
pub mod acp_conn;
pub mod agent_kind;
pub mod agent_status;
pub mod block_on;
pub mod claude_paths;
pub mod daemon_state;
pub mod font_config;
pub mod json_store;
pub mod login_env;
pub mod osc;
pub mod permission_menu;
pub mod remote_gateway;
pub mod term_text;
pub mod title_spinner;
pub mod workspace_override;
