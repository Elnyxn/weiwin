# Win+H CarbonDictation bridge

Rust `cdylib`，仅替换已验证版本 `TextInput.CarbonDictation.dll` 对 Speech SDK 的 IAT。库加载不自动安装；`DllMain` 只返回成功。代码不枚举或打开其他进程，不做 inline hook，不处理 UI 或录音。

## 导出 ABI

```rust
#[repr(C)]
pub struct InstallArgs {
    pub magic: u64,          // 0x5758484252494447
    pub request_write: usize,
    pub event_read: usize,
}
// Windows x64 extern "system"，三个函数均返回 u32。
WinhBridgeInstall(*const InstallArgs)
WinhBridgeDisable(*const core::ffi::c_void)
WinhBridgeDiagnostics(*const core::ffi::c_void)
```

InstallArgs 指针必须有效且可读。magic 正确，两个句柄非零、非 INVALID_HANDLE_VALUE 且互不相同后，所有权即转交 DLL，**包括此后所有失败返回路径**；host 不得再关闭远端副本。参数结构校验失败时未接管。两句柄必须已经 DuplicateHandle 进入当前 TextInputHost，分别为匿名管道写端和读端，无 named pipe 或 loopback。

安装先验证 Carbon 和 core 的磁盘 SHA256、PE64 及全部 Speech imports，再改变 IAT。当前 Speech.core 是 **delay import**：已解析槽必须等于 core 导出地址；未解析槽必须等于已校验磁盘 delay thunk 的精确重定位地址。任意第三方地址均拒绝。修改保存真实原槽，每次 VirtualProtect 后恢复原页保护；失败反向回滚已修改槽。启动入口先替换，Disable 最后恢复。DLL/core/Carbon 会 pin，避免在途代码被卸载。

| Install/Disable 状态 | 含义 |
| --- | --- |
| 0 | 成功；未安装时 Disable 也返回 0 |
| 1 | 参数未接管 |
| 2 | 接管后管道类型检查失败 |
| 3 | 已安装；新交付句柄已关闭 |
| 4 | 目标模块/导出不可用 |
| 5 | 模块身份不匹配 |
| 6 | 导入覆盖或原槽不匹配 |
| 7 | IAT 修改失败，回滚成功 |
| 8 | 回滚未完全成功，不能宣称恢复完成 |
| 9 | 工作线程创建失败，IAT 已回滚 |
| 10 | 活动会话/在途回调/未释放 fake handle，或旧读线程未退出 |

Disable 只允许引擎空闲、回调排空且 fake handles 全部释放。host 先停止 broker、关闭 event-write 产生 EOF，再调用 Disable；随后 request writer 退出，host 的 request-read 得到 EOF。Disable 不卸载自身，也不终止任何线程或进程。旧 event reader 退出后允许再次 Install；尚未退出时明确返回 10。

## IPC 与生命周期

host 可复用 `wechat_winh_bridge::ipc::{BridgeRequest, BridgeEvent, BridgeError}`。请求为 `cmd` tagged enum，Start/Stop 都含 `reco:u64,generation:u64,language:String,device:String`。事件为 `event` tagged enum：

- started：身份与 `session:String`。
- partial/final：身份与 `text:String,offset:u64,duration:u64`。
- stopped：身份。
- error：身份与 `error:{message:String,code:u32}`。

每行一个 JSON，单行最多 64 KiB。language 从真实 recognizer 的属性 3001 读取；device 为空表示 host 选择默认设备，不编造未证实的设备属性 ID。generation 单调递增，上一轮及未知 recognizer 的事件丢弃。

保留真实 config、audio config、recognizer 工厂及句柄；回调 setter 先透传 SDK，成功后保存函数/context。未观察到setter时，从版本门控的SDK事件对象只读恢复原callback/context；显式setter(None)不恢复。无法取得callback时返回已完成的自有async，并经已有取消回调报告，不启动任何识别，不向Carbon传播NOT_IMPL，不回退Microsoft ASR。continuous start 创建自有 async handle，收到 broker started 才完成。stop 等待final回调排空后才完成 stopped；Draining期间重复stop共享终结状态。回调重入 stop/wait 时采用 broker acknowledgement。

输入保护先结束后台、原生组件稍后才调用 stop 时，补发一次 SessionStopped 确认该 stop；重复 stop 不重复派发。这样原生面板能完成暂停并再次通过麦克风启动。

partial reason=2、final reason=3。每句 partial/final 共用 result_id，final 后下一句换 ID。首次文字前合成 speech-start，final 后合成 speech-end。SDK 回调在独立有序线程执行，不持 registry、session 或 pipe mutex。

fake handles 使用永久可识别的 0x5758 高位标签和不复用的计数器；释放后的标签永不透传 SDK。event 按回调作用域借用：消费者可主动 release，回调返回后清理仍残留的 event。event_get_result 返回独立拥有 Arc 的 result handle，event 到期后仍有效，直到消费者释放。result_get_property_bag 每次创建真正 SDK property bag，拥有权不共享，微软的 getter/free/release 原样工作。

有界 request 队列 64、callback 队列 128、fake handles 512；队列满用 try_send 立即使会话失败、唤醒 waiters，SDK 线程不阻塞入队。once、keyword 的启动分支返回 NOT_IMPL；未知 start/stop import 使整个安装失败。所有真实 event/result/async 句柄透传保存的 SDK 导出函数。

CancellationErrorCode_RuntimeError 使用 9，依据微软原始绑定：
https://raw.githubusercontent.com/microsoft/cognitive-services-speech-sdk-go/master/common/cancellation_error_code.go

## Diagnostics 位图

只读历史位，成功安装新一轮时清空；没有日志或文字内容。

| 位 | 含义 |
| --- | --- |
| 0 | 当前 installed/ready |
| 1 | 曾捕获 recognized callback |
| 2 | 曾捕获 recognizing callback |
| 3 | continuous start 被调用 |
| 4 | start 时缺少已捕获的 recognized callback |
| 5 | once 分支被阻止 |
| 6 | continuous start/stop 返回错误 |
| 7 | IPC/队列/dispatcher 失败 |
| 8 | keyword 分支被阻止 |
| 9 | 尚未 ready 时请求 continuous start/stop |
| 10 | continuous stop 被调用 |
| 11 | 接入前的callback恢复成功 |
| 12 | callback恢复布局校验失败 |
| 13 | 透传停止接入前已经存在的原SDK会话 |

## 验证边界

离线测试覆盖 C UTF-8 缓冲区状态、fake 句柄失效隔离、真实 Carbon delay-import fixture、未知入口拒绝、跨轮隔离、每句 result_id、final 排空、无锁回调重入和有界队列失败。读取实际 Carbon fixture 时显式设置：

```sh
WXB_BRIDGE_TEST_CARBON_PATH=/mnt/c/Windows/SystemApps/SxS/MicrosoftWindows.61869836.InpApp_cw5n1h2txyewy/TextInput.CarbonDictation.dll cargo test -p wechat-winh-bridge --lib
cargo check -p wechat-winh-bridge --target x86_64-pc-windows-gnu
```

隔离宿主已用真实SDK预注册callbacks，通过真实Carbon IAT验证启动、partial/final/canceled、真propertybag、stop与Disable恢复。主程序另在原生Win+H+标准EDIT窗口完成云端WAV识别文字提交，进程PID保持不变。麦克风采集另测；未宣称额外实时口述或原生音量波形验收。

## 已注册callback的只读恢复依据

Core hash门控下，singleton `*(core+0x2031B0)` 是索引树，以key0x2BB8136A取得manager，再从manager+0x30树按真实hreco精确查找B。沿虚基调整取得A，其vtable[1]必须是core+0xF0F50；只读vtordisp解析Q，不调用内部虚函数。事件基址按虚基表调整，成员偏移为started8、stopped0x78、recognizing0x388、recognized0x3F8、canceled0x468、speech-start0x2A8、speech-end0x318。

事件map最多一项，std::function的functor指针在node+0x60；functor vtable必须为core+0x197CD8(session)或0x197DD8(其余)。callback/hreco/context位于+8/+0x10/+0x18，消费者356C/3348也逐一证明对应原C callback参数。恢复只沿精确索引读两次稳定快照，验证hreco和可执行函数页，不扫描内存、不写SDK对象、不调用内部dispatch。`research/isolated_sdk.py --inspect`覆盖全部七类、替换context和清空None对照。
