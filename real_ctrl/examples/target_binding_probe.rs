//! 原仓库链路验收探针：真实 pinned TLS/HMAC 会话，向两台真实 Kik 验证目标绑定。
//! 只读取 E2E 脚本注入的本机端点和测试身份；不依赖 Android 或复制 App 实现。
use anyhow::{anyhow, bail, ensure, Context, Result};
use bytes::BytesMut;
use common::{
    command::{Command, CtrlCommand, SysCommand},
    config::Config,
    ltc_codec::{LengthFieldBasedFrameDecoder, CONTROL_MAX_FRAME_LENGTH},
    message::{
        init_frame::InitFrame,
        kik_resp::{ClientSuccessResp, KikResp},
    },
    protocol::{self, BufSerializable, CmdOptions, ReqCmd},
    secure_transport::{connect_real_ctrl, BoxedAsyncRead, BoxedAsyncWrite},
    session_auth::{ctrl_auth_proof, random_nonce_hex},
};
use ctrl_common::{
    cmd_resp_info::{ServerCapabilities, SysNow},
    ctrl_frame::Frame,
    ctrl_resp::{CmdResp, Resp, ServerResp, ServerSuccessResp},
};
use futures::StreamExt;
use std::{env, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    time::timeout,
};
use tokio_util::codec::FramedRead;
use uuid::Uuid;

struct Client {
    reader: FramedRead<BufReader<BoxedAsyncRead>, LengthFieldBasedFrameDecoder>,
    writer: BoxedAsyncWrite,
}
impl Client {
    async fn connect(config: &Config) -> Result<Self> {
        let parts = connect_real_ctrl(config).await?;
        let mut client = Self {
            reader: FramedRead::new(
                BufReader::new(parts.reader),
                LengthFieldBasedFrameDecoder::new_with_max_frame_len(CONTROL_MAX_FRAME_LENGTH),
            ),
            writer: parts.writer,
        };
        let nonce = random_nonce_hex();
        client
            .send(InitFrame::CtrlAuthStart {
                account_id: config.id.account_id().into(),
                instance_id: config.id.instance_id().into(),
                client_nonce: nonce.clone(),
            })
            .await?;
        let server_nonce = match InitFrame::from_buf(client.next().await?) {
            Some(InitFrame::CtrlAuthChallenge(value)) => value,
            _ => bail!("测试认证挑战失败"),
        };
        let proof = ctrl_auth_proof(
            config.id.control_plane_secret(),
            config.id.account_id(),
            config.id.instance_id(),
            &nonce,
            &server_nonce,
        );
        client
            .send(InitFrame::CtrlAuthProof {
                client_nonce: nonce,
                proof,
            })
            .await?;
        ensure!(
            matches!(
                InitFrame::from_buf(client.next().await?),
                Some(InitFrame::CtrlAuthSession(_))
            ),
            "测试身份认证失败"
        );
        Ok(client)
    }
    async fn next(&mut self) -> Result<BytesMut> {
        timeout(Duration::from_secs(15), self.reader.next())
            .await?
            .ok_or_else(|| anyhow!("控制连接关闭"))?
            .map_err(Into::into)
    }
    async fn send(&mut self, frame: impl BufSerializable) -> Result<()> {
        let bytes = protocol::transfer_encode_frame(frame);
        timeout(Duration::from_secs(15), self.writer.write_all(&bytes)).await??;
        Ok(())
    }
    async fn response(&mut self, id: &str) -> Result<CmdResp> {
        loop {
            match Frame::from_buf(self.next().await?) {
                Some(Frame::Resp(response)) => {
                    ensure!(response.get_cmd_id() == id, "关联 ID 串线");
                    return Ok(response);
                }
                Some(Frame::Ping | Frame::Pong) => self.send(Frame::Pong).await?,
                _ => bail!("未预期的服务端帧"),
            }
        }
    }
    async fn request(&mut self, target: Option<&str>, command: Command) -> Result<CmdResp> {
        let id = Uuid::new_v4().to_string();
        let request = ReqCmd::new(id.clone(), CmdOptions::default(), command);
        self.send(match target {
            Some(target) => Frame::TargetedCmd(target.into(), request),
            None => Frame::Cmd(request),
        })
        .await?;
        self.response(&id).await
    }
    async fn capabilities(&mut self) -> Result<()> {
        let id = Uuid::new_v4().to_string();
        self.send(Frame::Capabilities(id.clone())).await?;
        let response = self.response(&id).await?;
        let capabilities: ServerCapabilities = serde_json::from_str(&success_info(&response)?)?;
        ensure!(
            capabilities.target_bound_command_v1,
            "服务端未声明目标绑定能力"
        );
        Ok(())
    }
}
fn success_info(response: &CmdResp) -> Result<String> {
    match response.get_resp() {
        Resp::Server(ServerResp::Success(ServerSuccessResp::Info(info)))
        | Resp::Kik(KikResp::Success(ClientSuccessResp::Info(info))) => Ok(info.clone()),
        _ => bail!("预期成功响应"),
    }
}
fn assert_rejected(response: &CmdResp) -> Result<()> {
    ensure!(
        matches!(response.get_resp(), Resp::Server(ServerResp::Error(..))),
        "目标拒绝必须在服务端完成，不能已送往 Kik"
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    timeout(Duration::from_secs(90), probe())
        .await
        .context("目标绑定探针总超时")??;
    Ok(())
}

async fn probe() -> Result<()> {
    let config = real_ctrl::runtime_config::connection_config()?;
    ensure!(config.server_host == "127.0.0.1", "探针只允许本机 E2E 端点");
    let target_a = env::var("RTC_E2E_TARGET_A")?;
    let target_b = env::var("RTC_E2E_TARGET_B")?;
    let mode = env::var("RTC_E2E_TARGET_PHASE")?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()?;
    let mut client = Client::connect(&config).await?;
    client.capabilities().await?;
    let mut assertions = vec!["authenticated target-bound capability query"];
    if mode == "before" {
        // 两个 Kik 有不同工作目录；cd 的结果证明真正执行请求的是目标 A/B，而非当前选择。
        for (selected, target, expected) in [
            (&target_b, &target_a, root.clone()),
            (&target_a, &target_b, root.join("target/e2e/kik-b")),
        ] {
            success_info(
                &client
                    .request(None, Command::Sys(SysCommand::Use(selected.clone())))
                    .await?,
            )?;
            let actual = success_info(
                &client
                    .request(Some(target), Command::Exec("cd".into()))
                    .await?,
            )?;
            ensure!(
                PathBuf::from(actual.trim()).canonicalize()? == expected.canonicalize()?,
                "命令被错误地发往当前选择"
            );
            let now: SysNow = serde_json::from_str(&success_info(
                &client.request(None, Command::Sys(SysCommand::Now)).await?,
            )?)?;
            ensure!(
                matches!(now, SysNow::Kik(kik) if &kik.id == selected),
                "显式目标命令不能改变会话默认选择"
            );
        }
        assertions.push(
            "explicit A and B targets override the other current selection without changing it",
        );
        let path = root.join("target/e2e/directory with spaces 中文");
        let listing = success_info(
            &client
                .request(
                    Some(&target_a),
                    Command::Ctrl(CtrlCommand::Ls(path.to_string_lossy().into())),
                )
                .await?,
        )?;
        ensure!(
            listing.contains("space-marker.txt"),
            "带空格/中文目录读取失败"
        );
        assertions.push("target-bound directory preserves spaces and Unicode");
    } else if mode == "after" {
        let now: SysNow = serde_json::from_str(&success_info(
            &client.request(None, Command::Sys(SysCommand::Now)).await?,
        )?)?;
        ensure!(
            matches!(now, SysNow::Kik(kik) if kik.id == target_b),
            "故障后默认目标应为 B"
        );
        let marker = root.join("target/e2e/unintended-target.txt");
        ensure!(!marker.exists(), "测试前意外执行标记必须不存在");
        let command = format!("echo unintended>\"{}\"", marker.display());
        for _ in 0..24 {
            assert_rejected(
                &client
                    .request(Some(&target_a), Command::Exec(command.clone()))
                    .await?,
            )?;
        }
        assert_rejected(
            &client
                .request(
                    Some(&target_a),
                    Command::Ctrl(CtrlCommand::Screen(String::new())),
                )
                .await?,
        )?;
        let empty_hash = <sha2::Sha256 as sha2::Digest>::digest([]).to_vec();
        assert_rejected(
            &client
                .request(
                    Some(&target_a),
                    Command::Ctrl(CtrlCommand::SetBigFile(
                        Uuid::new_v4().to_string(),
                        0,
                        empty_hash,
                        marker.to_string_lossy().into(),
                    )),
                )
                .await?,
        )?;
        let output = success_info(
            &client
                .request(
                    Some(&target_b),
                    Command::Exec("echo target-b-still-usable".into()),
                )
                .await?,
        )?;
        ensure!(
            output.contains("target-b-still-usable") && !marker.exists(),
            "失败请求串到 B 或许可未回收"
        );
        assertions.push(
            "offline A rejects Exec, screenshot and upload without executing on B; permits recover",
        );
    } else if mode == "denied" {
        assert_rejected(
            &client
                .request(Some(&target_b), Command::Exec("echo must-not-run".into()))
                .await?,
        )?;
        assertions.push("authenticated restricted account cannot target a denied Kik");
    } else {
        bail!("未知测试阶段");
    }
    println!(
        "{}",
        serde_json::to_string(
            &serde_json::json!({"success":true,"phase":mode,"assertions":assertions})
        )?
    );
    Ok(())
}
