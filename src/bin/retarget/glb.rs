//! 最小 GLB 读写：只做「读 accessor 浮点数据」和「往现有 GLB 尾部追加 accessor」。
//!
//! 为什么不用 `gltf` crate 写：输出要求是「完整保留目标资产原有 mesh / material /
//! skin / 纹理，只在 JSON 追加 animations 与配套 accessor / bufferView / BIN」。
//! `gltf` crate 的 writer 会重排 accessor 索引、重新打包 BIN，等于把 75 MB 的贴图
//! 和顶点数据整体搬一遍，既有丢纹理的风险，也没必要。这里纯手工追加：
//!
//! - 原有 `bufferViews` 的 offset 全部不动，新数据一律追加到 BIN 末尾；
//! - 每段新数据按 [`F32_ALIGN`]（4 字节）对齐，理由见该常量的注释；
//! - JSON / BIN 两个 chunk 的长度都补齐到 4 的倍数，尾部按容器规范写 `0x20` / `0x00`。

use serde_json::{json, Value};

const MAGIC: u32 = 0x4654_6C67; // "glTF"
const CHUNK_JSON: u32 = 0x4E4F_534A; // "JSON"
const CHUNK_BIN: u32 = 0x004E_4942; // "BIN\0"

/// BIN 与 bufferView 的对齐粒度：本工具只写 f32，分量大小即 4 字节。
///
/// 对齐方案（写死在这一条约束上，不再按 accessor 逐个调整）：
/// - 每段新数据追加前把 BIN 长度补到 4 的倍数 → 新 bufferView.byteOffset 必然是 4 的倍数；
/// - accessor.byteOffset 恒为 0，因此 `accessor.byteOffset + bufferView.byteOffset` 也为 4 的倍数；
/// - byteLength 全是 4 的倍数，于是对齐一旦成立就一直成立；
/// - BIN chunk 末尾同样补到 4 的倍数（GLB 容器规范），补的字节不计入 buffer.byteLength。
const F32_ALIGN: usize = 4;

/// 一个 GLB 容器的两个部件；`json` 是整个 glTF 文档，`bin` 是 BIN chunk 的原始字节。
pub struct Glb {
    pub json: Value,
    pub bin: Vec<u8>,
}

/// 只印规模，不印 BIN：几十 MB 的字节数组进断言日志会淹掉真正的原因。
impl std::fmt::Debug for Glb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys = self
            .json
            .as_object()
            .map(|object| object.len())
            .unwrap_or(0);
        write!(f, "Glb {{ 顶层键 {keys} 个, bin {} 字节 }}", self.bin.len())
    }
}

impl Glb {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 12 {
            return Err(format!(
                "文件只有 {} 字节，连 GLB 头都放不下",
                bytes.len()
            ));
        }
        let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if magic != MAGIC {
            return Err(format!(
                "不是 GLB：文件头 magic=0x{magic:08X}，期望 0x{MAGIC:08X}"
            ));
        }
        let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if version != 2 {
            return Err(format!("只支持 glTF 2.0 的 GLB，这个文件是 version={version}"));
        }
        let declared = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
        let total = declared.min(bytes.len());
        if declared > bytes.len() {
            // 截断文件仍然尽量解析：动画数据在 JSON 里，BIN 缺尾只影响贴图，报清楚就行。
            eprintln!(
                "警告：GLB 头声明 {declared} 字节，实际只有 {} 字节（文件被截断？）",
                bytes.len()
            );
        }

        let mut offset = 12usize;
        let mut json: Option<Value> = None;
        let mut bin: Vec<u8> = Vec::new();
        while offset + 8 <= total {
            let len = u32::from_le_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ]) as usize;
            let kind = u32::from_le_bytes([
                bytes[offset + 4],
                bytes[offset + 5],
                bytes[offset + 6],
                bytes[offset + 7],
            ]);
            let start = offset + 8;
            let stop = start.saturating_add(len).min(total);
            match kind {
                CHUNK_JSON => {
                    let text = std::str::from_utf8(&bytes[start..stop])
                        .map_err(|err| format!("GLB 的 JSON chunk 不是合法 UTF-8：{err}"))?;
                    let text = text.trim_end_matches(['\0', ' ']);
                    json = Some(
                        serde_json::from_str(text)
                            .map_err(|err| format!("GLB 的 JSON chunk 解析失败：{err}"))?,
                    );
                }
                CHUNK_BIN => bin = bytes[start..stop].to_vec(),
                _ => {}
            }
            offset = start.saturating_add(len);
            if len == 0 {
                break;
            }
        }
        let json = json.ok_or_else(|| "GLB 里没有 JSON chunk".to_string())?;
        Ok(Self { json, bin })
    }

    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let mut json_bytes = serde_json::to_vec(&self.json)
            .map_err(|err| format!("序列化 glTF JSON 失败：{err}"))?;
        while json_bytes.len() % 4 != 0 {
            json_bytes.push(b' ');
        }
        let mut bin_bytes = self.bin.clone();
        while bin_bytes.len() % 4 != 0 {
            bin_bytes.push(0);
        }
        let has_bin = !bin_bytes.is_empty();
        let total = 12
            + 8
            + json_bytes.len()
            + if has_bin { 8 + bin_bytes.len() } else { 0 };

        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&CHUNK_JSON.to_le_bytes());
        out.extend_from_slice(&json_bytes);
        if has_bin {
            out.extend_from_slice(&(bin_bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(&CHUNK_BIN.to_le_bytes());
            out.extend_from_slice(&bin_bytes);
        }
        Ok(out)
    }

    fn array(&self, key: &str) -> Option<&Vec<Value>> {
        self.json.get(key).and_then(Value::as_array)
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.json.get(key)
    }

    pub fn arr(&self, key: &str) -> Option<&Vec<Value>> {
        self.array(key)
    }

    pub fn nodes(&self) -> &Vec<Value> {
        // 调用方在 parse 之后立刻检查过非空，这里给个静态空表更省事。
        static EMPTY: Vec<Value> = Vec::new();
        self.array("nodes").unwrap_or(&EMPTY)
    }

    /// 读一个 accessor 的全部标量/向量，返回 (维度, 扁平数据)。
    ///
    /// 只支持 float 分量与整数索引（normalized 会归一化到 [0,1]）；sparse accessor 明确拒绝——
    /// 我们的输入是 UE / Tripo 导出的普通 GLB，不会出现 sparse，遇到就该报错而不是猜。
    pub fn read_accessor(&self, index: usize) -> Result<(usize, Vec<f64>), String> {
        let accessors = self
            .array("accessors")
            .ok_or_else(|| "GLB 里没有 accessors".to_string())?;
        let acc = accessors
            .get(index)
            .ok_or_else(|| format!("accessors[{index}] 不存在（共 {} 个）", accessors.len()))?;
        if acc.get("sparse").is_some() {
            return Err(format!("accessors[{index}] 是 sparse accessor，本工具不支持"));
        }
        let count = acc
            .get("count")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("accessors[{index}] 缺 count"))? as usize;
        let ty = acc
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("accessors[{index}] 缺 type"))?;
        let dims = match ty {
            "SCALAR" => 1,
            "VEC2" => 2,
            "VEC3" => 3,
            "VEC4" => 4,
            "MAT2" => 4,
            "MAT3" => 9,
            "MAT4" => 16,
            other => return Err(format!("accessors[{index}] 的 type={other} 不认识")),
        };
        let ctype = acc
            .get("componentType")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("accessors[{index}] 缺 componentType"))?;
        let normalized = acc
            .get("normalized")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let comp_size = match ctype {
            5120 | 5121 => 1usize,
            5122 | 5123 => 2,
            5125 | 5126 => 4,
            other => return Err(format!("accessors[{index}] 的 componentType={other} 不认识")),
        };
        let view_index = acc
            .get("bufferView")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("accessors[{index}] 没有 bufferView（本工具不支持无视图数据）"))?
            as usize;
        let views = self
            .array("bufferViews")
            .ok_or_else(|| "GLB 里没有 bufferViews".to_string())?;
        let view = views
            .get(view_index)
            .ok_or_else(|| format!("bufferViews[{view_index}] 不存在"))?;
        let view_offset = view
            .get("byteOffset")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let view_len = view
            .get("byteLength")
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("bufferViews[{view_index}] 缺 byteLength"))?
            as usize;
        let acc_offset = acc
            .get("byteOffset")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let stride = view
            .get("byteStride")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(dims * comp_size);

        let elem_bytes = dims * comp_size;
        let start = view_offset
            .checked_add(acc_offset)
            .ok_or_else(|| format!("accessors[{index}] 的偏移溢出"))?;
        if count > 0 && (start + (count - 1) * stride + elem_bytes > view_offset + view_len) {
            return Err(format!(
                "accessors[{index}] 越界：需要读到 {} 字节，bufferViews[{view_index}] 只有 {} 字节",
                start + (count - 1) * stride + elem_bytes - view_offset,
                view_len
            ));
        }
        if start + count * stride > self.bin.len() + stride {
            return Err(format!(
                "accessors[{index}] 的数据超出 BIN chunk（BIN 只有 {} 字节，起点 {start}）",
                self.bin.len()
            ));
        }

        let mut data = Vec::with_capacity(count * dims);
        for i in 0..count {
            let base = start + i * stride;
            if base + elem_bytes > self.bin.len() {
                return Err(format!(
                    "accessors[{index}] 第 {i} 个元素超出 BIN chunk 末尾（BIN {} 字节）",
                    self.bin.len()
                ));
            }
            for c in 0..dims {
                let at = base + c * comp_size;
                let raw = &self.bin[at..at + comp_size];
                let value = match ctype {
                    5120 => (raw[0] as i8) as f64,
                    5121 => {
                        let v = raw[0] as f64;
                        if normalized {
                            v / 255.0
                        } else {
                            v
                        }
                    }
                    5122 => i16::from_le_bytes([raw[0], raw[1]]) as f64,
                    5123 => {
                        let v = u16::from_le_bytes([raw[0], raw[1]]) as f64;
                        if normalized {
                            v / 65535.0
                        } else {
                            v
                        }
                    }
                    5125 => {
                        let v =
                            u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as f64;
                        if normalized {
                            v / 4294967295.0
                        } else {
                            v
                        }
                    }
                    5126 => f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as f64,
                    _ => unreachable!("componentType 已在上文校验"),
                };
                data.push(value);
            }
        }
        Ok((dims, data))
    }

    /// 把 BIN 尾部补零到 `F32_ALIGN` 的整数倍，返回补齐后的长度（即新数据的起点）。
    ///
    /// 为什么是 4：本工具写进 BIN 的只有 f32，glTF 2.0 的两条硬要求合起来就是它——
    /// ① `accessor.byteOffset` 必须是分量大小（f32 = 4）的整数倍；②
    /// `accessor.byteOffset + bufferView.byteOffset` 同样必须是 4 的整数倍。
    /// 本工具追加的 accessor 一律 `byteOffset = 0`、数据整段塞进自己的 bufferView，
    /// 两条要求于是统一成「bufferView.byteOffset 是 4 的倍数」。又因为每个 accessor
    /// 的 byteLength 本身也是 4 的倍数（元素全是 f32），对齐一次之后后续起点天然
    /// 仍然对齐，不需要逐段重算。BIN 末尾同理按 4 补齐，GLB chunk 长度才合法。
    fn align_bin(&mut self) -> usize {
        // 目标资产若声明了比真实 BIN 更长的 buffer（规范允许尾部留空），先把差额补零，
        // 免得我们重算 byteLength 时把这段声明"缩水"，写出自相矛盾的文件。
        let declared = self.declared_buffer_len();
        if declared > self.bin.len() {
            self.bin.resize(declared, 0);
        }
        let pad = (F32_ALIGN - self.bin.len() % F32_ALIGN) % F32_ALIGN;
        self.bin.resize(self.bin.len() + pad, 0);
        self.bin.len()
    }

    /// 原 JSON 里 `buffers[0].byteLength` 声明的长度，缺省按 0。
    fn declared_buffer_len(&self) -> usize {
        self.json
            .get("buffers")
            .and_then(Value::as_array)
            .and_then(|buffers| buffers.first())
            .and_then(|first| first.get("byteLength"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize
    }

    /// 追加一段 f32 accessor（连同它的 bufferView），返回 accessor 索引。
    ///
    /// `min_max` 用于动画 input（时间轴）：glTF 规定 input accessor 必须带 min/max，
    /// 输出（四元数 / 平移）不带，避免给 75 MB 资产再塞多余的极值字段。
    pub fn push_f32_accessor(
        &mut self,
        values: &[f64],
        ty: &str,
        min_max: bool,
    ) -> Result<usize, String> {
        let dims = match ty {
            "SCALAR" => 1,
            "VEC3" => 3,
            "VEC4" => 4,
            other => return Err(format!("push_f32_accessor 不支持 type={other}")),
        };
        if values.len() % dims != 0 {
            return Err(format!(
                "push_f32_accessor：{} 个分量不是 {dims} 的整数倍",
                values.len()
            ));
        }
        let count = values.len() / dims;

        let byte_offset = self.align_bin();
        let mut rounded = Vec::with_capacity(values.len());
        for value in values {
            let q = *value as f32;
            self.bin.extend_from_slice(&q.to_le_bytes());
            // 极值按写入后的 f32 再算一遍，否则 min/max 与真实数据差一个舍入，
            // 有些查看器会直接拒绝时间轴。
            rounded.push(q as f64);
        }
        let byte_len = self.bin.len() - byte_offset;

        let views = self
            .json
            .get_mut("bufferViews")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| "目标资产没有 bufferViews，无法追加动画数据".to_string())?;
        let view_index = views.len();
        views.push(json!({
            "buffer": 0,
            "byteOffset": byte_offset,
            "byteLength": byte_len,
        }));

        let accessors = self
            .json
            .get_mut("accessors")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| "目标资产没有 accessors，无法追加动画数据".to_string())?;
        let accessor_index = accessors.len();
        let mut acc = json!({
            "bufferView": view_index,
            "componentType": 5126,
            "count": count,
            "type": ty,
        });
        if min_max && count > 0 {
            let mut min = vec![f64::INFINITY; dims];
            let mut max = vec![f64::NEG_INFINITY; dims];
            for i in 0..count {
                for c in 0..dims {
                    let v = rounded[i * dims + c];
                    if v < min[c] {
                        min[c] = v;
                    }
                    if v > max[c] {
                        max[c] = v;
                    }
                }
            }
            if dims == 1 {
                acc["min"] = json!([min[0]]);
                acc["max"] = json!([max[0]]);
            } else {
                acc["min"] = json!(min);
                acc["max"] = json!(max);
            }
        }
        accessors.push(acc);

        // buffers[0].byteLength 必须覆盖实际写到的字节。align_bin 已经把「声明得比实际
        // 更长」的差额补成零了，所以这里写实际长度不会让任何已有声明缩水。
        if let Some(buffers) = self.json.get_mut("buffers").and_then(Value::as_array_mut) {
            if let Some(first) = buffers.get_mut(0) {
                let declared = first
                    .get("byteLength")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                first["byteLength"] = json!(declared.max(self.bin.len()));
            }
        }
        Ok(accessor_index)
    }

    /// 统计目标资产里「必须原样保留」的东西，供报告与读回自检比对。
    pub fn asset_counts(&self) -> AssetCounts {
        let count = |key: &str| self.array(key).map(Vec::len).unwrap_or(0);
        let joints = self
            .array("skins")
            .and_then(|skins| skins.first())
            .and_then(|skin| skin.get("joints"))
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        let animations = self.array("animations").map(Vec::len).unwrap_or(0);
        let channels: usize = self
            .array("animations")
            .map(|anims| {
                anims
                    .iter()
                    .map(|a| {
                        a.get("channels")
                            .and_then(Value::as_array)
                            .map(Vec::len)
                            .unwrap_or(0)
                    })
                    .sum()
            })
            .unwrap_or(0);
        AssetCounts {
            nodes: count("nodes"),
            meshes: count("meshes"),
            materials: count("materials"),
            textures: count("textures"),
            images: count("images"),
            skins: count("skins"),
            joints,
            animations,
            channels,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AssetCounts {
    pub nodes: usize,
    pub meshes: usize,
    pub materials: usize,
    pub textures: usize,
    pub images: usize,
    pub skins: usize,
    pub joints: usize,
    pub animations: usize,
    pub channels: usize,
}

impl std::fmt::Display for AssetCounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "nodes={} meshes={} materials={} textures={} images={} skins={} joints={} animations={} channels={}",
            self.nodes,
            self.meshes,
            self.materials,
            self.textures,
            self.images,
            self.skins,
            self.joints,
            self.animations,
            self.channels
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_glb() -> Vec<u8> {
        let json = json!({
            "asset": {"version": "2.0"},
            "nodes": [{"name": "a"}],
            "buffers": [{"byteLength": 0}],
            "bufferViews": [],
            "accessors": [],
        });
        let glb = Glb {
            json,
            bin: Vec::new(),
        };
        glb.encode().expect("编码")
    }

    #[test]
    fn encode_then_parse_round_trip() {
        let bytes = tiny_glb();
        // 总长与 chunk 长度都必须 4 字节对齐。
        assert_eq!(bytes.len() % 4, 0);
        let glb = Glb::parse(&bytes).expect("解析");
        assert_eq!(glb.asset_counts().nodes, 1);
    }

    #[test]
    fn push_accessor_aligns_and_updates_byte_length() {
        let mut glb = Glb::parse(&tiny_glb()).expect("解析");
        glb.bin = vec![1, 2, 3]; // 故意留一个非 4 字节对齐的尾巴
        let idx = glb
            .push_f32_accessor(&[0.0, 0.5, 1.0], "SCALAR", true)
            .expect("追加");
        assert_eq!(idx, 0);
        // 3 字节尾巴补 1 个零 → 起点 4；写 3 个 f32 共 12 字节 → BIN 16 字节。
        assert_eq!(glb.bin.len(), 16);
        assert_eq!(&glb.bin[..4], &[1, 2, 3, 0]);
        let view = &glb.json["bufferViews"][0];
        assert_eq!(view["byteOffset"], 4);
        assert_eq!(view["byteLength"], 12);
        // byteLength 必须覆盖到真实末尾（16），不能停在第 15 字节。
        assert_eq!(glb.json["buffers"][0]["byteLength"], 16);
        // min/max 按写入后的 f32 复算
        assert_eq!(glb.json["accessors"][0]["min"][0], 0.0);
        assert_eq!(glb.json["accessors"][0]["max"][0], 1.0);
        let (dims, data) = glb.read_accessor(0).expect("读回");
        assert_eq!(dims, 1);
        assert_eq!(data, vec![0.0, 0.5, 1.0]);
    }

    /// 连续追加必须段段对齐，且每段落点严丝合缝（不留缝、不重叠）。
    #[test]
    fn consecutive_appends_stay_aligned() {
        let mut glb = Glb::parse(&tiny_glb()).expect("解析");
        glb.bin = vec![9; 5]; // 5 字节尾巴，比 4 的倍数多 1
        let a = glb
            .push_f32_accessor(&[0.0, 1.0], "SCALAR", false)
            .expect("第一段");
        let b = glb
            .push_f32_accessor(&[0.0, 0.0, 0.0, 1.0], "VEC4", false)
            .expect("第二段");
        let c = glb
            .push_f32_accessor(&[1.0, 2.0, 3.0], "VEC3", false)
            .expect("第三段");
        assert_eq!((a, b, c), (0, 1, 2));

        let views = glb.json["bufferViews"].as_array().expect("bufferViews");
        assert_eq!(views.len(), 3);
        let mut cursor = 0usize;
        for view in views {
            let offset = view["byteOffset"].as_u64().expect("byteOffset") as usize;
            let len = view["byteLength"].as_u64().expect("byteLength") as usize;
            // 方案的两条硬要求：起点是 4 的倍数，长度也是 4 的倍数
            assert_eq!(offset % F32_ALIGN, 0, "bufferView 起点未对齐：{offset}");
            assert_eq!(len % F32_ALIGN, 0, "bufferView 长度未对齐：{len}");
            // 逐段检查缓冲不重叠：本工具只追加，不允许回头覆盖已有数据
            assert!(offset >= cursor, "第 {offset} 字节处与上一段重叠");
            cursor = offset + len;
        }
        assert_eq!(cursor, glb.bin.len());
        // 5 补到 8，+8（SCALAR×2）+16（VEC4×4）+12（VEC3×3）= 44
        assert_eq!(glb.bin.len(), 44);
        assert_eq!(glb.json["buffers"][0]["byteLength"], 44);
        let (dims, data) = glb.read_accessor(1).expect("读回第二段");
        assert_eq!(dims, 4);
        assert_eq!(data, vec![0.0, 0.0, 0.0, 1.0]);
    }

    /// 原 JSON 声明了比真实 BIN 更长的 buffer 时，追加不能把这段声明"缩水"掉。
    #[test]
    fn byte_length_never_shrinks_below_declaration() {
        let json = json!({
            "asset": {"version": "2.0"},
            "buffers": [{"byteLength": 64}],
            "bufferViews": [],
            "accessors": [],
        });
        let mut glb = Glb {
            json,
            bin: vec![0; 8],
        };
        glb.push_f32_accessor(&[1.0, 2.0], "SCALAR", false)
            .expect("追加");
        // 先补到声明的 64，再从 64 起写 8 字节
        assert_eq!(glb.json["bufferViews"][0]["byteOffset"], 64);
        assert_eq!(glb.json["buffers"][0]["byteLength"], 72);
    }

    /// 写出的文件必须能被自己的 parser 读回来 —— 对齐错了这里会直接暴露。
    #[test]
    fn appended_data_survives_encode_parse_round_trip() {
        let mut glb = Glb::parse(&tiny_glb()).expect("解析");
        glb.bin = vec![7; 3];
        glb.push_f32_accessor(&[0.25, -0.5, 0.75], "VEC3", false)
            .expect("追加");
        let bytes = glb.encode().expect("编码");
        assert_eq!(bytes.len() % 4, 0);
        let back = Glb::parse(&bytes).expect("读回");
        let (dims, data) = back.read_accessor(0).expect("读回数据");
        assert_eq!(dims, 3);
        assert_eq!(data, vec![0.25, -0.5, 0.75]);
    }

    #[test]
    fn read_accessor_rejects_out_of_range() {
        let mut glb = Glb::parse(&tiny_glb()).expect("解析");
        glb.push_f32_accessor(&[1.0, 2.0], "SCALAR", false).expect("追加");
        // 人为把 count 改大，读回必须报错而不是读到隔壁数据。
        glb.json["accessors"][0]["count"] = json!(9);
        let err = glb.read_accessor(0).expect_err("应当越界");
        assert!(err.contains("越界"), "{err}");
    }

    #[test]
    fn rejects_non_glb() {
        let err = Glb::parse(b"not a glb at all").expect_err("应当失败");
        assert!(err.contains("不是 GLB"), "{err}");
    }
}
