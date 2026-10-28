---
AIGC:
    Label: "1"
    ContentProducer: 001191440300708461136T1XGW3
    ProduceID: 04526fdd866a8c5c9aceee6b83ed2c05_a294c8add1bb11f1b84e52540064ee0f
    ReservedCode1: AHt1q5SFG1KpCOqm58DvetdKlpPvhT40uGdQzV+oC8tluUDMCy4e7B9bd5J+5E4EPLL4qNhVYRq1iqE1TxAoQ1Ch6iTQqsOgxcQPEppscYvejERyzMqcZEdyFN1ZTMRGuyb70bMOczTcvqU9BD6jWrKnra8+4KRb44Gs5Cu9YaKfRbFXSbP8+E2gmCg=
    ContentPropagator: 001191440300708461136T1XGW3
    PropagateID: 04526fdd866a8c5c9aceee6b83ed2c05_a294c8add1bb11f1b84e52540064ee0f
    ReservedCode2: AHt1q5SFG1KpCOqm58DvetdKlpPvhT40uGdQzV+oC8tluUDMCy4e7B9bd5J+5E4EPLL4qNhVYRq1iqE1TxAoQ1Ch6iTQqsOgxcQPEppscYvejERyzMqcZEdyFN1ZTMRGuyb70bMOczTcvqU9BD6jWrKnra8+4KRb44Gs5Cu9YaKfRbFXSbP8+E2gmCg=
---

# asset3d

ai-ide 的 3D 资产导入插件：把 `.fbx` / `.glb` / `.gltf` 解析成引擎内核可直接消费的 **AIR** 数据，并给出离线预览。

插件本体是一个独立可执行文件（Windows 下为 `asset3d.exe`），通过 NDJSON 帧协议与宿主通信；不参与宿主内核的 Cargo workspace。

自 0.4.0 起还多一个**视口进程** `asset3d-gui.exe`：宿主需要把资产画给人看时把它拉起来，两者之间走命名管道，画面上不下发几何数据（见「视口」一节）。

## 安装

1. 构建：

   ```powershell
   cargo build --release
   ```

   > 本机工具链（MinGW，供 `ufbx` 的 C 代码编译）按需先配置好；`rust-version = "1.79"`。

2. 打包目录（`dist/`）需要三件东西：

   | 文件 | 说明 |
   | --- | --- |
   | `asset3d.exe` | `target/release/asset3d.exe` 的原样副本（**不要**再 strip 或改名） |
   | `asset3d-gui.exe` | `target/release/asset3d-gui.exe` 的原样副本。`plugin.json` 的 `views[].external` 按文件名找它，改名等于视口失效 |
   | `plugin.json` | 清单。必须由可执行文件自己生成，见下 |
   | — | 无其他运行时依赖；贴图/JSON 都由宿主原语读写 |

   复制并生成清单：

   ```powershell
   Copy-Item target\release\asset3d.exe dist\asset3d.exe -Force
   Copy-Item target\release\asset3d-gui.exe dist\asset3d-gui.exe -Force
   # 清单与工具目录同源（tools::catalog），不要手工维护第二份
   .\dist\asset3d.exe --manifest | Set-Content dist\plugin.json -Encoding utf8NoBOM
   ```

   `plugin.json` 必须是 **无 BOM** 的 UTF-8：带 BOM 时宿主解析清单会失败（`dev/harness.py` 会直接报出来）。

3. 安装到宿主：把整个 `dist/` 目录放进宿主的插件目录，由宿主按 `main` 字段拉起。清单里的 `id`（`asset3d`）决定工具命名空间，改了它等于换插件。

版本号只有一处真源：`Cargo.toml` 的 `version`。清单与握手回包都取 `env!("CARGO_PKG_VERSION")`，因此三者天然一致；当前为 `0.4.0`。

## 工具

插件对外 6 个工具，参数与 `plugin.json` 完全一致。

| 工具 | 风险 | 作用 |
| --- | --- | --- |
| `inspect_asset` | read | 解析资产并输出 AIR 摘要：网格/材质/贴图/骨骼/动画/包围盒 + 导入期问题清单。只读，不写盘 |
| `export_asset` | write | 导出 `.air.bin`（AIRB v1 二进制，几何平铺可直灌顶点缓冲）与/或 `.air.json`（便于 diff 与手改），写盘后立即回读校验 |
| `preview_asset` | write | 生成离线预览：薄 HTML 骨架 + 若干 `.shard.NN.js` 数据分片，浏览器/Electron 直接打开，无需服务器 |
| `list_assets` | read | 递归扫描工作区目录，列出所有 `.fbx` / `.glb` / `.gltf` 及体积，便于批量导入前摸家底 |
| `view_render` | read | 插件面板视图入口（`aiide.view/1`）：把资产折成一棵声明式描述树（概览/材质/贴图/动画/骨骼/导入问题/动作按钮）交给宿主面板渲染；省略 `path` 时列出工作区可打开的资产。只读，不解析动画轨道 |
| `export_engine_loader` | write | 把引擎侧读取 AIR/AIRB 的 Rust 加载器源码（纯 std + serde）写到指定路径，附格式说明 |

插件还对外一个**命令**（`plugin.json` 的 `commands`，供宿主菜单/快捷键直接调）：

| 命令 | 风险 | 作用 |
| --- | --- | --- |
| `open_viewport` | read | 把资产路径发给已启动的视口窗口；视口没起来时先由宿主拉起进程。参数 `path` 必填，`pipe` 省略时发默认管道（手动双击那个窗口） |

常用参数：

- `inspect_asset` / `export_asset` / `preview_asset` 的 `path` 支持工作区相对路径与工作区内绝对路径。
- `export_asset.format`：`bin` / `json` / `both`（默认 `both`）。
- `export_asset.animations`、`inspect_asset.animations`：是否解析/导出动画，默认 `true`。
- `export_asset.embedTextures`：是否把贴图以 base64 内嵌进 AIR，默认 `false`。
- `preview_asset.maxTriangles`：预览三角形预算，默认 `400000`；`0` 表示不限制。
- `list_assets.limit` 默认 `200`，`recursive` 默认 `true`。

## 能力边界

- **只认 `.fbx` / `.glb` / `.gltf`**。`.obj` / `.dae` / `.usd` / `.blend` 一概不处理，也不做格式互转。
- **只读不查盘**：所有文件访问都走宿主 `host/fs.read` / `host/fs.write` / `host/fs.stat` / `host/fs.list` 原语，插件自己不开文件句柄。因此路径必须落在宿主工作区内——越界路径由宿主闸门拒绝；插件侧不做路径预判，也不绕闸门读盘。
- **不做纹理转码**：贴图按原字节（base64 或原样）搬运，不解 PNG/JPEG、不生成 mipmap、不改色彩空间。**视口进程是例外**——它为了把花纹画出来会解码 PNG/JPEG（见「视口」一节），但插件侧的输出仍不动贴图字节。
- **不烘焙**：节点 TRS 原样进 AIR，顶点保持 mesh 局部空间（蒙皮/骨骼动画都靠节点链与关节矩阵在引擎侧还原）。预览不做蒙皮变形。
- **不做网格优化**：不去重顶点、不重排索引、不生成 LOD；只有预览在超预算时才截断三角形（见下）。
- **不写引擎资产格式**：输出只有 AIR（`.air.bin` / `.air.json`），不生成 glTF/FBX，也不写引擎自定义二进制。

## 视口：asset3d-gui.exe

宿主面板里只有一棵声明式描述树，看不到形状。形状由**另一个进程**画：`asset3d-gui.exe`（winit + wgpu），与插件分进程是硬要求——winit 的事件循环要独占线程，塞进宿主的 stdio 帧泵里两边只会互相等死。

### 三种启动形态（argv 区分）

| argv | 形态 | 行为 |
| --- | --- | --- |
| `--pipe <name>` | IDE 托管 | 监听宿主给的命名管道，`open` 指令决定画面；宿主断开后进程继续存活 |
| `--standalone <asset>` | 独立看图 | 不建管道，启动即打开该资产；同一资产只开一个窗口 |
| 无参数 | 手动双击 | 抢 `Local\asset3d.viewport` 互斥体；抢不到就把已有窗口置前后退出（只留一个视口）；抢到就弹文件对话框 |

退出码：参数错误 `2`、路径错误 `3`，正常 `0`。

### 交互

左键拖动转视角、滚轮推拉、`O` 打开文件、把文件拖进窗口直接看、`Esc` 清空。相机是轨道相机，打开资产时按包围球自动取景（`Orbit::fit`），所以不同尺度的资产都不用自己先调距离。

### 协议 `asset3d.session/1`

行分隔 JSON，`{"v":1,"op":"...","seq":N,...}`，走命名管道：

| op | 作用 | 回包 |
| --- | --- | --- |
| `open` | 打开资产（`path` 为工作区相对路径，由插件宿主校验后转发） | `ok` + 网格/三角形/贴图数与 `notes` |
| `close` | 清空画面，进程不退 | `ok` |
| `focus` | 置前抢焦点 | `ok` |
| `ping` | 心跳 | `pong` |
| `screenshot` | 截图（本期占位） | `error{code:"unsupported"}` |

三条硬规则：未知 `op` / 未知字段一律忽略（协议要能往前加）；`seq=0` 是单向通知不回包，`seq>0` 按同 `seq` 回；握手 `ready.capabilities` 本期只报 `["static"]`，动画与蒙皮留给 M-UI-4。占位 op 明确回 `unsupported` 而不是静默超时——让对端知道"不是没收到，是这版不做"。

### 画面里有什么

- **网格 + 材质贴图**：顶点带位置/法线/UV，同一 mesh 按 AIR 的 `material_parts` 切成若干索引区间，每段一份 baseColor 与（可选）解码后的贴图，渲染侧一段一个 draw call、材质相同的分区共用 bind group。缺贴图就退回 baseColor 纯色，贴图坏了只记一条 `notes`，不至于看不到模型。
- **轨道相机 + 包围球**：近远裁剪面按 `radius` 推算，默认视角略高于桌面。
- **参考地板与坐标轴**：1 米一格的地面网格 + X 轴红 / Z 轴蓝，给尺度感；空窗口和清空之后都靠它证明"程序是活的"。
- **贴图读取优先级**：内嵌 base64（GLB / FBX 内嵌）→ 资产同目录的相对 `uri`。外部 `uri` 只允许落在资产同目录，越界一律当读不到。
- **UV 翻转**：FBX 的 UV 原点在左下、glTF 在左上，按 AIR 的 `units.uv_origin` 决定是否翻 v（读字段而不是猜格式名）。

### 视口侧的取舍

- **一次上传、不打 mipmap**：贴图按 `Rgba8UnormSrgb` 上传，采样 `Repeat` + `Linear`。不打 mipmap 的代价是远处有摩尔纹，收益是省掉一整条生成链——M-UI-3 的判据是"能看懂一个资产"。
- **超过 4096×4096 的贴图不解码**：只记一条 `notes` 说清"太大没展开"。为了看一眼资产把 8K 贴图展成 256 MB 像素不划算。
- **不排序半透明**：baseColor 的 alpha 一律当不透明画。透明资产会显得实心，但不会出现黑洞或前后乱序。
- **不播动画、不做蒙皮**：与预览一致，这一层只画静态网格。

## 宿主约束：单帧 1 MiB 与预览分片

宿主与插件之间的每条 NDJSON 帧上限 **1 MiB**（`FRAME_LIMIT_BYTES`）。超限不是"截断"，而是协议违规、插件被踢下线，所以预算必须提前算，不能写完再发现。

插件侧的算法（`src/rpc.rs`）：

- 预留 64 KiB 给 JSON 外壳、路径、字段名（`FRAME_RESERVE_BYTES`），剩约 960 KiB 才是载荷预算。
- 预算按**编码后**长度算：`base64` 是 4/3 膨胀，用原始字节数去比预算会高估写入量。
- 回包只放摘要文本；一切大产物走 `host/fs.write`。
- **单体资产不再受单帧限制**：读走 `host/fs.read` 的 `offset` 分片（每片 ≤ 宿主单次窗口 512 KiB，再按帧预算扣掉 JSON 与 base64 的 4/3 膨胀），循环推进直到 `eof`；写走 `host/fs.write` 的分片模式（首片整替换建文件，后续片 `append` 或 `offset` 定位续写）。
- 握手期取 `host/workspace.info` 用 **2 秒**短超时（`HOST_CALL_TIMEOUT_SHORT`）：失败就降级为"无工作区"继续握手，绝不让一个不响应的宿主把启动卡满 20 秒；其余宿主调用仍用 20 秒。

预览分片机制（`src/view.rs` + `tools::preview`）：

- `preview_asset` 把几何/材质/贴图序列化成一个 JSON 载荷，再切成 `<base>.shard.NN.js`，每片严格低于单帧预算（再留 1 KiB 余量 `SHARD_MARGIN`，按最长分片名算路径开销）。
- HTML 骨架本身也走同一条闸门；超了直接报错并建议缩短 `out` 路径（骨架无法分片，因为它是入口）。
- 写入顺序是「先分片、后 HTML」：中途失败时用户手上不会出现一个指向空目录的页面。
- 分片必须与 HTML **放在同一目录**，缺片页面会在界面上提示。
- 三角形预算：单个网格超 `maxTriangles` 时**截断索引并压掉用不到的顶点**（不是整网格丢光），多个网格时先丢小网格、保住大件，所有降级都写在页面 HUD 与工具回包里。
- 贴图超限（单片 6 MiB / 合计 24 MiB）时**不内嵌**，但把数据原样放回 AIR，并在回包里点名，导出仍然完整。
- 预览只画静态网格与材质：**不载入动画轨道**（无消费方，白占分片体积），也不做蒙皮变形；要动画数据请用 `export_asset`。

## 分片读写：为什么要循环，以及上限怎么算

宿主帧闸门没变（1 MiB / 帧），变的是**每次 `host/*` 调用只搬一小片**，大资产靠多次往返拼起来（`src/rpc.rs`）：

- **读**：`READ_WINDOW_BYTES = 512 KiB`，直接取宿主 `MAX_READ_WINDOW_BYTES` 之值——512 KiB base64 后约 683 KB，加路径与字段名仍在一帧的 960 KiB 载荷预算内，所以这一侧真正的瓶颈是宿主窗口而不是帧预算。响应里的 `bytes` / `eof` / `size` 是唯一真相：按回包的 `bytes` 推进 `offset`，`eof` 为真才收工，绝不假设"窗口要多少就回多少"。`size` 与已读字节不一致（文件在读取期间被改）时直接报错，不拼一个错位资产。
- **写**：片长 `write_chunk_bytes` 按路径现算——先把帧预算反推成原始字节（`payload_budget / 4 × 3`），再与 512 KiB 取小，读写两侧用同一片长，排障时"第几片"能直接对上。载荷装得进一帧就照旧整文件写（老宿主也认），装不进才切分片：第 0 片以整替换语义建文件，后续片按 `append`（`offset` 定位）续写。任一分片失败即尽力清理已写的半截文件，回包里给出可读原因而不是留一个大小正确、内容截断的产物。
- **配额实测**：3 MiB 资产读 6 片、预览写出 6 片，单次工具调用最多用掉 26 / 200 次宿主调用，离配额还有余量。
- **上限 `MAX_ASSET_BYTES` = 96 MiB**：按真实能力推算——512 KiB 窗口 × 200 次调用 ≈ 100 MiB，取 96 MiB 留安全边际。超限由**插件自己**报，附上具体大小、上限、以及"这个上限是怎么算出来的"（读满需要 192 次 `host/fs.read`），并建议拆 `.gltf` + 外部 `.bin`；不把问题推给宿主闸门去判违规。此外宿主单次调用还有 120 秒硬超时，96 MiB 属于"能读但很慢"的量级，超大资产仍建议拆分以便反复迭代。

## 旧宿主降级

分片能力是宿主新增的。对着旧宿主跑时，`host/fs.read` / `host/fs.write` 会因为不认 `offset` / `append` 参数而报参数错误，插件据此判定宿主版本过旧并走原有整文件路径：

- 资产本身装得进一帧（约 720 KiB 以内）→ 照常工作，行为与 0.2.0 一致；
- 装不进一帧 → 明确报"宿主版本过旧"，并写清两个数字：资产的真实大小、当前宿主单帧能带回的上限，以及升级宿主即可解决。不会静默截断，也不会拿半截数据去解析。

## 已知限制

1. **超过 96 MiB 的单个资产仍读不进来**：分片读写把旧的天花板（约 720 KiB）抬到了 96 MiB，但不是无限——上限由"宿主单次窗口 × 单次调用配额"决定，超大资产建议拆成 `.gltf` + 外部 `.bin`。另外宿主单次调用有 120 秒硬超时，接近上限的资产即便能读完也会很慢。
2. **旧宿主上仍有约 720 KiB 天花板**：宿主不认 `offset` / `append` 时插件退回整文件路径，此时超过单帧预算的资产会被拦下并提示"宿主版本过旧"。升级宿主即可解除。
3. **预览不播动画、不做蒙皮**：预览是"看图找问题"的工具，不是播放器。骨骼/权重只在导出里完整保留。
4. **预览骨架不能分片**：极长的 `out` 路径（含长目录名）会把骨架预算吃光，工具会直接报错而不是生成半成品。
5. **`maxTriangles` 只影响预览**：导出始终是完整几何，不会因为预览截断而丢数据。
6. **FBX 兼容性取决于 ufbx**：加密/压缩过的老版 FBX、以及 ufbx 未覆盖的扩展属性会降级或跳过，导入期以 issue 形式记录，不静默丢弃。
7. **贴图只搬字节**：插件不转码、不缩放，超大贴图在预览里会被排除（导出不受影响）；视口会解码，但超过 4096×4096 的贴图不解码，画面上退回纯色并在 `notes` 里给原因。
8. **视口不排序半透明、不打 mipmap**：alpha 一律当不透明，远处贴图会有摩尔纹。静态渲染的取舍，不是 bug。
9. **视口截图是协议占位**：`screenshot` 回 `unsupported`，要图请用系统截图。
10. **单进程单线程处理工具调用**：工具在独立线程执行（否则解析期间连宿主回包都收不到），但同一时刻只跑一个工具；并发调用会排队。

## 开发与验证

```powershell
cargo test --release        # 单测：帧预算/accessor 上限/sparse 降级/三角化/蒙皮/分片/三角预算
cargo clippy --release      # 0 error；现存 warning 是未使用的辅助函数与风格建议
python dev\harness.py       # 宿主侧联调：真 1 MiB 闸门 + 分片读写 + 旧宿主降级 + 真 exe
python dev\verify_preview.py
```

`dev/harness.py` 会现场生成夹具（`.tmp-ws/`）：`cube.gltf`、`cube.glb`（同源对照）、`quad.fbx`（四边形三角化 + UV 翻转）、`skin.glb`（2 关节 + IBM + 动画 + 节点 TRS）、`big.glb`（26450 三角形，专压 1 MiB 分片路径）、`huge_1m.glb` / `huge_3m.glb`（1.04 MiB / 2.87 MiB，专压分片读写与 96 MiB 上限之外的诊断）。它自己实现 6 个 `host/*` 方法并带真实帧上限，任何超限帧都会被记为失败——不必开 IDE 就能撞出帧预算问题。

## 版本

- 格式与协议版本：`pluginApi 1.1`、AIRB `v1`、视口会话 `asset3d.session/1`
- 插件版本：见 `CHANGELOG.md`，当前 `0.4.0`
*（内容由AI生成，仅供参考）*
