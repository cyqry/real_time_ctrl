use crate::context::{id, Context};
use crate::input_command::InputCtrlCommand;
use anyhow::anyhow;
use bytes::{BufMut, BytesMut};
use common::command::{Command, CtrlCommand};
use common::message::frame::Frame;
use common::message::resp::Resp;
use common::protocol::dok::{Dok, ErrCode};
use common::protocol::{BufSerializable, CmdOptions, ReqCmd};
use common::{file_util, protocol};
use sha2::digest::DynDigest;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio_stream::StreamExt;
use uuid::Uuid;
use common::async_util::AsyncExecutor;

pub async fn execute(context: &Context, mut input_ctrl_cmd: InputCtrlCommand) -> anyhow::Result<String> {
    let mut cmd_options = CmdOptions::default();



   let cmd = match input_ctrl_cmd.clone() {
       InputCtrlCommand::SetFile( file_path, target_path) => {
           let v = file_util::read_file(file_path).await?;
           match context.find_ctrl_data().await {
               None => {
                   return Err(anyhow::Error::msg("应用数据传输通道未初始化!"));
               }
               Some(data_conn) => {
                   let data_id = Uuid::new_v4().to_string();
                   let mut bytes_mut = BytesMut::with_capacity(v.len());
                   bytes_mut.put_slice(&v);
                   data_conn
                       .clone()
                       .lock()
                       .await
                       .write_and_flush(&protocol::transfer_encode_frame(
                           Frame::Data(data_id.clone(), bytes_mut),
                       ))
                       .await?;
                   //让Kik拿到data_id
                   CtrlCommand::SetFile(data_id, target_path)
               }
           }
       }
       InputCtrlCommand::SetBigFile( file_path, target_path) => {
           cmd_options = cmd_options.with_timeout(false);
           //将文件分割并带上这部分的[start,end], 文件在到达时分块写入，写完后校验hash

           let (file_size, mut iter) =
               file_util::read_big_file(file_path, 1024 * 1024 * 10).await?;

           let mut hasher = Sha256::new();
           let data_id = Uuid::new_v4().to_string();

           let executor = AsyncExecutor::new();
           let mut res_recviers = Vec::new();
           let sended = loop {
               match iter.next().await {
                   None => {
                       break None;
                   }
                   Some(Ok((range, data))) => {
                       DynDigest::update(&mut hasher, data.as_ref());
                       let v = Dok::FilePart(range.start, range.end - 1, data).to_buf();
                       let data_id = data_id.clone();
                       let context = context.clone();
                       res_recviers.push(   executor.submit_with_result(move || {
                           return async move {
                              return  match context.send_data_with_id(data_id, v.as_ref()).await {
                                   Ok(_) => {
                                       None
                                   }
                                   Err(e) => {
                                       Some((ErrCode::ReadError, e))
                                   }
                               };
                           }
                       })?);//这里提交任务不可能出错
                   }
                   Some(Err(e)) => {
                       break Some((ErrCode::WriteError, anyhow!(e)));
                   }
               }
           };
           executor.shutdown().await.unwrap_or(());
           if let Some((code, e)) = sended {
               let v = Dok::Err(code).to_buf();
               context.send_data_with_id(data_id.clone(), v.as_ref()).await?;
               return Err(e);
           }
           
           //异步执行数据发送中如果出错，这里需要发一个
           let context = context.clone();
           let data_id_c = data_id.clone();
           tokio::spawn(async move {
               //这里不会一直等
               for recv in res_recviers {
                   if let Ok(Some((code, e))) = recv.await {
                       let v = Dok::Err(code).to_buf();
                       context.send_data_with_id(data_id_c, v.as_ref()).await.unwrap_or(());
                       break;
                   }
               }
           });
           // 发送hash
           CtrlCommand::SetBigFile(data_id, file_size, hasher.finalize().to_vec(), target_path)
       }
       icc => {
           icc.into()
       }
   };


    let req_cmd = ReqCmd::new(id(), cmd_options, Command::Ctrl(cmd.clone()));
    match context
        .agent
        .clone()
        .write()
        .await
        .req(&req_cmd)
        .await?
    {
        Resp::Info(info) => Ok(info),
        Resp::DataId(data_id) => {
            match input_ctrl_cmd {
                InputCtrlCommand::GetFile(_, save_path) => {
                    //get data
                    match context.wait_data(data_id.as_str()).await {
                        Ok(data) => match file_util::save_file(save_path.as_str(), &data).await {
                            Ok(_) => Ok(format!("保存文件至:{}", save_path)),
                            Err(e) => {
                                Err(anyhow!(format!("保存文件至:{}失败,err:{}", save_path, e)))
                            }
                        },
                        Err(e) => Err(anyhow!(format!("接收文件失败,{}", e))),
                    }
                }
                InputCtrlCommand::GetBigFile(_, save_path) => {
                    return Err(anyhow::Error::msg("暂不支持"));
                }
                InputCtrlCommand::Screen(save_path) => {
                    //get data
                    match context.wait_data(data_id.as_str()).await {
                        Ok(data) => {
                            let mut path = PathBuf::from(save_path.as_str());

                            if path.is_dir() {
                                path = path.join("1.png");
                            };
                            match file_util::save_file_with_unique_name(path.as_path(), &data).await
                            {
                                Ok(p) => Ok(format!("保存Kik的截屏至:{:?}", p)),
                                Err(e) => Err(anyhow!(format!(
                                    "保存Kik的截屏至:{:?}失败,err:{}",
                                    save_path, e
                                ))),
                            }
                        }
                        Err(e) => Err(anyhow!(format!("接收文件失败,{}", e))),
                    }
                }
                _ => {
                    return Err(anyhow::Error::msg("不支持的类型"));
                }
            }
        }
    }
}

