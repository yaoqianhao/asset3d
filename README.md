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
   | `plugin.json` | 清单。必须由可执行文件自己生成，见下 |
   | — | 无其他运行时依赖；贴图/JSON 都由宿主原语读写 |

   复制并生成清单：

   ```powershell
   Copy-Item target\release\asset3d.exe dist\asset3d.exe -Force
   # 清单与工具目录同源（tools::catalog），不要手工维护第二份
   .\dist\asset3d.exe --manifest | Set-Content dist\plugin.json -Encoding utf8NoBOM
   ```

   `plugin.json` 必须是 **无 BOM** 的 UTF-8：带 BOM 时宿主解析清单会失败（`dev/harness.py` 会直接报出来）。

3. 安装到宿主：把整个 `dist/` 目录放进宿主的插件目录，由宿主按 `main` 字段拉起。清单里的 `id`（`asset3d`）决定工具命名空间，改了它等于换插件。

版本号只有一处真源：`Cargo.toml` 的 `version`。清单与握手回包都取 `env!("CARGO_PKG_VERSION")`，因此三者天然一致；当前为 `0.2.0`。

## 工具

插件对外只有 5 个工具，参数与 `plugin.json` 完全一致。

| 工具 | 风险 | 作用 |
| --- | --- | --- |
| `inspect_asset` | read | 解析资产并输出 AIR 摘要：网格/材质/贴图/骨骼/动画/包围盒 + 导入期问题清单。只读，不写盘 |
| `export_asset` | write | 导出 `.air.bin`（AIRB v1 二进制，几何平铺可直灌顶点缓冲）与/或 `.air.json`（便于 diff 与手改），写盘后立即回读校验 |
| `preview_asset` | write | 生成离线预览：薄 HTML 骨架 + 若干 `.shard.NN.js` 数据分片，浏览器/Electron 直接打开，无需服务器 |
| `list_assets` | read | 递归扫描工作区目录，列出所有 `.fbx` / `.glb` / `.gltf` 及体积，便于批量导入前摸家底 |
| `export_engine_loader` | write | 把引擎侧读取 AIR/AIRB 的 Rust 加载器源码（纯 std + serde）写到指定路径，附格式说明 |

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
- **不做纹理转码**：贴图按原字节（base64 或原样）搬运，不解 PNG/JPEG、不生成 mipmap、不改色彩空间。
- **不烘焙**：节点 TRS 原样进 AIR，顶点保持 mesh 局部空间（蒙皮/骨骼动画都靠节点链与关节矩阵在引擎侧还原）。预览不做蒙皮变形。
- **不做网格优化**：不去重顶点、不重排索引、不生成 LOD；只有预览在超预算时才截断三角形（见下）。
- **不写引擎资产格式**：输出只有 AIR（`.air.bin` / `.air.json`），不生成 glTF/FBX，也不写引擎自定义二进制。

## 宿主约束：单帧 1 MiB 与预览分片

宿主与插件之间的每条 NDJSON 帧上限 **1 MiB**（`FRAME_LIMIT_BYTES`）。超限不是"截断"，而是协议违规、插件被踢下线，所以预算必须提前算，不能写完再发现。

插件侧的算法（`src/rpc.rs`）：

- 预留 64 KiB 给 JSON 外壳、路径、字段名（`FRAME_RESERVE_BYTES`），剩约 960 KiB 才是载荷预算。
- 预算按**编码后**长度算：`base64` 是 4/3 膨胀，用原始字节数去比预算会高估写入量。
- 回包只放摘要文本；一切大产物走 `host/fs.write`。
- 因此 `host/fs.read` 能带回的原始字节上限约 **720 KiB**（1 MiB / 4 × 3）——比 720 KiB 大的单体资产**读不进来**，会明确报错并建议改用 `.gltf` + 外部 `.bin` 拆分（这是本次修复后仍然存在的边界，见「已知限制」）。
- 握手期取 `host/workspace.info` 用 **2 秒**短超时（`HOST_CALL_TIMEOUT_SHORT`）：失败就降级为"无工作区"继续握手，绝不让一个不响应的宿主把启动卡满 20 秒；其余宿主调用仍用 20 秒。

预览分片机制（`src/view.rs` + `tools::preview`）：

- `preview_asset` 把几何/材质/贴图序列化成一个 JSON 载荷，再切成 `<base>.shard.NN.js`，每片严格低于单帧预算（再留 1 KiB 余量 `SHARD_MARGIN`，按最长分片名算路径开销）。
- HTML 骨架本身也走同一条闸门；超了直接报错并建议缩短 `out` 路径（骨架无法分片，因为它是入口）。
- 写入顺序是「先分片、后 HTML」：中途失败时用户手上不会出现一个指向空目录的页面。
- 分片必须与 HTML **放在同一目录**，缺片页面会在界面上提示。
- 三角形预算：单个网格超 `maxTriangles` 时**截断索引并压掉用不到的顶点**（不是整网格丢光），多个网格时先丢小网格、保住大件，所有降级都写在页面 HUD 与工具回包里。
- 贴图超限（单片 6 MiB / 合计 24 MiB）时**不内嵌**，但把数据原样放回 AIR，并在回包里点名，导出仍然完整。
- 预览只画静态网格与材质：**不载入动画轨道**（无消费方，白占分片体积），也不做蒙皮变形；要动画数据请用 `export_asset`。

## 已知限制

1. **单体资产 > 约 720 KiB 读不进来**：宿主 `host/fs.read` 没有 offset/分片参数，插件也无法绕过能力闸门直读磁盘。当前只能靠 `.gltf` + 外部 `.bin` 规避；真正的修复需要宿主提供分段读取。
2. **预览不播动画、不做蒙皮**：预览是"看图找问题"的工具，不是播放器。骨骼/权重只在导出里完整保留。
3. **预览骨架不能分片**：极长的 `out` 路径（含长目录名）会把骨架预算吃光，工具会直接报错而不是生成半成品。
4. **`maxTriangles` 只影响预览**：导出始终是完整几何，不会因为预览截断而丢数据。
5. **FBX 兼容性取决于 ufbx**：加密/压缩过的老版 FBX、以及 ufbx 未覆盖的扩展属性会降级或跳过，导入期以 issue 形式记录，不静默丢弃。
6. **贴图只搬字节**：不转码、不缩放，超大贴图在预览里会被排除（导出不受影响）。
7. **单进程单线程处理工具调用**：工具在独立线程执行（否则解析期间连宿主回包都收不到），但同一时刻只跑一个工具；并发调用会排队。

## 开发与验证

```powershell
cargo test --release        # 单测：帧预算/accessor 上限/sparse 降级/三角化/蒙皮/分片/三角预算
cargo clippy --release      # 0 error；现存 warning 是未使用的辅助函数与风格建议
python dev\harness.py       # 宿主侧联调：真 1 MiB 闸门 + 真 exe
python dev\verify_preview.py
```

`dev/harness.py` 会现场生成夹具（`.tmp-ws/`）：`cube.gltf`、`cube.glb`（同源对照）、`quad.fbx`（四边形三角化 + UV 翻转）、`skin.glb`（2 关节 + IBM + 动画 + 节点 TRS）、`big.glb`（26450 三角形，专压 1 MiB 分片路径）。它自己实现 6 个 `host/*` 方法并带真实帧上限，任何超限帧都会被记为失败——不必开 IDE 就能撞出帧预算问题。

## 版本

- 格式与协议版本：`pluginApi 1.0`、AIRB `v1`
- 插件版本：见 `CHANGELOG.md`，当前 `0.2.0`
*（内容由AI生成，仅供参考）*
