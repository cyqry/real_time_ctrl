//! 验证编译后的锁路径确实来自集中配置，而非发布脚本或父进程继承的环境变量。
//! 回归时可在 Cargo 启动前设置旧 RTC_CTRL_KIK_BUILD_LOCK_PATH，再执行本测试。

use common::generated::encrypted_strings::LOCK_FILE_PATH;

#[test]
fn lock_file_path_comes_only_from_config_json() {
    // 对比实际生成并解密的值，覆盖“读取配置 -> 构建期处理 -> 运行时取值”整条链路。
    // 不在测试进程中修改环境变量：构建脚本早已运行，届时修改无法验证构建期优先级。
    let config: serde_json::Value = serde_json::from_str(include_str!("../config.json"))
        .expect("common/config.json 必须是合法 JSON");
    let expected = config["strings"]["LOCK_FILE_PATH"]
        .as_str()
        .expect("LOCK_FILE_PATH 必须在 common/config.json 中配置为字符串");

    assert_eq!(LOCK_FILE_PATH(), expected);
}
