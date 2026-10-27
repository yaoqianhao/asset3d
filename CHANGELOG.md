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
