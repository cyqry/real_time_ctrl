use crate::context::Context;
use anyhow::anyhow;
use bytes::{BufMut, BytesMut};
use common::channel::Channel;
use common::command::{Command, CtrlCommand};
use common::message::frame::Frame;
use common::message::resp::Resp;
use common::protocol::BufSerializable;
use common::{file_util, protocol};
use std::f32::consts::E;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

pub async fn execute(context: &Context, mut cmd: CtrlCommand) -> anyhow::Result<String> {
    match cmd {
        CtrlCommand::SetFile(ref mut file_path, _) => {
            let v = file_util::read_file(file_path.clone()).await?;
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
                        .write_and_flush(&protocol::transfer_encode(
                            Frame::Data(data_id.clone(), bytes_mut).to_buf(),
                        ))
                        .await?;
                    //让Kik拿到data_id
                    *file_path = data_id;
                }
            }
        }
        _ => {}
    }

    match context
        .agent
        .clone()
        .write()
        .await
        .req(&Command::Ctrl(cmd.clone()))
        .await?
    {
        Resp::Info(info) => Ok(info),
        Resp::DataId(data_id) => {
            match cmd {
                CtrlCommand::GetFile(_, save_path) => {
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
                CtrlCommand::GetBigFile(_, save_path) => {
                    return Err(anyhow::Error::msg("暂不支持"));
                }
                CtrlCommand::Screen(save_path) => {
                    //get data
                    match context.wait_data(data_id.as_str()).await {
                        Ok(data) => {
                            let mut path = PathBuf::from(save_path.as_str());

                            if path.is_dir() {
                                path = path.join("1.png");
                            };
                            match file_util::save_file_with_unique_name(path.as_path(), &data).await {
                                Ok(_) => Ok(format!("保存Kik的截屏至:{:?}", save_path)),
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
