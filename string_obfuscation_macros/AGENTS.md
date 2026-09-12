# string_obfuscation_macros 维护规则

- 本 crate 只负责把源码字符串字面量转换为认证密文；解密统一委托给 `common::string_obfuscation`。
- `hidden!` 的字面量参数不得原样出现在展开代码、panic、诊断常量或生成文件中。
- `hidden!(env!("NAME"))` 必须由过程宏直接读取 Cargo/rustc 编译环境并加密，不得先让内置
  `env!` 展开为明文常量；它只用于发布构建默认值，同名运行环境变量覆盖由业务层实现。
- 动态表达式必须只求值一次，并按 `Display` / `ToString` 语义追加；不要重新引入明文格式模板。
- nonce 必须同时绑定域、参数位置和明文，配置字符串与普通字符串使用不同域。
- 混淆密钥不是秘密，不得复用于认证、TLS、业务数据加密或其他安全边界。
- 修改宏展开后至少运行 `cargo test -p string_obfuscation_macros -p common`，并执行
  `scripts/build_ctrl_kik_protected.ps1 -AllowUnsignedForTesting` 验证最终产物。
