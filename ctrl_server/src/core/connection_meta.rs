use common::channel::ChannelAttributeKey;
use common::message::kik_resp::KikResp;
use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::Mutex;

pub type KikResponse = (KikResp, String);
pub type KikResponseSender = Sender<KikResponse>;
pub type KikResponseReceiver = Arc<Mutex<Receiver<KikResponse>>>;

pub const CTRL_AUTH_CLIENT_NONCE: ChannelAttributeKey<String> =
    ChannelAttributeKey::new("ctrl_auth_v2_client_nonce");
pub const CTRL_AUTH_SERVER_NONCE: ChannelAttributeKey<String> =
    ChannelAttributeKey::new("ctrl_auth_v2_server_nonce");
pub const KIK_ID: ChannelAttributeKey<String> = ChannelAttributeKey::new("kik_id");
pub const KIK_RESPONSE_TX: ChannelAttributeKey<KikResponseSender> =
    ChannelAttributeKey::new("kik_response_tx");
pub const KIK_RESPONSE_RX: ChannelAttributeKey<KikResponseReceiver> =
    ChannelAttributeKey::new("kik_response_rx");
