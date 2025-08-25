use anyhow::{anyhow, Error};
use bytes::{BufMut, BytesMut};
use common::channel::Channel;
use common::kik::Kik;
use common::message::frame::Frame;
use common::protocol;
use common::protocol::BufSerializable;
use std::collections::HashMap;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::Mutex;
use tokio::time::timeout;
use uuid::Uuid;

#[derive(Clone)]
pub struct Context {
    pub id: Arc<Mutex<Option<String>>>,
    kik_op: Arc<Mutex<Option<Kik>>>,
    data_x: DataChan,
}

#[derive(Clone)]
struct DataChan {
    tx: Sender<(String, BytesMut)>,
    rx: Arc<Mutex<Receiver<(String, BytesMut)>>>,
    //原子引用， 当前正在等待的id
    wait_data: Arc<Mutex<*mut String>>,
}

impl Context {
    pub fn new() -> Self {
        let (tx, rx) = channel(5);
        Context {
            id: Arc::new(Mutex::new(None)),
            kik_op: Arc::new(Mutex::new(None)),
            data_x: DataChan {
                tx,
                rx: Arc::new(Mutex::new(rx)),
                wait_data: Arc::new(Mutex::new(null_mut())),
            },
        }
    }
    pub async fn set_kik(&self, kik_op: Option<Kik>) {
        *(self.kik_op.clone().lock().await) = kik_op;
    }
    pub async fn send_data(&self, op: (String, BytesMut)) -> anyhow::Result<()> {
        let can_send = unsafe {
            let wait = self.data_x.wait_data.clone().lock().await;
            !wait.is_null() && **wait.as_ref().unwrap() == op.0
        };
        if can_send {
            Ok(self.data_x.tx.send(op).await?)
        } else {
            //直接丢弃
            Ok(())
        }
    }
    pub async fn read_data(&self, key: String) -> anyhow::Result<BytesMut> {
        let wait_data_arc = self.data_x.wait_data.clone();
        let key_ptr = Box::into_raw(Box::new(key));

        *(wait_data_arc.clone().lock().await) = key_ptr;

        let ret = timeout(Duration::from_secs(360), async {
            loop {
                let (k, data) = match self.data_x.rx.clone().lock().await.recv().await {
                    //todo 写端关闭，不可能，需要上报日志
                    None => return Err(anyhow!("系统异常，原因: 写端关闭")),
                    Some(o) => o,
                };

                //这里可以安全的无锁访问
                unsafe {
                    if k == *key_ptr {
                        return Ok(data);
                    }
                }
            }
        })
        .await
        .map_err(|e| anyhow!("数据读取超时"));

        //确保这一步最后要执行
        unsafe {
            //这里锁住，不会发生 另一个线程取原始指针内容 却取到已释放内容的问题
            let ptr = wait_data_arc.clone().lock().await;
            // 回收内存
            let _ = Box::from_raw(*ptr);
            *ptr = null_mut();
        }
        ret?
    }

    pub fn get_data_rx(&self) -> Arc<Mutex<Receiver<(String, BytesMut)>>> {
        self.data_x.rx.clone()
    }
    pub fn get_data_tx(&self) -> Sender<(String, BytesMut)> {
        self.data_x.tx.clone()
    }
    pub async fn get_kik(&self) -> Option<Kik> {
        (*self.kik_op.clone().lock().await).clone()
    }
    pub async fn set_kik_conn(
        &self,
        kik_conn: Option<Arc<Mutex<Channel>>>,
    ) -> Option<Arc<Mutex<Channel>>> {
        match (*(self.kik_op.clone().lock().await)).clone() {
            None => {
                panic!("不能在没有 kik时set conn");
            }
            Some(ref kik) => match kik_conn {
                None => kik.delete_kik_conn().await,
                Some(conn) => kik.set_kik_conn(conn).await,
            },
        }
    }
    pub async fn get_kik_conn(&self) -> Option<Arc<Mutex<Channel>>> {
        match (*(self.kik_op.clone().lock().await)).clone() {
            None => None,
            Some(ref kik) => kik.get_kik_conn().await,
        }
    }
    pub async fn insert_data_conn(&self, conn: Arc<Mutex<Channel>>) {
        self.kik_op
            .clone()
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
                    .write_and_flush(&protocol::transfer_encode(
                        Frame::Data(data_id.clone(), bytes_mut).to_buf(),
                    ))
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
