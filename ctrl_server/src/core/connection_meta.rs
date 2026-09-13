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
