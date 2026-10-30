---
AIGC:
    Label: "1"
    ContentProducer: 001191440300708461136T1XGW3
    ProduceID: 04526fdd866a8c5c9aceee6b83ed2c05_8ed9a79ad44e11f19fe5525400638852
    ReservedCode1: q4W5DhHkNtW51bgmv+WK9sEIqDFEUi9mLGAJdEbn6bZPBcU0PqlmDSvVzBLlMaRmKt2h66nv5D7rmrvUkCX8tnQ7hA82R32MVYKye1iSEw0LnwuI0/gm0TVUN62XFgCjyOmF7Mw5gzREiS8LeRqlV56EzWFTWWPBxozawNUcT2y0mnViK3KiOsrcCsY=
    ContentPropagator: 001191440300708461136T1XGW3
    PropagateID: 04526fdd866a8c5c9aceee6b83ed2c05_8ed9a79ad44e11f19fe5525400638852
    ReservedCode2: q4W5DhHkNtW51bgmv+WK9sEIqDFEUi9mLGAJdEbn6bZPBcU0PqlmDSvVzBLlMaRmKt2h66nv5D7rmrvUkCX8tnQ7hA82R32MVYKye1iSEw0LnwuI0/gm0TVUN62XFgCjyOmF7Mw5gzREiS8LeRqlV56EzWFTWWPBxozawNUcT2y0mnViK3KiOsrcCsY=
---

# asset3d-retarget：UE 动作 → Tripo 人形角色重定向

把 UE5 Mannequin 的动作序列（`D:\share\Bow_Attack_Seq.glb` 这类）搬到 Tripo 生成并绑定好的人形资产
（`D:\share\character\rebelsoldier.glb` 这类）身上，输出一个**能直接播放**的新 GLB。

```
asset3d-retarget.exe  --source <动作.glb|fbx>  --target <角色.glb>  --out <输出.glb>
```

一次调用做四件事：读源动作 → 建骨骼映射 → 逐帧求解姿态 → 写新 GLB 并**读回自检**。
目标资产已有的 mesh / material / texture / skin 一个字节都不动，只在 glTF JSON 尾部追加 `animations` 与配套 accessor。

- 可执行文件：`target\release\asset3d-retarget.exe`（构建：`cargo build --release --bin asset3d-retarget`）
- 代码：`src/bin/asset3d-retarget.rs`（命令行）+ `src/bin/retarget/`（glb / source / mapping / math / plan / writer / verify / report）

---

## 1. 快速开始

```powershell
cd D:\asset3d
cargo build --release --bin asset3d-retarget

# 射箭攻击（81 帧 / 1.33s）
.\target\release\asset3d-retarget.exe `
    --source D:\share\Bow_Attack_Seq.glb `
    --target D:\share\character\rebelsoldier.glb `
    --out    D:\share\character\rebelsoldier_bow_attack.glb `
    --report D:\asset3d\temp\retarget\report_attack.txt

# 持弓防御待机（602 帧 / 10.02s，源为 FBX）
.\target\release\asset3d-retarget.exe `
    --source D:\share\Bow_Defense_Start_Loop_Seq.FBX `
    --target D:\share\character\rebelsoldier.glb `
    --out    D:\share\character\rebelsoldier_bow_defense.glb `
    --report D:\asset3d\temp\retarget\report_defense.txt
```

输出路径必须是**新文件**，不允许等于 `--source` 或 `--target`（工具会拒绝）。

## 2. 参数

| 参数 | 必填 | 说明 |
| --- | --- | --- |
| `--source <path>` | 是 | 源动作文件，支持 `.glb` / `.fbx`。读第 0 条动作，或用 `--anim` 指定 |
| `--target <path>` | 是 | 目标角色 `.glb`，需带 mesh + skin（可以完全没有动画） |
| `--out <path>` | 是 | 输出 `.glb` 路径，新文件 |
| `--map <json>` | 否 | 追加/覆盖骨骼映射，两种写法都认：`{"spine_04":"Spine02"}` 或 `[{"source":"spine_04","target":"Spine02"}]`。同名条目**顶掉**内置表，新名字**追加** |
| `--anim <name>` | 否 | 源文件含多条动作时按名字挑一条（默认第 0 条） |
| `--scale-motion` / `--no-scale-motion` | 否 | 根位移是否按「目标腿长 / 源腿长」缩放，**默认开** |
| `--no-twist` | 否 | `*Twist01/02` 一组辅助骨整组不映射（怀疑扭转骨带来抖动时用） |
| `--picks a,b,c` | 否 | 自检时逐帧数值比对的代表骨（默认自动挑上臂 / 大腿 / 脊柱各一根） |
| `--report <path>` | 否 | 完整报告另存为文本 |
| `--quiet` | 否 | 只打印结论行 |
| `-h` / `--help` | 否 | 帮助 |

退出码：`0` 成功且自检通过 / `1` 用法或读写错误 / `2` 文件已写出但自检不通过。

## 3. 算法

### 3.1 局部空间增量法的世界空间等价形式 + rest 朝向对齐

```
src 世界增量   delta[i] = src_world_anim[i] * src_world_rest[i]⁻¹
rest 对齐      align[i] = shortest_arc(dst_rest_dir[i] → src_rest_dir[src(i)])
dst 世界姿态   W[i]     = delta[i] * align[i] * dst_world_rest[i]      (已映射骨)
               W[i]     = W[parent] * dst_rest_local_rot[i]            (未映射骨)
dst 局部姿态   local[i] = W[parent]⁻¹ * W[i]
```

两个关键设计：

- **必须走世界空间**：UE 每根骨的 `+X` 指向子骨，Tripo 是 `+Y` 指向子骨，两侧局部轴约定不同。
  直接用 `src_local_rest⁻¹ * src_local_anim` 喂目标骨的局部通道会整体歪掉；换成世界增量后，轴约定差异被 rest 对齐吸收，
  只剩「目标骨该朝哪」这一个信息。
- **`align` 不能省**：去掉它只剩纯世界增量，只保证「相对各自 rest 转了多少」一致。源是 UE 的 T-pose、Tripo 角色多是手臂下垂的 A-pose，
  源抬手 90° 落到目标身上会被下垂初始角吃掉，画面上就是「手臂抬不起来」。`align` 先把目标 rest 骨朝向旋到源 rest 骨朝向，
  增量落在同一基准上。用**最短弧**而非任意对齐矩阵，是为了不动绕骨轴的 twist 自由度，不给四肢凭空加扭转。

### 3.2 未映射骨

保持目标自身 rest 局部旋转、刚性跟随父骨。缺骨骼（手指、无对应段）时这才是正确行为：不会把子树拧成麻花，也不会凭空继承祖先增量多转一份。

### 3.3 平移与缩放

- 只重定向**根链**（源 `root` / `pelvis` 对应的 `Root` / `Hip`），位移量按腿长比缩放；
- 其余骨骼的局部平移保持目标 rest —— 局部平移就是骨长与关节连接点，照抄源骨架等于把 UE 的骨架比例硬塞进 Tripo 角色，必然拉断；
- 缩放通道完全不重定向。

腿长比 = 目标腿长 / 源腿长，取两侧 `thigh → calf → foot` 的 rest 世界距离。
`rebelsoldier` 实测 **0.6098**（源 0.9191 m / 目标 0.5605 m），即目标腿只有源的 61%，不缩放会出现「一步跨出半个身位」的滑步。

### 3.4 祖先缩放链（FBX 厘米单位）

`pose_frame` 求世界平移时必须把祖先的 rest/anim 缩放逐级累乘进去。UE 导出的 FBX 常在 `Armature` 上挂 `0.01` 世界缩放、
各骨局部平移以厘米计：漏掉这一步，`pelvis` 的 94 cm 会被当成 94 m，产物 `Hip` 平移直接差出 3.195 m。
单元测试 `root_motion_follows_ancestor_scale`（100 cm × 0.01 = 1 m）锁住这个行为。

## 4. 映射规则

内置表在 `src/bin/retarget/mapping.rs::BUILTIN`，分两层：**内置默认 + `--map` 覆盖**。命名差异是这张表存在的原因——
UE 用小写下划线（`upperarm_l`），Tripo 用大驼峰带段号（`L_Upperarm`）。

几个容易踩的语义点：

| 源（UE） | 目标（Tripo） | 说明 |
| --- | --- | --- |
| `pelvis` | `Hip` | UE 的 pelvis 是髋部根，两腿与脊柱都从它分叉；Tripo 的 `Hip` 同样分出 `Pelvis`→腿 与 `Waist`→脊柱。Tripo 的 `Pelvis` 只是腿部中转节点，源侧无对应骨，保持 rest |
| `spine_01/02/03` | `Waist` / `Spine01` / `Spine02` | 三节脊柱正好接到 `Spine02`（其下即脖子与锁骨） |
| `clavicle_*` / `lowerarm_*` / `ball_*` | `Clavicle` / `Forearm` / `ToeBase` | 命名差异 |
| `*_twist_01_*` | `*Twist01` | UE 每段只有 twist_01；目标的 `*Twist02` 永远缺席 → 保持 rest、跟随父骨刚性转动 |
| 手指 `index/middle/ring/pinky/thumb_01..03_*` | 无 | Tripo 角色通常没有手指骨，跳过并保持 rest |
| `ik_foot_*` / `ik_hand_*` / `ik_foot_root` / `ik_hand_root` / `ik_hand_gun` | 无 | IK 控制骨，UE 导出时附带，本身不参与蒙皮；跳过（真要接可用 `--map "ik_foot_l":"L_Foot"`） |

**覆盖率口径**：分母是「源侧真正带动画的骨」（有 rotation/translation 曲线的骨），分子是最终建立的映射对数。
`--map` 覆盖进来的对同样计入分子。内置表没覆盖但 `--map` 也没覆盖的骨，报告里逐条给出处理方式和候选建议。

## 5. 输出与自检

### 5.1 写出

`writer.rs` 三条硬约束：

1. **只追加不重排**：`meshes` / `materials` / `textures` / `images` / `skins` / `accessors[0..N]` 按原字节保留，75 MB 贴图不会因写动画被重新打包；
2. **采样时刻与源一致**：`input` accessor 直接写源时间轴（源 81 帧就是 81 帧），不重采样不降帧，`interpolation` 沿用源；
3. **只写需要的轨道**：每根有增量的骨 1 条 rotation，根链额外 1 条 translation，scale 一律不写。

### 5.2 读回自检（`verify.rs`）

写完后**重新解析产物 GLB**，而不是拿内存里的期望值自说自话：

- 资产完整性：关节数 / mesh / 材质 / 纹理数量前后比对，动画 0 → 1；
- 姿态正确性：挑 3 根代表骨（默认上臂 / 大腿 / 脊柱）× 3 个时刻（首帧 / 中帧 / 末帧）比对四元数，**容差 0.5°**；
- 位移正确性：根链末帧平移与期望比对，**容差 1 mm**。

任一超差 → 打印差异并以退出码 `2` 结束（文件仍在，便于人工查）。

### 5.3 视觉核对（`asset3d-gui.exe`）

数值自检证明「写进去的就是算出来的」，不证明「画出来好看」。逐帧画面证据用 GUI 的截帧模式取：

```powershell
$env:ASSET3D_POSE_SHOT="1"
$env:ASSET3D_POSE_SHOT_DIR="D:\asset3d\temp\pose\attack"
$env:ASSET3D_POSE_SHOT_AT="f00:0,f25:0.25,f50:0.5,f75:0.75,f99:0.999"
.\target\release\asset3d-gui.exe --standalone D:\share\character\rebelsoldier_bow_attack.glb
```

`AT` 是「标签:时长比例」逗号列表，按顺序停靠播放头，每处等三帧后抓客户区，最后写 `pose-shots.json`。
**第一张抓帧固定是热身轮，会被丢弃重抓**——载入后的第一次 present 还没进合成器，抓早了只得到一张空背景；
所以清单里不会出现那一张空图，但日志能看到停靠点被重走一遍。

## 6. 实测结果（rebelsoldier，2026-10-30）

| | Bow_Attack_Seq | Bow_Defense_Start_Loop_Seq |
| --- | --- | --- |
| 源 | `D:\share\Bow_Attack_Seq.glb` | `D:\share\Bow_Defense_Start_Loop_Seq.FBX` |
| 输出 | `D:\share\character\rebelsoldier_bow_attack.glb` | `D:\share\character\rebelsoldier_bow_defense.glb` |
| 产物大小 | 78,665,260 B（75.02 MB） | 78,938,296 B（75.28 MB） |
| 新动画 | `Bow_Attack_Seq_0_retarget`，33 channel / 2,673 关键帧 / 1.3333 s / 81 采样点 | `Unreal Take_retarget`，33 channel / 19,866 关键帧 / 10.0167 s / 602 采样点 |
| 映射覆盖率 | 45.6%（源侧 68 根带动画的骨命中 31 根） | 同左（同一套骨架） |
| 腿长比 | 0.6098（源 0.9191 m / 目标 0.5605 m） | 同左 |
| 读回自检 | 3 骨 × 3 时刻角度差全 0.0000°，根链平移差 0.000000 m | 同左（末帧 `Hip` 平移 = 期望） |
| 视觉核对 | 25% 举弓 → 50% 抬至瞄准 → 75% 拉弦后撤，无关节反折 / 穿模 / 躯干错位 | 全程持弓站立，五帧几乎无变化，无异常 |

产物资产保留：mesh 1 / material 1 / texture 1 / image 1 / skin 1 / joints 41 → 41。

转换是确定性的：同一条命令多次运行产出字节完全一致（SHA256 相同），可用它做回归比对。

## 7. 限制

1. **覆盖率 45.6% 不是缺陷**：38 根未映射骨里 30 根是手指（目标角色没有指骨）、7 根是 IK 控制骨、1 根是 Tripo 导出时混入的 `node#0` 空节点。真正参与形变的主干骨（脊柱 / 四肢 / 头颈 / 脚趾）全部命中。
2. **目标侧 12 根骨不被驱动**：`*Twist02` 中段扭转骨（源侧没有同段位骨）、`Pelvis`、`Armature`、Tripo 的节点残留。它们保持 rest 并跟随父骨刚性转动，视觉上正常，但绕骨轴的细节扭转会略硬。
3. **控制骨/IK 驱动的动画无法重定向**：如果源 FBX 的动作是靠 UE 里 Control Rig / IK 求解出来的、导出时**没有烘焙成 FK 曲线**，则 FBX 里只有控制骨的平移，骨骼 FK 旋转全程几乎不动——重定向结果就是一条静止的待机姿态。`Bow_Defense_Start_Loop_Seq` 正是这种情况（602 帧里各主骨旋转变化 < 1°，只有 pelvis 位移 1~7 cm、`ik_hand_l` 摆动约 6 cm），画面表现为「持弓站立呼吸」。**这不是转换器的 bug，是源资产没烘焙**；要主动作请在 UE 里 Bake Animation 后重导。
4. **没有脚部 IK 修正**：不做脚底锁定与地面接触求解，腿长比已按比例缩放，但接地滑动在跨步大的动作里仍可能出现，需要引擎侧 IK 或手动微调。
5. **一次只处理一条动作**：源文件多条动作需逐条 `--anim` 调用，产物是各自独立的 GLB（不合并成多 clip）。
6. **骨架假设**：内置表针对 UE5 Mannequin 命名 + Tripo 人形绑定。换成 Mixamo 或其他绑定需靠 `--map` 补齐，报告里会逐条列缺失骨。
7. **产物体积**：写动画不重打包贴图，所以产物 ≈ 目标资产 + 动画数据（本例 +50 KB ~ +320 KB）。

## 8. 排错

| 现象 | 原因 | 处理 |
| --- | --- | --- |
| 手臂抬不起来 / 动作幅度被吃掉 | 少了 rest 朝向对齐（A-pose vs T-pose） | 确认 `align` 生效；不要退回纯世界增量 |
| 位移大得离谱（几十米） | 源 FBX 厘米单位 + `Armature` 0.01 缩放未累乘 | 检查 `pose_frame` 的祖先缩放链 |
| 动作整体静止 | 源动作没烘焙成 FK 曲线（见限制 3） | 回 UE 重新 Bake 后导出 |
| 四肢发麻花 / 抖动 | twist 辅助骨映射错位 | 试 `--no-twist` 对比 |
| 末帧与首帧对不上 | 源片段本身不是循环；或位移未缩放 | 看报告里首/末帧数值，必要时 `--no-scale-motion` 对比 |
| 退出码 2 | 读回自检超差（角度 > 0.5° 或位移 > 1 mm） | 报告里会打印具体骨名与差值 |
*（内容由AI生成，仅供参考）*
