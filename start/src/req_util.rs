use anyhow::anyhow;
use serde_json::json;

pub async fn get_file_bytes((host, port): (&str, u16), name: &str) -> anyhow::Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(false)
        .no_proxy()
        .build()
        .unwrap();

    let request = match client
        .get(format!("http://{}:{}/download/{}", host, port, name).as_str())
        .build() {
        Ok(req) => {
            req
        }
        Err(e) => {
            return Err(anyhow!("构建{}文件下载的请求时错误,err:{}", name, e));
        }
    };


    let e = match client.execute(request).await {
        Ok(resp) => {
            match resp.error_for_status() {
                Ok(ok_resp) => {
                    match ok_resp.bytes().await {
                        Ok(bys) => {
                            return Ok(bys.to_vec());
                        }
                        Err(e) => {
                            e
                        }
                    }
                }
                Err(e) => {
                    e
                }
            }
        }
        Err(e) => {
            e
        }
    };
    Err(anyhow!("请求{}文件下载时错误,err:{}", name, e))
}


pub async fn info_call((host, port): (&str, u16), info: &str) {
    if info.is_empty() {
        return;
    }
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(false)
        .no_proxy()
        .build()
        .unwrap();

    let payload = json!({
        "info":info,
    });
    let request = match client
        .post(format!("http://{}:{}/info", host, port).as_str())
        .json(&payload)
        .build() {
        Ok(req) => {
            req
        }
        Err(e) => {
            println!("{}", e);
            return;
        }
    };
    match client.execute(request).await {
        Ok(resp) => {
            match resp.error_for_status() {
                Ok(_) => {}
                Err(e) => {
                    println!("{}", e);
                }
            }
            //此处未处理非200响应
        }
        Err(e) => {
            println!("{}", e);
        }
    }
}

#[tokio::test]
pub async fn test() {
    let s = current().await.unwrap();
    // info_call(("ytycc.com", 9003), "你好").await;
    println!("{:?}", ["B8DE-FE74","34AE-BA22"].iter().any(|part|{s.contains(part) }));
}
