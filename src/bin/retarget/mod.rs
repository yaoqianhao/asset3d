//! `asset3d-retarget` 的模块树。
//!
//! 分成六层，每层只依赖前一层，便于单测与替换：
//!
//! ```text
//! math    局部 TRS / 四元数增量（纯数学，无 IO）
//! glb     GLB 容器读写、accessor 追加（只碰字节布局）
//! source  把 GLB/FBX 读成 Skeleton + Clip（把「文件」变成「骨架 + 逐帧姿态」）
//! mapping UE→Tripo 骨骼名映射表 + --map 覆盖
//! plan    映射规划 + 逐帧姿态求解（重定向算法本体）
//! writer  把姿态序列写回目标 GLB（只追加不重排）
//! verify  把写出的文件读回来，跟独立复算的期望值对账
//! report  人话报告
//! ```

pub mod glb;
pub mod mapping;
pub mod math;
pub mod plan;
pub mod report;
pub mod source;
pub mod verify;
pub mod writer;
