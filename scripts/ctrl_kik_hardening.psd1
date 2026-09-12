@{
    # 日期工具链是生产/CI 的首选名称；ExpectedCommit 是最终不可绕过的版本门禁。
    Toolchain = "nightly-2025-12-14"
    ExpectedCommit = "430d8297c712ca7e8a4866d7ddccf1b71ba5d4d3"
    CompatibleLocalAliases = @("nightly")
    # 这些短词由重新编译后的 Rust 标准库/依赖自身携带，恰好与项目源码字面量碰撞。
    # 它们只从“项目归因”门禁中剔除，仍会记录在 receipt，不能据此宣称整个 PE 无明文。
    KnownRuntimeStringCollisions = @(
        ".exe",
        "127.0.0.1",
        "channel",
        "cmd.exe",
        # 以下三个词只出现在 #[cfg(test)] 截屏断言/报告字段中；release PE 的同名短词来自图像依赖。
        "fallback",
        "height",
        "other",
        "server",
        "session",
        "stale",
        "true",
        "value",
        "worker",
        "width"
    )
}
