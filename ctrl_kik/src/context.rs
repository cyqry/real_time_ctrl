use crate::read_handle;
use anyhow::{anyhow, Context as AnyhowContext, Error};
use bytes::{BufMut, BytesMut};
use common::channel::Channel;
use common::command::Command;
use common::message::kik_frame::KikFrame;
use common::protocol;
use common::protocol::BufSerializable;
use log::debug;
use std::collections::HashMap;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU16, Ordering};
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::{
    channel, unbounded_channel, Receiver, Sender, UnboundedReceiver, UnboundedSender,
};
use tokio::sync::{Mutex, RwLock};
use tokio::time::timeout;
use uuid::Uuid;

#[derive(Clone)]
pub struct Context {
    pub id: Arc<Mutex<Option<String>>>,
    kik_op: Arc<Mutex<Option<Kik>>>,
    data_x: DataChan,
}

#[derive(Clone)]
pub struct DataChan {
    tx: UnboundedSender<(String, BytesMut)>,
    rx: Arc<Mutex<UnboundedReceiver<(String, BytesMut)>>>,
    //原子引用， 当前正在等待的id
    wait_data: Arc<Mutex<usize>>,
}

#[derive(Clone)]
pub struct Kik {
    pub id: String,
    pub name: String,
    pub conn_op: Arc<RwLock<Option<Arc<Mutex<Channel>>>>>,
    data_conns: Arc<Mutex<HashMap<String, Arc<Mutex<Channel>>>>>,
    next_data_conn: Arc<AtomicU16>,
}

impl Kik {
    pub async fn delete_data_conn(&self, conn: Arc<Mutex<Channel>>) -> Option<Arc<Mutex<Channel>>> {
        let id = conn.lock().await.get_id().to_string();
        self.data_conns.lock().await.remove(id.as_str())
    }
}

impl Kik {
    pub fn new(id: String, name: String, channel: Arc<Mutex<Channel>>) -> Kik {
        Kik {
            id,
            name,
            conn_op: Arc::new(RwLock::new(Some(channel))),
            data_conns: Arc::new(Mutex::new(HashMap::new())),
            next_data_conn: Arc::new(AtomicU16::new(0)),
        }
    }

    pub async fn insert_data_conn(&self, data_chan: Arc<Mutex<Channel>>) {
        let id = data_chan.lock().await.get_id().to_string();
        self.data_conns
            .lock()
            .await
            .insert(id, data_chan);
    }

    pub async fn find_data_conn(&self) -> Option<Arc<Mutex<Channel>>> {
        let next_arc = self.next_data_conn.clone();
        let data_map_arc = self.data_conns.clone();
        let data_map = data_map_arc.lock().await;
        if data_map.len() == 0 {
            None
        } else {
            let next = (next_arc.load(Ordering::SeqCst) + 1) % data_map.len() as u16;
            let c = data_map.values().nth(next as usize).unwrap().clone();
            next_arc.store(next + 1, Ordering::SeqCst);
            Some(c)
        }
    }

    pub async fn delete_kik_conn(&self) -> Option<Arc<Mutex<Channel>>> {
        let arc = self.conn_op.clone();
        let mut guard = arc.write().await;
        let option = guard.clone();
        *guard = None;
        option
    }
    pub async fn clear(&self) {
        {
            //data_conn的清理
            let arc = self.data_conns.clone();
            let mut guard = arc.lock().await;
            for x in guard.values() {
                x.lock().await.try_write_half_close().await;
            }
            guard.clear();
        }
        //kik_conn 关闭
        match self.conn_op.write().await.clone() {
            None => {}
            Some(conn) => {
                conn.lock().await.try_write_half_close().await;
            }
        };
    }
}

impl Context {
    pub fn new() -> Self {
        //由于我们一定要及时处理tcp消息，不及时处理会导致tcp发送方发消息阻塞，所以这里不能用有限容量通道
        let (tx, rx) = unbounded_channel();
        let rx = Arc::new(Mutex::new(rx));

        Context {
            id: Arc::new(Mutex::new(None)),
            kik_op: Arc::new(Mutex::new(None)),
            data_x: DataChan {
                tx,
                rx,
                wait_data: Arc::new(Mutex::new(0)),
            },
        }
    }
    pub async fn set_kik(&self, kik_op: Option<Kik>) {
        *(self.kik_op.clone().lock().await) = kik_op;
    }
    pub async fn send_data(&self, op: (String, BytesMut)) -> anyhow::Result<()> {
        //只在 确为当前在等数据 或者 当前无在等数据时，可以写入此数据
        let (can_send, now_wait_id) = unsafe {
            let wait = self.data_x.wait_data.lock().await;
            let wait = ((*wait) as *mut String).as_ref();
            let wait_id = wait.cloned();
            (
                wait_id.is_none() || wait_id.as_ref().unwrap() == &op.0,
                wait_id,
            )
        };
        if can_send {
            Ok(self.data_x.tx.send(op).context("不可能没有读者!")?)
        } else {
            //直接丢弃
            Err(anyhow!(
                "正在读，得到数据id:{}与预期id:{:?}与不一致",
                op.0,
                now_wait_id
            ))
        }
    }

    pub async fn read_data(&self, key: String) -> anyhow::Result<BytesMut> {
        let wait_data_arc = self.data_x.wait_data.clone();

        let uint = {
            //.await之前不能存在非Send的类型，所以这里需要将key_ptr销毁
            let key_ptr = Box::into_raw(Box::new(key));
            key_ptr as usize
        };
        //不想复制一份key，所以用原始指针
        *(wait_data_arc.clone().lock().await) = uint;

        let ret = timeout(Duration::from_secs(360), async {
            loop {
                let (k, data) = match self.data_x.rx.clone().lock().await.recv().await {
                    //todo 写端关闭，不可能，需要上报日志
                    None => return Err(anyhow!("系统异常，原因: 写端关闭")),
                    Some(o) => o,
                };

                //这里可以安全的无锁访问
                unsafe {
                    if k == *(uint as *mut String) {
                        return Ok(data);
                    }
                }
            }
        })
        .await
        .map_err(|e| anyhow!("数据读取超时"));

        //确保这一步最后要执行
        unsafe {
            //锁住，使得这里不会发生 另一个线程取原始指针内容 却取到已释放内容的问题
            let mut ptr = wait_data_arc.lock().await;
            // 回收内存
            let _ = Box::from_raw(*ptr as *mut String);
            *ptr = 0;
        }
        ret?
    }

    pub fn get_data_rx(&self) -> Arc<Mutex<UnboundedReceiver<(String, BytesMut)>>> {
        self.data_x.rx.clone()
    }
    pub fn get_data_tx(&self) -> UnboundedSender<(String, BytesMut)> {
        self.data_x.tx.clone()
    }
    pub async fn get_kik(&self) -> Option<Kik> {
        (*self.kik_op.clone().lock().await).clone()
    }
    // pub async fn set_kik_conn(
    //     &self,
    //     kik_conn: Option<Arc<Mutex<Channel>>>,
    // ) -> Option<Arc<Mutex<Channel>>> {
    //     match (*(self.kik_op.clone().lock().await)).clone() {
    //         None => {
    //             panic!("不能在没有 kik时set conn");
    //         }
    //         Some(ref kik) => match kik_conn {
    //             None => kik.delete_kik_conn().await,
    //             Some(conn) => kik.set_kik_conn(conn).await,
    //         },
    //     }
    // }
    // pub async fn get_kik_conn(&self) -> Option<Arc<Mutex<Channel>>> {
    //     match (*(self.kik_op.clone().lock().await)).clone() {
    //         None => None,
    //         Some(ref kik) => kik.get_kik_conn().await,
    //     }
    // }
    pub async fn insert_data_conn(&self, conn: Arc<Mutex<Channel>>) {
        self.kik_op
            .lock()
            .await
            .clone()
            .expect("不能在没有 kik时set conn")
            .insert_data_conn(conn)
            .await;
    }
    pub async fn delete_data_conn(&self, conn: Arc<Mutex<Channel>>) {
        self.kik_op
            .clone()
            .lock()
            .await
            .clone()
            .expect("不能在没有 kik时set conn")
            .delete_data_conn(conn)
            .await;
    }
    pub async fn find_data_conn(&self) -> Option<Arc<Mutex<Channel>>> {
        let arc = self.kik_op.clone();
        let guard = arc.lock().await;
        if guard.is_some() {
            guard.as_ref().unwrap().find_data_conn().await
        } else {
            None
        }
    }
    pub async fn find_and_send_data(&self, data: &[u8]) -> anyhow::Result<String> {
        match self.find_data_conn().await {
            None => Err(anyhow::Error::msg("Kik数据连接未初始化完成")),
            Some(c) => {
                let data_id = Uuid::new_v4().to_string();
                let mut bytes_mut = BytesMut::with_capacity(data.len());
                bytes_mut.put_slice(&data);
                c.clone()
                    .lock()
                    .await
                    .write_and_flush(&protocol::transfer_encode_frame(KikFrame::Data(
                        data_id.clone(),
                        bytes_mut,
                    )))
                    .await?;
                Ok(data_id)
            }
        }
    }
    pub async fn clear(&self) {
        match self.kik_op.clone().lock().await.as_ref() {
            None => {}
            Some(kik) => {
                kik.clear().await;
            }
        }
    }
}

#[test]
fn test2() {
    unsafe {
        let raw = Box::into_raw(Box::new(String::from("test")));
        let wait = raw as usize;
        println!("{}", wait);
        let string = (wait as *mut String).as_ref().unwrap().clone();
        println!("{}", string); //ok
    }
}

#[test]
fn test() {
    let x = unsafe {
        let raw = Box::into_raw(Box::new(String::from("test")));
        let wait = raw as usize;
        println!("{}", wait);
        let y = (wait as *mut String).as_ref().unwrap();
        let _ = Box::from_raw(raw); //释放内存，实际上是先交换rust内存分配器，不会立即交还操作系统
        y
    };

    // let _filler = vec![0u8; 1024 * 1024 * 1024]; //x那一小块内存无法满足，于是直接重新向操作系统申请大块内存，x的内存不会被占用

    //连续向rust分配器申请小块内存，这样x的内存就很有可能被覆盖重写
    for _ in 0..1000 {
        let _filler = Box::new(42u64); // 8字节，与 String 结构大小相近
    }
    println!("{}", x); //乱码
}
