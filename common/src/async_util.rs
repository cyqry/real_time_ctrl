use anyhow::anyhow;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

// 定义任务类型：一个返回Future的闭包
type Task = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output=()> + Send + 'static>> + Send + 'static>;

/// 异步执行器
#[derive(Debug)]
pub struct AsyncExecutor<T> {
    task_tx: Option<T>,
    worker_handle: Option<JoinHandle<()>>,
    capacity: usize,
}

pub trait Sender {
    fn send(
        &self,
        data: Option<Task>,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
}
pub trait Receiver {
    fn recv(&mut self) -> impl Future<Output = Option<Option<Task>>> + Send;
}

impl Sender for mpsc::Sender<Option<Task>> {
    fn send(
        &self,
        data: Option<Task>,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send {
        async {
            let res = self.send(data).await;
            match res {
                Ok(_) => Ok(()),
                Err(e) => Err(anyhow!("{}", e)),
            }
        }
    }
}

impl Sender for mpsc::UnboundedSender<Option<Task>> {
    async fn send(&self, data: Option<Task>) -> anyhow::Result<()> {
        self.send(data).map_err(|e| anyhow!("{}", e))
    }
}

impl Receiver for mpsc::UnboundedReceiver<Option<Task>> {
    async fn recv(&mut self) -> Option<Option<Task>> {
        self.recv().await
    }
}
impl Receiver for mpsc::Receiver<Option<Task>> {
    async fn recv(&mut self) -> Option<Option<Task>> {
        self.recv().await
    }
}

/// 创建有限缓存的异步执行器
pub fn new(size: usize) -> AsyncExecutor<mpsc::Sender<Option<Task>>> {
    // 创建有界任务通道
    let (task_tx, task_rx) = mpsc::channel(size);

    // 启动工作协程
    let worker_handle = tokio::spawn(worker_loop(task_rx));

    AsyncExecutor {
        task_tx: Some(task_tx),
        worker_handle: Some(worker_handle),
        capacity: size,
    }
}

/// 创建无限缓存的异步执行器
pub fn new_unbound() -> AsyncExecutor<mpsc::UnboundedSender<Option<Task>>> {
    // 创建无界任务通道
    let (task_tx, task_rx) = mpsc::unbounded_channel();

    // 启动工作协程
    let worker_handle = tokio::spawn(worker_loop(task_rx));

    AsyncExecutor {
        task_tx: Some(task_tx),
        worker_handle: Some(worker_handle),
        capacity: 0, // 0表示无限容量
    }
}

/// 工作协程的主循环
async fn worker_loop(mut task_rx: impl Receiver) {
    while let Some(task_opt) = task_rx.recv().await {
        match task_opt {
            Some(task) => {
                let future = task();
                future.await;
            }
            None => {
                break;
            }
        }
    }
}

impl<T: Sender> AsyncExecutor<T> {
    /// 获取执行器容量
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    // /// 获取当前队列中的任务数量（近似值）
    // pub async fn pending_tasks(&self) -> usize {
    //     if let Some(tx) = &self.task_tx {
    //         tx.capacity()
    //             .map(|cap| {
    //                 // 有界队列：容量减去可用空间
    //                 if cap > 0 { cap - tx.max_capacity() } else { 0 }
    //             })
    //             .unwrap_or(0)
    //     } else {
    //         0
    //     }
    // }

    /// 提交异步任务（阻塞直到有可用空间）
    pub async fn submit(&self, task: Task) -> anyhow::Result<()> {
        self.task_tx
            .as_ref()
            .ok_or(anyhow!("执行器已关闭"))?
            .send(Some(task))
            .await
            .map_err(|e| anyhow!("Failed to submit task: {}", e))
    }

    /// 提交异步任务并等待返回结果（阻塞直到有可用空间）
    pub async fn submit_with_result<R>(&self, f: Box<dyn FnOnce() -> Pin<Box<dyn Future<Output=R> + Send >> + Send>) -> anyhow::Result<oneshot::Receiver<R>>
    where
        R: Send + 'static,
    {
        let (result_tx, result_rx) = oneshot::channel();

        let task2 = Box::new(move ||-> Pin<Box<dyn Future<Output=()> + Send>> {
            let f = f;
            Box::pin(async move {
                let result = f().await;
                _ = result_tx.send(result);
            })
        });

        self.submit(task2).await?;

        Ok(result_rx)
    }

    /// 任务执行结束信号
    pub async fn finish(&mut self) -> Result<(), String> {
        // 发送关闭信号
        if let Some(tx) = self.task_tx.take() {
            // 这里我们发送None来通知worker退出
            let _ = tx.send(None).await;
            Ok(())
        } else {
            Err("执行器已关闭".to_string())
        }
    }

    /// 等待所有任务完成
    pub async fn wait(&mut self) -> Result<(), String> {
        // 等待工作协程完成
        if let Some(handle) = self.worker_handle.take() {
            handle
                .await
                .map_err(|e| format!("Worker panicked: {}", e))?;
        }
        Ok(())
    }

    /// 检查执行器是否已关闭
    pub fn is_closed(&self) -> bool {
        self.task_tx.is_none()
    }

    // 检查队列是否已满（仅对有限缓存版本有意义）
    // pub fn is_full(&self) -> bool {
    //     match &self.task_tx {
    //         Some(tx) => tx.is_full(),
    //         None => true,
    //     }
    // }
}

// impl Drop for AsyncExecutor<mpsc::UnboundedSender<Option<Task>>> {
//     fn drop(&mut self) {
//         // 如果用户没有显式调用finish，则尝试关闭
//         if let Some(tx) = self.task_tx.take() {
//             // 由于在Drop中不能await，我们只能尝试发送，如果失败则忽略
//             _ = tx.send(None);
//         }
//         // 注意：在Drop中不能等待异步操作完成
//     }
// }
#[cfg(test)]
mod tests {
    use super::*;
    use crate::async_util;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::sleep;

    // #[tokio::test]
    // async fn test_basic_execution() {
    //     let mut executor = async_util::new(5);
    //     let counter = Arc::new(AtomicUsize::new(0));
    //
    //     for i in 0..10 {
    //         let counter_clone = counter.clone();
    //         executor
    //             .submit(move || {
    //                 let counter = counter_clone;
    //                 async move {
    //                     counter.fetch_add(i, Ordering::SeqCst);
    //                 }
    //             })
    //             .await
    //             .unwrap();
    //     }
    //     executor.finish().await.unwrap();
    //     // 给任务一些时间执行
    //     sleep(Duration::from_millis(100)).await;
    //     executor.wait().await.unwrap();
    //     // 0+1+2+...+9 = 45
    //     assert_eq!(counter.load(Ordering::SeqCst), 45);
    // }
    //
    // #[tokio::test]
    // async fn test_shutdown_with_pending_tasks() {
    //     let mut executor = new(5);
    //     let counter = Arc::new(AtomicUsize::new(0));
    //
    //     for i in 0..5 {
    //         let counter_clone = counter.clone();
    //         executor
    //             .submit(move || {
    //                 let counter = counter_clone;
    //                 async move {
    //                     // 模拟长时间运行的任务
    //                     sleep(Duration::from_millis(50)).await;
    //                     counter.fetch_add(i, Ordering::SeqCst);
    //                 }
    //             })
    //             .await
    //             .unwrap();
    //     }
    //     executor.finish().await.unwrap();
    //     // 立即关闭，但应该等待所有任务完成
    //     sleep(Duration::from_millis(10)).await;
    //     executor.wait().await.unwrap();
    //
    //     // 所有任务都应该完成
    //     assert_eq!(counter.load(Ordering::SeqCst), 10); // 0+1+2+3+4 = 10
    // }
    //
    // #[tokio::test]
    // async fn test_result_return() {
    //     let mut executor = new(5);
    //
    //     let result = executor
    //         .submit_with_result(|| async { 42 })
    //         .await
    //         .unwrap()
    //         .await
    //         .unwrap();
    //     executor.finish().await.unwrap();
    //     executor.wait().await.unwrap();
    //     assert_eq!(result, 42);
    // }
    //
    // #[tokio::test]
    // async fn test_concurrent_submission() {
    //     // let executor = Arc::new(new(4));
    //     // let counter = Arc::new(AtomicUsize::new(0));
    //     //
    //     // let mut handles = vec![];
    //     //
    //     // for _ in 0..10 {
    //     //     let counter = counter.clone();
    //     //     let executor = executor.clone();
    //     //     let handle = tokio::spawn(async move {
    //     //         let mut executor =executor;
    //     //       executor.submit(||{
    //     //           async {
    //     //
    //     //           }
    //     //       }).await.unwrap();
    //     //         // executor.submit( || {
    //     //         //     async {}
    //     //         // }).await.unwrap();
    //     //         // for i in 0..10 {
    //     //         //     let counter = counter.clone();
    //     //         //     executor
    //     //         //         .submit(move || {
    //     //         //             let counter = counter;
    //     //         //             async move {
    //     //         //                 counter.fetch_add(i, Ordering::SeqCst);
    //     //         //             }
    //     //         //         })
    //     //         //         .await
    //     //         //         .unwrap();
    //     //         // }
    //     //     });
    //     //     handles.push(handle);
    //     // }
    //     //
    //     // for handle in handles {
    //     //     handle.await.unwrap();
    //     // }
    //     // let mut async_executor = Arc::try_unwrap(executor).unwrap();
    //     // async_executor.finish().await.unwrap();
    //     // // 给任务一些时间执行
    //     // sleep(Duration::from_millis(100)).await;
    //     // async_executor.wait().await.unwrap();
    // }
}
