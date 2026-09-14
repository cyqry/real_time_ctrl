//! 初始化状态临时附着到 `Channel` 时使用的类型安全键。
//!
//! 认证分多帧完成，前一帧中的随机数和身份需要保存到同一连接。固定键避免字符串拼写错误和手写
//! `Any` downcast；这些值只属于当前连接，不能当成全局会话表。

use common::channel::ChannelAttributeKey;
pub const CTRL_AUTH_CLIENT_NONCE: ChannelAttributeKey<String> =
    ChannelAttributeKey::new(0x6374_726c_636e_6f6e);
pub const CTRL_AUTH_SERVER_NONCE: ChannelAttributeKey<String> =
    ChannelAttributeKey::new(0x6374_726c_736e_6f6e);
pub const CTRL_ACCOUNT_ID: ChannelAttributeKey<String> =
    ChannelAttributeKey::new(0x6374_726c_6163_6374);
pub const CTRL_INSTANCE_ID: ChannelAttributeKey<String> =
    ChannelAttributeKey::new(0x6374_726c_696e_7374);
pub const CTRL_SESSION_ID: ChannelAttributeKey<String> =
    ChannelAttributeKey::new(0x6374_726c_7365_7373);
pub const KIK_ID: ChannelAttributeKey<String> = ChannelAttributeKey::new(0x6b69_6b5f_6964);
