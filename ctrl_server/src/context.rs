use common::channel::Channel;
use common::kik::Kik;
use std::collections::HashMap;
use std::ops::Index;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, RwLockWriteGuard};

#[derive(Clone)]
//不要直接修改context,Context也可看为一个指针
pub struct Context {
    //todo 最外层优化为原子锁
    //当前控制者和它的数据连接
    ctrl_op: Arc<
        RwLock<
            Option<(
                Option<Arc<Mutex<Channel>>>,
                HashMap<String, Arc<Mutex<Channel>>>,
            )>,
        >,
    >,
    next_ctrl_data: Arc<AtomicU16>,
    now_cmd_id: Arc<RwLock<Option<String>>>,
    //todo 最外层优化为原子锁
    //当前正在控制的kik
    //之所以要在Kik内部加一个arc，是因为会被kik_map变量共享引用，Kik的conn和vec都只能在堆中存在一份，kik_info是可克隆的，conn和vec锁分开是因为他们没有关系
    kik_op: Arc<RwLock<Option<Kik>>>,
    //所有在线的被控端map<String,Kik>
    pub kik_map: Arc<RwLock<HashMap<String, Kik>>>,

    //记录kik生命周期状态
    pub kik_states: Arc<RwLock<HashMap<String, Kik>>>,
}

impl Context {
    pub fn init() -> Self {
        Context {
            ctrl_op: Arc::new(RwLock::new(None)),
            next_ctrl_data: Arc::new(AtomicU16::new(0)),
            now_cmd_id: Arc::new(RwLock::new(None)),
            kik_op: Arc::new(RwLock::new(None)),
            kik_map: Arc::new(RwLock::new(HashMap::new())),
            kik_states: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn find_ctrl_data(&self) -> Option<Arc<Mutex<Channel>>> {
        let next_arc = self.next_ctrl_data.clone();
        let ctrl_arc = self.ctrl_op.clone();
        let ctrl_guard = ctrl_arc.read().await;
        if ctrl_guard.is_some() {
            let data_map = ctrl_guard.clone().unwrap().1;
            if data_map.len() == 0 {
                None
            } else {
                let next = (next_arc.load(Ordering::SeqCst) + 1) % data_map.len() as u16;
                let c = data_map.values().nth(next as usize).unwrap().clone();
                next_arc.store(next + 1, Ordering::SeqCst);
                Some(c)
            }
        } else {
            None
        }
    }

    pub async fn now_cmd_id(&self) -> Option<String> {
        self.now_cmd_id.clone().read().await.clone()
    }
    pub async fn set_now_cmd_id_if_none(&self, id: String) -> bool {
        let arc = self.now_cmd_id.clone();
        let mut now_id = arc.write().await;
        match now_id.as_ref() {
            None => {
                *now_id = Some(id);
                true
            }
            Some(_) => false,
        }
    }
    pub async fn delete_now_cmd_id(&self) {
        *(self.now_cmd_id.clone().write().await) = None;
    }

    pub async fn exist_ctrl(&self) -> bool {
        self.ctrl_op.clone().read().await.is_some()
    }

    pub async fn delete_ctrl_conn(&self) -> Option<Arc<Mutex<Channel>>> {
        let arc = self.ctrl_op.clone();
        let mut guard = arc.write().await;
        if guard.is_some() {
            //由于这个Map只属于这里，只被这里控制，就像直接是一个栈变量，所以可以直接clone
            let (conn_op, data_conns) = guard.clone().unwrap();
            *guard = Some((None, data_conns));
            return conn_op;
        }
        None
    }

    // 执行后 ctrl_op 一定为some,其ctrl_conn也为some
    pub async fn set_ctrl_conn(&self, channel: Arc<Mutex<Channel>>) -> Option<Arc<Mutex<Channel>>> {
        let arc = self.ctrl_op.clone();
        let mut guard = arc.write().await;
        if guard.is_some() {
            let (old_ctrl, old_datas) = guard.clone().unwrap();
            *guard = Some((Some(channel), old_datas));
            old_ctrl
        } else {
            *guard = Some((Some(channel), HashMap::new()));
            None
        }
    }

    pub async fn get_ctrl_conn(&self) -> Option<Arc<Mutex<Channel>>> {
        let arc = self.ctrl_op.clone();
        let guard = arc.read().await;
        match *guard {
            None => None,
            Some((ref ctrl_conn, ref data_conns)) => ctrl_conn.clone(),
        }
    }

    pub async fn delete_ctrl_data_conn(&self, data_conn: Arc<Mutex<Channel>>) {
        match *(self.ctrl_op.clone().write().await) {
            None => {}
            Some((ref ctrl_conn, ref mut data_conns)) => {
                data_conns.remove(data_conn.lock().await.get_id());
            }
        };
    }
    pub async fn insert_ctrl_data_conn(&self, data_conn: Arc<Mutex<Channel>>) {
        match *(self.ctrl_op.clone().write().await) {
            None => {}
            Some((ref ctrl_conn, ref mut data_conns)) => {
                data_conns.insert(
                    data_conn.clone().lock().await.get_id().to_string(),
                    data_conn,
                );
            }
        };
    }

    pub async fn offline_kik(&self, kik_id: &str) {
        if let Some(kik) = self.get_kik().await {
            if kik.kik_info.id.clone().unwrap() == kik_id {
                *(self.kik_op.clone().write().await) = None;
            }
        }
        let kik = self.kik_map.clone().write().await.remove(kik_id);
        if kik.is_some() {
            kik.unwrap().clear().await;
        }
    }

    //清理对应id kik的 kik_conn,
    pub async fn delete_kik_conn_if_id(&self, id: &str) {
        //先从 kik_op找
        {
            let arc = self.kik_op.clone();
            let guard = arc.write().await;
            if guard.is_some() {
                if guard.clone().unwrap().kik_info.id.unwrap() == id {
                    guard.clone().unwrap().delete_kik_conn().await;
                    return;
                }
            }
        }
        //再去map中找
        let arc = self.kik_map.clone();
        let kik_map = arc.read().await;
        let option = kik_map.get(id);
        if option.is_some() {
            option.unwrap().delete_kik_conn().await;
        }
        //判断map和 op是否为0和None，是的话自动删除map中的Kik,说明其彻底下线
    }

    pub async fn delete_kik_data_conn(&self, data_conn: Arc<Mutex<Channel>>) {
        //先从 kik_op中找
        {
            let kik_op = self.kik_op.clone().read().await.clone();
            if kik_op.is_some() {
                kik_op.unwrap().delete_data_conn(data_conn.clone()).await;
            }
        }
        //再从map中找
        //不要与下面写在一行，因为引用传递导致的生命周期问题或者match的一个生命周期问题，所不会getid了就释放，然后在match中 delete时又lock了所以死锁
        let kik_id = data_conn
            .clone()
            .lock()
            .await
            .get::<String>("kik_id")
            .expect("kik data conn必需要有kik_id!!").to_string();
        match self.kik_map.clone().read().await.get(kik_id.as_str()) {
            None => {}
            Some(kik) => {
                kik.delete_data_conn(data_conn).await;
            }
        };
    }

    pub async fn set_kik(&self, kik: Kik) {
        *(self.kik_op.clone().write().await) = Some(kik);
    }

    pub async fn get_kik(&self) -> Option<Kik> {
        self.kik_op.clone().read().await.clone()
    }

    pub async fn kiks_emtpy(&self) -> bool {
        self.kik_map.clone().read().await.is_empty()
    }


    pub(crate) fn set_kik_state(&self) {
        todo!()
    }
}
