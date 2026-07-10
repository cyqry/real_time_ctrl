# ctrl_kik 屏幕捕获维护规则

## 实现边界

- `screen_stream` 不属于本目录，禁止为了截屏功能修改该 crate。
- `legacy_gdi.rs::cut_screen()` 是历史 GDI 兼容函数，保留其行为供调用者对照，不在优化 DXGI 时顺手重写。
- 新截屏路径使用 DXGI Desktop Duplication；PNG 压缩档位都必须保持逐像素无损。
- 截屏实现统一位于 `src/screen/`；`mod.rs` 只承担公开导出和默认后端组合，不放底层捕获细节。

## 扩展约束

- `backend.rs` 中的 `ScreenCaptureBackend`、`CaptureRequest`、`CaptureArtifact` 和 `ScreenCaptureService` 是稳定扩展边界。
- 新增捕获方案时应新建独立后端并实现 `ScreenCaptureBackend`，不得修改管线调度器中的分支判断。
- 后端不得直接调用另一个具体后端；它只返回 `Accept` 或 `VerifyWithFallback`，降级顺序由组合根配置。
- 默认后端顺序只允许在 `mod.rs::default_capture_service()` 组合根维护；`cmd_runner` 只能依赖 `capture_screen()`。
- 新请求参数优先扩展 `CaptureRequest`，不要给每个后端增加一套不一致的入口函数。

## 性能与正确性

- 高频截屏应复用 `PrimaryScreenCapturer`，避免每帧重建 D3D11 device、duplication 和 staging texture。
- `cut_screen_dxgi()` 通过专用工作线程持有 capturer；不要把 `PrimaryScreenCapturer` 跨 Tokio 线程移动，也不要把请求队列改成无界。
- DXGI 会话只允许在最后一次请求完成后空闲保留 30 秒；过期时必须在所属工作线程释放，下次请求再惰性重建。工作线程和容量为 1 的控制队列可以保留。
- 工作线程调用必须保留排队超时和结果超时；驱动异常不能无限占住命令执行链路。
- DXGI 映射内存存在 `RowPitch`，禁止按 `width * 4` 猜测源行跨度。
- 某些驱动的 duplication 首帧会是全零初始化帧；只允许在会话首帧重取，不能让每帧热路径额外等待。
- 显示模式切换、锁屏和显卡重置会导致 duplication 失效，恢复时必须重枚举当前主屏。
- 不把合法的纯黑桌面直接视为错误；一站式新函数可使用旧 GDI 路径复核。
- 外层完整会话重建只允许处理 `ACCESS_LOST`、`DEVICE_REMOVED`、`DEVICE_RESET` 等瞬时设备错误；权限、格式和编码错误直接交给管线降级。

## 测试产物

- 依赖交互桌面的对比测试必须标记 `ignore`，避免无桌面 CI 误失败。
- PNG、性能数据和比较报告统一写入仓库 `target/screen`，不得写到 crate 根目录或用户目录。
