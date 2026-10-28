---
AIGC:
    Label: "1"
    ContentProducer: 001191440300708461136T1XGW3
    ProduceID: 04526fdd866a8c5c9aceee6b83ed2c05_a365b46ed1bb11f1b84e52540064ee0f
    ReservedCode1: KnywNPxekYS190ngNQiPht9kMa6TJ6It/MklPWmE+RnGHZ0tNigDweJ1wkPxMTlwQXnWmVWhEjiUVfoTsYIoQWqgidTyKk1N1M0CKfL9zsejMsHWzXMxpdg5bzIAHObnOdpihDgmspJSY+DguKRuDOa/kMsTUC9LX7VkR2ZUVsyLYFGlKFXLMD8+dYU=
    ContentPropagator: 001191440300708461136T1XGW3
    PropagateID: 04526fdd866a8c5c9aceee6b83ed2c05_a365b46ed1bb11f1b84e52540064ee0f
    ReservedCode2: KnywNPxekYS190ngNQiPht9kMa6TJ6It/MklPWmE+RnGHZ0tNigDweJ1wkPxMTlwQXnWmVWhEjiUVfoTsYIoQWqgidTyKk1N1M0CKfL9zsejMsHWzXMxpdg5bzIAHObnOdpihDgmspJSY+DguKRuDOa/kMsTUC9LX7VkR2ZUVsyLYFGlKFXLMD8+dYU=
---

# 变更记录

格式参考 Keep a Changelog，版本遵循语义化版本。`AIRB` 格式与 `pluginApi` 的兼容性单独标注。

## [Unreleased]

M-UI-4 第一刀：把动画从「只统计条数」推进到「能按时间点求值」。这一刀不碰渲染，也不动 `AIRB` 布局与 `pluginApi`——判据是：给定一条 AIR 轨道和任意时间，能算出该节点该时刻的 TRS，且这个结果能被单测钉住。

### 新增

- **轨道采样 `AirTrack::sample_at`**：按时间求值并处理四种边界——越界不外推（两端保持）、单关键帧视为常量、`time` 为 `NaN` 退回首帧（`NaN` 参与比较会得到未定义摆位）、时间轴重复时不除零（取后一帧）。`linear` 走线性插值，`rotation` 走球面插值，`step` 保持前一帧。
- **`AirTrack::interpolation`（`linear` / `step`）**：省略即 `linear`，老 AIR 文件反序列化时自动补齐，不需要迁移。`glTF` 的 `STEP` 采样器现在原样带进 AIR——它表达的是「保持」，降级成线性会把闪光/开关类动画抹平；`CUBICSPLINE` 的切线在提取阶段就已丢弃，仍按 `linear` 走，不假装支持三次插值。FBX 侧轨道由 ufbx 按帧烘焙，固定 `linear`。
- **片段级接口**：`effective_duration`（`duration` 缺失或短于轨道末端时按末端兜底）、`wrap_time`（循环取模，负时间也能取到正确相位）、`apply_at`（把某时刻采样写回节点 TRS，旋转写回前归一化）、`keyframe_count` / `driven_node_count`（摘要与预算用）。
- **`util::slerp_quat`**：最短路径球面插值。`q` 与 `-q` 表示同一旋转，按点积判号取反，否则会绕远路（180° 的插值会变成 260° 的回转）。
- **导入期动画校验**：`validate` 分级报告动画问题。阻断级（`errors`）为属性名不是 `translation` / `rotation` / `scale`、轨道目标节点下标越界、时间点数与关键值数不等、时间轴非单调；提示级（`issues`）为片段无轨道、常量轨道、旋转关键帧未归一化、未知插值方式、`duration` 与轨道末端不符。这些错误此前全部静默通过，表现为「动画在面板里有名字，播出来不动」。
- **`inspect_asset` 摘要的动画段**：每片段补出有效时长、关键帧数、被驱动节点数、`STEP` 轨道数——判断一段动画值不值得导，靠这几个数比看 `duration` 有用。
- **单测 +13 条**：`air` 侧覆盖线性/端点/保持、`NaN`、`step`、单关键帧、球面插值与 `q`/`-q` 同解、重复时间戳、`duration` 兜底、`wrap_time` 负相位、`apply_at` 三通道写回与归一化、校验分级、`interpolation` 的序列化省略与反序列化补齐；`air_bin` 侧覆盖动画块经 `AIRB` 往返后采样值逐点一致；`gltf` 侧覆盖插值模式映射（`STEP` 保留、`CUBICSPLINE` 降级）。插件主 crate 57 条、视口 bin 50 条全绿。

### 变更

- `AirTrack` 新增字段 `interpolation`，`Default` 为空串（序列化时省略），构造处统一走 `crate::air::INTERPOLATION_LINEAR` / `INTERPOLATION_STEP` 常量，避免各处手写字符串。

### 已知限制

- 采样是逐轨道独立求值，不含骨骼层级与蒙皮：`apply_at` 只把 TRS 写到节点自身，父子叠加与 `skin` 权重留给第二刀（视口骨骼矩阵 + 蒙皮着色器）。
- 只支持 `linear` / `step`。`CUBICSPLINE` 的切线在 AIR 里没有存放位置，降级是既定口径，不是待办。

## [0.4.0] - 2026-10-28

本版把「面板里只有一棵描述树」补成了「能看见形状」：新增独立视口进程 `asset3d-gui.exe`，宿主按 `views[].external` 拉起它，两者以命名管道跑会话协议 `asset3d.session/1`；视口从「空窗口」一路做到网格 + 材质贴图 + 轨道相机 + 包围盒取景。`pluginApi` 升到 `1.1`（新增 `views` 与 `commands`），`AIRB` 仍为 `v1`——视口只消费既有 AIR 字段，不改格式。

### 新增

- **视口进程 `asset3d-gui.exe`**（独立 `[[bin]]`，winit + wgpu）：不与插件同进程——winit 的事件循环要独占线程，塞进宿主 stdio 帧泵里两边只会互相等死。三种启动形态：`--pipe <name>`（IDE 托管）、`--standalone <asset>`（独立看图，同资产只开一个窗口）、无参数（双击，抢 `Local\asset3d.viewport` 互斥体，抢不到就把已有窗口置前后退出）。退出码：参数错误 `2`、路径错误 `3`、正常 `0`。
- **会话协议 `asset3d.session/1`**：行分隔 JSON `{"v":1,"op":"...","seq":N,...}`，op 为 `open` / `close` / `focus` / `ping` / `screenshot`。三条硬规则落在代码里：未知 op 与未知字段一律忽略（协议要能往前加）、`seq=0` 为单向通知不回包、`seq>0` 按同 `seq` 回；`screenshot` 本期是占位，明确回 `error{code:"unsupported"}` 而不是静默超时；握手 `ready.capabilities` 只报 `["static"]`。
- **网格渲染**：顶点带位置/法线/UV，按 AIR 的 `material_parts` 把同一 mesh 切成若干索引区间，一段一个 draw call，材质相同的分区共用 bind group 与 uniform。
- **材质贴图**：`scene.rs` 解析纹理解码，`gfx.rs` 新增 sampler + texture bind group。贴图读取优先级为「内嵌 base64 → 资产同目录的相对 `uri`」，外部 `uri` 只允许落在资产同目录，越界当读不到；UV 按 AIR 的 `units.uv_origin` 决定是否翻 v（读字段不猜格式名）；缺贴图退回 baseColor 纯色，贴图损坏只记一条 `notes`，不放大成打不开。
- **轨道相机与取景**：`Orbit::fit` 按包围球自动定距离与近远裁剪面，左键拖动转视角、滚轮推拉，不同尺度资产都不用先手动调距离。
- **参考地板与坐标轴**：1 米一格地面网格 + X 轴红 / Z 轴蓝；空窗口与 `Esc` 清空后都靠它证明程序还活着。
- **面板视图入口 `view_render` 工具**：把资产折成声明式描述树（概览/材质/贴图/动画/骨骼/导入问题/动作按钮）交给宿主面板渲染，`path` 省略时列出工作区可打开资产。
- **`open_viewport` 命令**：把资产路径发给已启动的视口；视口没起来先由宿主拉起进程，`pipe` 省略时发默认管道（手动双击那个窗口）。
- **单测补 13 条**：`scene` 侧覆盖分区切分（`material_parts`）、UV 翻转、越界索引丢弃、贴图解码降级、`percent_decode` 等；`asset3d-gui` 侧共 38 条、插件主 crate 44 条全绿。

### 变更

- 版本号升到 `0.4.0`（`Cargo.toml` 为唯一真源），`pluginApi` 由 `1.0` 升到 `1.1`，`dist/` 打包清单多一件 `asset3d-gui.exe`。
- `Cargo.toml` 新增视口依赖与 `image` 的 `png` / `jpeg` 解码特性（贴图在视口侧要展开成 RGBA，插件侧仍只搬字节）。
- `README.md` 新增「视口：asset3d-gui.exe」一节（启动形态、交互、协议表、画面构成、取舍），工具表补到 6 个并单列 `commands`，版本与已知限制同步更新。
- 视口复用插件源文件的 `air` / `fbx` / `gltf` / `util` / `pipe` 模块（`#[path]` 引入），保证「宿主读出来的 AIR」与「视口画出来的形状」永远一致，不复制第二份解析器。

### 已知限制

- 视口只做**静态渲染**：不播动画、不做蒙皮；`capabilities` 只报 `static`，动画留给 M-UI-4。
- 贴图超过 4096×4096 不解码，画面上退回纯色并在 `notes` 里给原因。
- 不排序半透明（alpha 一律当不透明）、不打 mipmap（远处贴图有摩尔纹）。
- `screenshot` op 为协议占位，本节要图请用系统截图。

## [0.3.0] - 2026-10-27

本版把「单体资产读不进来」这个天花板拆掉了：宿主新增了 `host/fs.read` 的 `offset` 与 `host/fs.write` 的 `append` / `offset`，插件侧改成循环分片读写，上限从约 720 KiB 抬到 96 MiB，并对旧宿主保留整文件降级路径。`pluginApi` 仍为 `1.0`，`AIRB` 仍为 `v1`——只改读写方式，不改格式与约定。

### 新增

- **分片读**：`Host::read_asset` 按 `READ_WINDOW_BYTES = 512 KiB`（取宿主 `MAX_READ_WINDOW_BYTES`）循环调用 `host/fs.read` 并带 `offset`，严格依回包里的 `bytes` / `eof` / `size` 推进：`bytes` 为 0 或 `size` 与已读字节数不符（文件在读取期间被改）都直接报错，绝不拼一个错位资产。512 KiB base64 后约 683 KB，仍在 960 KiB 帧载荷预算内，因此这一侧的真实瓶颈是宿主窗口而非帧预算。
- **分片写**：`Host::write_sharded` 先按路径现算片长（`write_chunk_bytes` = 帧预算反推的原始字节与 512 KiB 取小，与读侧同片长），装得进一帧照旧整文件写，装不进则第 0 片以整替换建文件、后续片按 `append` / `offset` 定位续写；任一分片失败即清理已写半截文件并返回可读原因（不留大小对、内容截断的产物）。
- **上限抬到 96 MiB**：`MAX_ASSET_BYTES` 由帧预算的副产物（约 720 KiB）改为按真实能力设定——512 KiB 窗口 × 200 次调用 ≈ 100 MiB，取 96 MiB 留余量。超限由插件自己拦下，诊断里写明实际大小、上限、以及上限怎么算出来的（读满需 192 次 `host/fs.read`），并建议拆 `.gltf` + 外部 `.bin`，不再把问题甩给宿主去判协议违规。
- **旧宿主降级**：`host/fs.read` / `host/fs.write` 因不认 `offset` / `append` 返回参数错误时，自动退回整文件路径；装得进一帧照常工作（行为与 0.2.0 一致），装不进则明确报「宿主版本过旧」并给出资产真实大小与当前宿主单帧上限，提示升级宿主即可解决。
- **联调台支持分片语义**：`dev/harness.py` 的模拟宿主实现 `offset` 读与 `append` / `offset` 写（含 1 MiB 帧闸门、200 次调用配额、`--legacy` 旧宿主模式），新增 `huge_1m.glb`（1.04 MiB）与 `huge_3m.glb`（2.87 MiB）两个量级的端到端用例，`inspect_asset` / `export_asset` / `preview_asset` 三工具全通，并逐字节核对内嵌贴图哈希、校验分片数与单片长度、核对半截文件不残留。
- 新增夹具脚本 `dev/make_huge_fixture.py`（随机噪声 PNG 撑体积，贴图哈希可复算）。
- 单测补到 35 条：新增「资产上限反映分片能力」「写分片装得进一帧且不超配额」。

### 变更

- 版本号升到 `0.3.0`（`Cargo.toml` 为唯一真源，`plugin.json` 由 `asset3d --manifest` 重新生成，无 BOM）。
- `README.md` 补「分片读写：为什么要循环，以及上限怎么算」与「旧宿主降级」两节，已知限制改写为 96 MiB 与旧宿主两条。
- `dev/harness.py` 的 `run_huge` 用显式 `.air.json` 作 `out`，避免被工具按后缀规范成 `.json` 后与断言文件名对不上（工具行为正确，是断言写错了）。

### 已知限制

- 单个资产超过 96 MiB 仍读不进来：上限由「宿主单次窗口 × 单次调用配额」决定；且宿主单次调用有 120 秒硬超时，接近上限的资产能读但很慢，仍建议拆 `.gltf` + 外部 `.bin`。
- 旧宿主（不认识 `offset` / `append`）上仍有约 720 KiB 天花板。
- 预览不播动画、不做蒙皮变形；`maxTriangles` 只影响预览，导出始终完整。

## [0.2.0] - 2026-10-27

本版围绕「不把宿主搞挂、不静默丢数据」收紧：宿主 1 MiB 单帧是硬顶，触碰即协议违规、插件被踢下线，因此所有大产物都改走预算内分片；同时把「预览能看」与「导出完整」彻底分开。

### 修复

- **预览分片突破宿主 1 MiB 上限**：分片预算改为按真实路径与编码算，每片再留 1 KiB 余量（`SHARD_MARGIN`），保证每片严格低于预算；此前按固定切片，最后一片会超限并被宿主判协议违规。HTML 骨架也走同一条闸门，超限时明确报错而不是写出半成品。
- **预览不再烘焙动画轨道**：预览页只画静态网格，动画没有任何消费方，却会白占分片体积、把本来能单片的资产顶成多片。`preview_asset` 现在按 `animations: false` 载入，回包与页面都写明「动画轨道未载入」；`export_asset` 照常带动画。
- **贴图超限时不再吞数据**：贴图超过单张 6 MiB / 合计 24 MiB 时，此前 `take()` 之后直接丢弃，导致导出结果比源资产少一张图。现在把字节原样放回 `texture.data_base64`，只在预览里不内嵌，并在回包里点名是哪几张。
- **单网格超三角预算被整网格丢弃**：`maxTriangles` 触发时，此前把整个网格扔掉，用户看到的是空白页面。现在先丢小网格、保住大件；只剩一个网格时改为**截断索引并压掉用不到的顶点**，HUD 与回包标注 "26450 → 1000" 之类的降级明细，统计数字跟着截断后的几何走。
- **viewer 的 MIME 判定与失败容错**：贴图 MIME 改为先看魔数（PNG/JPEG/GIF/WebP）、认不出才回落到声明值，避免贴图全黑；分片缺失/损坏时 `AIR` 置为 `null` 并给出面板提示（写明缺哪个文件、必须与 HTML 同目录），不再白屏或画出错乱几何。
- **`estimate_binary_bytes` 漏算蒙皮与动画**：估算此前只按几何平铺算，带骨骼的资产会被低估。现在计入每关节 64 B 的逆绑定矩阵与动画采样头部开销，估算值不再低于真实落盘体积。
- **handshake 期 `host/workspace.info` 阻塞 20 秒**：该调用改用 2 秒短超时（`HOST_CALL_TIMEOUT_SHORT`），失败就降级为「无工作区」继续握手，其余宿主调用仍为 20 秒。宿主不响应时插件不再卡死在启动阶段。

### 新增

- 单测补到 33 条，覆盖此前只有联调台能验的路径：
  - `gltf`：accessor `count` 上限、sparse 降级去重、等于上限的边界；
  - `rpc`：帧预算「刚好等于」放行 / 略超拦截、base64 编码长度口径、握手短超时非零且远小于 20 秒；
  - `view`：分片严格低于预算、单网格超预算截断不丢、贴图超限放回并点名、三角预算只在超限时才裁；
  - `fbx`：四边形三角化与绕序、索引越界兜底、超过 4 根骨按权重截断并归一化、零权重/NaN 兜底。
- `dev/harness.py` 增两条真链路断言：带节点 TRS 的蒙皮资产（2 关节 / 2 份 IBM / 1 段动画，顶点必须留在局部空间，节点位移不能被烘进顶点）；超 1 MiB 预览必须分片、每片低于预算、骨架与分片都落盘。
- 新增夹具 `dev/make_skin_fixture.py`（`skin.glb`，带节点位移+旋转）、`dev/make_big_fixture.py`（`big.glb`，26450 三角形，专压分片路径）。
- `README.md`、`CHANGELOG.md`、`.gitignore`。
- `.gitignore` 覆盖 `target/`、`.tmp-ws/`、`*.log` / `*.err` / `*.out`、`__pycache__/`。

### 变更

- 版本号统一升到 `0.2.0`。真源只有 `Cargo.toml`，清单与握手回包都取 `env!("CARGO_PKG_VERSION")`，三处因此必然一致；`plugin.json` 由 `asset3d --manifest` 生成（无 BOM）。
- 清理仓库根目录的一次性探针与日志（`temp_manifest.json`、`dev/probe.py` 及各构建/调试日志）。
- `plugin.json` 仍为 `pluginApi 1.0`，`AIRB` 仍为 `v1`：本版只改行为，不改格式。

### 已知限制

- 单体资产超过约 720 KiB 仍读不进来：宿主 `host/fs.read` 没有 offset/分片参数，插件也不能绕过能力闸门直读磁盘。规避方式是 `.gltf` + 外部 `.bin`；根治需要宿主提供分段读取。
- 预览不播动画、不做蒙皮变形；`maxTriangles` 只影响预览，导出始终完整。

## [0.1.0]

首个可用版本：`inspect_asset` / `export_asset` / `preview_asset` / `list_assets` / `export_engine_loader` 五个工具，FBX（ufbx）与 glTF/GLB 解析，AIR JSON / AIRB v1 二进制导出，离线 HTML + 数据分片预览，节点 TRS 不烘焙（顶点保持 mesh 局部空间）。
*（内容由AI生成，仅供参考）*
