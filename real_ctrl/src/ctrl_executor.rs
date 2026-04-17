use crate::context::{id, Context};
use crate::input_command::{RemoteResp, InputCtrlCommand};
use anyhow::{anyhow, Context as AnyhowContext, };
use bytes::{BufMut, BytesMut};
use common::async_util::AsyncExecutor;
use common::command::{Command, CtrlCommand};
use common::message::kik_resp::{ClientSuccessResp, KikResp};
use common::message::dok::{Dok, ErrCode};
use common::protocol::{BufSerializable, CmdOptions, ReqCmd};
use common::{async_util, file_util, protocol};
use ctrl_common::ctrl_resp::{CmdResp, Resp, ServerResp, ServerSuccessResp};
use futures::future::ok;
use sha2::digest::DynDigest;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use tokio_stream::StreamExt;
use uuid::Uuid;
use ctrl_common::ctrl_frame::Frame;

pub async fn execute(
    context: &Context,
    input_ctrl_cmd: InputCtrlCommand,
    origin_data:bool,
) -> anyhow::Result<RemoteResp> {
    let (cmd, cmd_options) = process_cmd(context, &input_ctrl_cmd).await?;

    let req_cmd = ReqCmd::new(id(), cmd_options, Command::Ctrl(cmd.clone()));
    match context
        .agent
        .clone()
        .write()
        .await
        .req(&req_cmd)
        .await?
        .get_resp()
    {
        Resp::Kik(KikResp::Success(ClientSuccessResp::Info(info))) => {
            Ok(RemoteResp::Success(info.to_string()))
        }
        Resp::Kik(KikResp::Error(err_code, info)) => Ok(RemoteResp::Error(
            err_code.clone() as u32,
            info.to_string(),
        )),
        Resp::Server(ServerResp::Success(ServerSuccessResp::Info(info))) => {
            Ok(RemoteResp::Success(info.to_string()))
        }
        Resp::Server(ServerResp::Error(err_code, info)) => Ok(RemoteResp::Error(
            err_code.clone() as u32,
            info.to_string(),
        )),
        Resp::Kik(KikResp::Success(ClientSuccessResp::DataId(data_id))) => {
            if origin_data {
                let v = context.wait_data(data_id.as_str()).await.context("获取数据失败")?;
                Ok(RemoteResp::SuccessData(v))
            } else {
                let ok_info = process_ctrl_cmd_data_id_resp(context, input_ctrl_cmd, data_id).await?;
                Ok(RemoteResp::Success(ok_info))
            }
         
            // match context.wait_data(data_id.as_str()).await {
            //     Ok(data) => {
            //         let ok_info = process_ctrl_cmd_data_resp(context, input_ctrl_cmd, data).await?;
            //         Ok(RemoteResp::Success(ok_info))
            //     }
            //     Err(e) => {
            //         return Err(anyhow!("获取数据失败,err:{}",e));
            //     }
            // }
         
        }
    }
}

async fn process_cmd(context: &Context, input_ctrl_cmd: &InputCtrlCommand) -> anyhow::Result<(CtrlCommand, CmdOptions)> {
    let (cmd, cmd_options) = match input_ctrl_cmd.clone() {
        InputCtrlCommand::SetFile(file_path, target_path) => {
            do_set_file(context, file_path, target_path).await?
        }
        InputCtrlCommand::SetBigFile(file_path, target_path) => {
            do_set_big_file(context, file_path, target_path).await?
        }
        InputCtrlCommand::GetBigFile(src_path, dst_path) => (
            CtrlCommand::GetBigFile(src_path, dst_path),
            CmdOptions::default().with_timeout(false),
        ),
        icc => (icc.into(), CmdOptions::default()),
    };
    Ok((cmd, cmd_options))
}

// 
async fn process_ctrl_cmd_data_id_resp(context: &Context, input_ctrl_cmd: InputCtrlCommand, data_id: &String) -> anyhow::Result<String> {
    let ok_info = match input_ctrl_cmd {
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
            match context.wait_data(data_id.as_str()).await {
                Ok(data) => match file_util::save_file(save_path.as_str(), &data).await {
                    Ok(_) => Ok(format!("保存大文件至:{}", save_path)),
                    Err(e) => {
                        Err(anyhow!(format!("保存大文件至:{}失败,err:{}", save_path, e)))
                    }
                },
                Err(e) => Err(anyhow!(format!("接收大文件失败,{}", e))),
            }
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
                        Ok(p) => Ok(format!("保存Kik的截屏至:{}", p.to_string_lossy())),
                        Err(e) => Err(anyhow!(format!(
                                    "保存Kik的截屏至:{}失败,err:{}",
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
    }?;
    Ok(ok_info)
}


async fn do_set_file(
    context: &Context,
    file_path: String,
    target_path: String,
) -> anyhow::Result<(CtrlCommand, CmdOptions)> {
    let v = file_util::read_file(file_path).await?;
    let data_conn = context
        .find_ctrl_data()
        .await
        .ok_or(anyhow!("应用数据传输通道未初始化"))?;

    let data_id = Uuid::new_v4().to_string();
    let mut bytes_mut = BytesMut::with_capacity(v.len());
    bytes_mut.put_slice(&v);
    data_conn
        .lock()
        .await
        .write_and_flush(&protocol::transfer_encode_frame(Frame::Data(
            data_id.clone(),
            bytes_mut,
        )))
        .await?;
    //让Kik拿到data_id
    Ok((
        CtrlCommand::SetFile(data_id, target_path),
        CmdOptions::default(),
    ))
}

async fn do_set_big_file(
    context: &Context,
    file_path: String,
    target_path: String,
) -> anyhow::Result<(CtrlCommand, CmdOptions)> {
    let cmd_options = CmdOptions::default().with_timeout(false);

    //将文件分割并带上这部分的[start,end], 文件在到达时分块写入，写完后校验hash
    let (file_size, mut iter) =
        file_util::read_big_file(file_path.clone(), 1024 * 1024 * 10).await?;

    let data_id = Uuid::new_v4().to_string();

    let mut executor = async_util::new_unbound();

    let data_id_c = data_id.clone();
    let context_c = context.clone();

    let hash = file_util::compute_hash(file_path).await?;

    let receiver = executor
        .submit_with_result(Box::new(move || {
            Box::pin(async move {
                let s = loop {
                    match iter.next().await {
                        None => {
                            break None;
                        }
                        Some(Ok((range, data))) => {
                            let v = Dok::FilePart(range.start, range.end - 1, data).to_buf();

                            match context_c
                                .send_data_with_id(data_id_c.clone(), v.as_ref())
                                .await
                            {
                                Ok(_) => {}
                                Err(e) => break Some((ErrCode::ReadError, e)),
                            };
                        }
                        Some(Err(e)) => {
                            break Some((ErrCode::WriteError, anyhow!(e)));
                        }
                    }
                };
                s
            })
        }))
        .await?; //这里提交任务不可能出错

    executor.finish().await.map_err(|e| anyhow!(e))?;

    //异步执行数据发送中如果出错，这里需要发一个
    let context = context.clone();
    let data_id_c = data_id.clone();
    tokio::spawn(async move {
        if let Ok(Some((code, e))) = receiver.await {
            context
                .send_data_with_id(data_id_c, Dok::Err(code).to_buf().as_ref())
                .await
                .unwrap_or(());
        }
    });
    // 发送hash
    Ok((
        CtrlCommand::SetBigFile(data_id, file_size, hash, target_path),
        cmd_options,
    ))
}
