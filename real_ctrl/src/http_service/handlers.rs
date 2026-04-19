use anyhow::anyhow;
use bytes::Bytes;
use serde_json::{json, Value};
use spring_web::axum::body::Body;
use spring_web::axum::http::{header, StatusCode};
use spring_web::axum::response::{IntoResponse, Response};
use crate::input_command::{InputCommand, InputCtrlCommand, RemoteResp};
use crate::local_client::client::invoke;
use crate::pipe::pipe_common::ServerResponse;

pub async fn health_check() -> &'static str {
    "OK"
}

pub async fn hello_world() -> Value {
    json!({
        "message": "Hello, World!"
    })
}

pub(crate) async fn sys_list() {
    todo!()
}


// let response = local_client::client::invoke(&InputCommand::Sys(SysCommand::Now)).await?;
// println!("{:?}", response);
// let response = local_client::client::invoke(&InputCommand::Sys(SysCommand::List)).await?;
// println!("{:?}", response);
//
// let response = local_client::client::invoke(&InputCommand::Ctrl(InputCtrlCommand::Ls("D:\\Ax201".to_string()))).await?;
// println!("{:?}", response);
// let response = local_client::client::invoke(&InputCommand::Exec("ipconfig".to_owned())).await?;
// println!("{:?}", response);

pub(crate) async fn screen() -> anyhow::Result<impl IntoResponse> {
    let encoded_name = percent_encoding::utf8_percent_encode("screen.png", percent_encoding::NON_ALPHANUMERIC);
    let disposition = format!("attachment; filename=\"{}\"", encoded_name);
    let resp = invoke(&InputCommand::Ctrl(InputCtrlCommand::Screen("_".to_owned()))).await?;
   let v = match resp {
        ServerResponse::Success(RemoteResp::SuccessData(v)) => {
            v
        }
        _ => {
            return Err(anyhow!("截屏失败"));
        }
    };
    let body = Bytes::from_owner(v);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_DISPOSITION, disposition)
        .body(Body::from(body))?)
}