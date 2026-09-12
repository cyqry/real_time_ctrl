use common::channel::ChannelAttributeKey;
use common::message::kik_resp::KikResp;
use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::Mutex;

pub type KikResponse = (KikResp, String);
pub type KikResponseSender = Sender<KikResponse>;
pub type KikResponseReceiver = Arc<Mutex<Receiver<KikResponse>>>;

pub const CTRL_AUTH_CLIENT_NONCE: ChannelAttributeKey<String> =
    ChannelAttributeKey::new(0x6374_726c_636e_6f6e);
pub const CTRL_AUTH_SERVER_NONCE: ChannelAttributeKey<String> =
    ChannelAttributeKey::new(0x6374_726c_736e_6f6e);
pub const KIK_ID: ChannelAttributeKey<String> = ChannelAttributeKey::new(0x6b69_6b5f_6964);
pub const KIK_RESPONSE_TX: ChannelAttributeKey<KikResponseSender> =
    ChannelAttributeKey::new(0x6b69_6b5f_7274_7801);
pub const KIK_RESPONSE_RX: ChannelAttributeKey<KikResponseReceiver> =
    ChannelAttributeKey::new(0x6b69_6b5f_7272_7802);
