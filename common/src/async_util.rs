use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use anyhow::anyhow;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::sync::oneshot::Receiver;
use tokio::task::JoinHandle;

// 定义任务类型：一个返回Future的闭包
type Task = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

/// 异步执行器
#[derive(Debug)]
pub struct AsyncExecutor {
    task_tx: UnboundedSender<Option<Task>>,
    worker_handle: Option<JoinHandle<()>>,
}

impl AsyncExecutor {
    /// 创建新的异步执行器
    pub fn new() -> Self {
        // 创建任务通道
        let (task_tx, task_rx) = mpsc::unbounded_channel();


        // 启动工作协程
        let worker_handle = tokio::spawn(Self::worker_loop(task_rx));

        Self {
            task_tx,
            worker_handle: Some(worker_handle),
        }
    }

    /// 工作协程的主循环
    async fn worker_loop(mut task_rx: UnboundedReceiver<Option<Task>>) {
        let mut shutdown = false;
        while let Some(Some(task)) = task_rx.recv().await {
            let future = task();
            future.await;
        }
    }

    /// 提交异步任务
    pub fn submit<F, Fut>(&self, f: F) -> anyhow::Result<()>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let task: Task = Box::new(move || Box::pin(f()));

        self.task_tx
            .send(Some(task))
            .map_err(|e| anyhow!("Failed to submit task: {}", e))
    }

    /// 提交异步任务并等待返回结果
    pub fn submit_with_result<F, Fut, R>(&self, f: F) -> anyhow::Result<Receiver<R>>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        let (result_tx, result_rx) = oneshot::channel();

        let task = move || {
            let future = f();
            async move {
                let result = future.await;
                let _ = result_tx.send(result);
            }
        };

        self.submit(task)?;

        Ok(result_rx)
    }

    /// 优雅关闭执行器
    pub async fn shutdown(mut self) -> Result<(), String> {
        // 发送关闭信号
        self.task_tx.send(None).unwrap_or(());
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
        self.task_tx.is_closed()
    }
}

impl Drop for AsyncExecutor {
    fn drop(&mut self) {
        // 如果用户没有显式调用shutdown，则尝试关闭
        self.task_tx.send(None).unwrap_or(());
        // 注意：在Drop中不能等待异步操作完成
    }
}

/// 线程安全的执行器引用
// pub type AsyncExecutorRef = Arc<AsyncExecutor>;

/// 构建器模式，用于配置执行器
// pub struct ExecutorBuilder {
//     worker_name: Option<String>,
//     panic_handler: Option<Box<dyn Fn(Box<dyn std::any::Any + Send>) + Send + Sync>>,
// }
//
// impl ExecutorBuilder {
//     pub fn new() -> Self {
//         Self {
//             worker_name: None,
//             panic_handler: None,
//         }
//     }
//
//     pub fn worker_name(mut self, name: impl Into<String>) -> Self {
//         self.worker_name = Some(name.into());
//         self
//     }
//
//     pub fn panic_handler<F>(mut self, handler: F) -> Self
//     where
//         F: Fn(Box<dyn std::any::Any + Send>) + Send + Sync + 'static,
//     {
//         self.panic_handler = Some(Box::new(handler));
//         self
//     }
//
//     pub fn build(self) -> AsyncExecutor {
//         let (task_tx, task_rx) = mpsc::unbounded_channel();
//         let (shutdown_tx, shutdown_rx) = oneshot::channel();
//
//         // 包装工作循环以处理panic
//         let worker_loop = async move {
//             let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
//                 async move {
//                     AsyncExecutor::worker_loop(task_rx).await
//                 }
//             ));
//
//             match result {
//                 Ok(future) => future.await,
//                 Err(panic) => {
//                     if let Some(handler) = &self.panic_handler {
//                         handler(panic);
//                     } else {
//                         eprintln!("Executor worker panicked!");
//                     }
//                 }
//             }
//         };
//
//         // 创建任务，可选命名
//         let worker_handle = if let Some(name) = self.worker_name {
//             tokio::task::Builder::::new().name(&name).spawn(worker_loop)
//         } else {
//             tokio::spawn(worker_loop)
//         }.expect("Failed to spawn worker task");
//
//         AsyncExecutor {
//             task_tx,
//             worker_handle: Some(worker_handle),
//         }
//     }
// }

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::sleep;

    #[tokio::test]
    async fn test_basic_execution() {
        let executor = AsyncExecutor::new();
        let counter = Arc::new(AtomicUsize::new(0));

        for i in 0..10 {
            let counter_clone = counter.clone();
            executor
                .submit(move || {
                    let counter = counter_clone;
                    async move {
                        counter.fetch_add(i, Ordering::SeqCst);
                    }
                })
                .unwrap();
        }

        // 给任务一些时间执行
        sleep(Duration::from_millis(100)).await;

        // 0+1+2+...+9 = 45
        assert_eq!(counter.load(Ordering::SeqCst), 45);

        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_shutdown_with_pending_tasks() {
        let executor = AsyncExecutor::new();
        let counter = Arc::new(AtomicUsize::new(0));

        for i in 0..5 {
            let counter_clone = counter.clone();
            executor
                .submit(move || {
                    let counter = counter_clone;
                    async move {
                        // 模拟长时间运行的任务
                        sleep(Duration::from_millis(50)).await;
                        counter.fetch_add(i, Ordering::SeqCst);
                    }
                })
                .unwrap();
        }

        // 立即关闭，但应该等待所有任务完成
        sleep(Duration::from_millis(10)).await;
        executor.shutdown().await.unwrap();

        // 所有任务都应该完成
        assert_eq!(counter.load(Ordering::SeqCst), 10); // 0+1+2+3+4 = 10
    }

    #[tokio::test]
    async fn test_result_return() {
        let executor = AsyncExecutor::new();

        let result = executor
            .submit_with_result(|| async { 42 })
            .unwrap().await.unwrap();

        assert_eq!(result, 42);

        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_concurrent_submission() {
        let executor = Arc::new(AsyncExecutor::new());
        let counter = Arc::new(AtomicUsize::new(0));

        let mut handles = vec![];

        for _ in 0..10 {
            let executor = executor.clone();
            let counter = counter.clone();
            let handle = tokio::spawn(async move {
                for i in 0..10 {
                    let counter = counter.clone();
                    executor
                        .submit(move || {
                            let counter = counter;
                            async move {
                                counter.fetch_add(i, Ordering::SeqCst);
                            }
                        })
                        .unwrap();
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.await.unwrap();
        }

        // 给任务一些时间执行
        sleep(Duration::from_millis(100)).await;

        Arc::try_unwrap(executor)
            .unwrap()
            .shutdown()
            .await
            .unwrap();
    }
}
