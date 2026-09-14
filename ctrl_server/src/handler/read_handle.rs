//! 读循环分派入口。
//!
//! `init` 只处理未认证连接；认证完成后 Ctrl、数据和 Kik 分别进入独立模块，使安全状态转换不会和
//! 长耗时命令、文件转发混在同一个函数中。

mod control;
mod data;
mod init;
mod kik;

pub use control::handle_ctrl;
pub use data::{handle_ctrl_data, handle_kik_data};
pub use init::handle_init_message;
pub use kik::handle_kik;
