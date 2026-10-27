/* 在 Node 里把生成的预览页 JS 真跑一遍：分片合并、缺片/坏片容错、mime 魔数回退。
 *
 * 只截取到 `const AIR = loadAir();` 为止——后面全是 WebGL 初始化，Node 里没有 GL
 * 上下文，硬跑会淹没在无关报错里。这一段正好是「数据能不能进页面」的全部逻辑。
 *
 * 用法：node dev/verify_viewer.js .tmp-ws/big.preview.html
 */
"use strict";

const fs = require("fs");
const path = require("path");
const vm = require("vm");

const htmlPath = process.argv[2];
if (!htmlPath) {
  console.error("用法：node dev/verify_viewer.js <preview.html>");
  process.exit(2);
}

const dir = path.dirname(htmlPath);
const html = fs.readFileSync(htmlPath, "utf8");

// 1) 收齐分片：按 HTML 里的 <script src> 顺序读文件。
const srcs = [...html.matchAll(/<script src="([^"]+)"><\/script>/g)].map((m) => m[1]);
const shardTexts = srcs.map((name) => fs.readFileSync(path.join(dir, name), "utf8"));

// 2) 切出页面自己的脚本，截到 `const AIR = loadAir();`。
const inline = [...html.matchAll(/<script>([\s\S]*?)<\/script>/g)]
  .map((m) => m[1])
  .filter((body) => body.includes("function loadAir"))
  .pop();
if (!inline) {
  console.error("HTML 里找不到 loadAir（预览页结构不对）");
  process.exit(2);
}
const cut = inline.indexOf("const AIR = loadAir();");
if (cut < 0) {
  console.error("HTML 里找不到 AIR 初始化");
  process.exit(2);
}
const core = inline.slice(0, cut + "const AIR = loadAir();".length);

const elems = new Map();
function elem(id) {
  if (!elems.has(id)) {
    elems.set(id, {
      id,
      textContent: "",
      style: {},
      classList: { add() {}, remove() {} },
    });
  }
  return elems.get(id);
}

function run(shards) {
  for (const [, el] of elems) {
    el.textContent = "";
    el.style = {};
  }
  const sandbox = {
    window: {},
    document: { getElementById: elem, createElement: () => elem("tmp") },
    console,
    Image: function Image() {},
    performance: { now: () => 0 },
  };
  sandbox.globalThis = sandbox;
  sandbox.window.__AIR_SHARDS__ = shards;
  // sniffMime 定义在 `const AIR` 之后（贴图那段），单独抠出来一起跑。
const sniffSrc = inline.match(/function sniffMime[\s\S]*?\n}/)[0];
const code = `${core}\n${sniffSrc}\n;globalThis.__probe = { AIR, loadAir, sniffMime, SHARD_META };`;
  vm.createContext(sandbox);
  vm.runInContext(code, sandbox, { filename: "preview-inline.js" });
  return {
    probe: sandbox.__probe,
    title: (elems.get("loadfail-title") || { textContent: "" }).textContent,
    detail: (elems.get("loadfail-detail") || { textContent: "" }).textContent,
  };
}

const failures = [];
function check(ok, label, extra = "") {
  console.log(`[${ok ? "ok  " : "fail"}] ${label}${extra ? "：" + extra : ""}`);
  if (!ok) failures.push(label);
}

// --- 正常合并 ---
const parsed = shardTexts.map((text) => {
  const body = text.slice('window.__AIR_SHARDS__.push('.length).replace(/\);\s*$/, "");
  return JSON.parse(body);
});
const full = run(parsed);
const air = full.probe.AIR;
if (!air) {
  console.error("分片合并失败：" + full.title + " / " + full.detail);
  process.exit(1);
}
const triangles = (air.meshes || []).reduce((sum, m) => sum + (m.indices || []).length, 0) / 3;
check(triangles >= 24000, `分片合并出几何（${triangles} 三角形）`);
check(full.title === "", "正常合并不该弹失败面板");
check(full.probe.SHARD_META.total === srcs.length, `分片清单与文件数一致（${srcs.length}）`);

// --- 缺片容错：少给一片，页面必须说清缺的是哪个文件 ---
const dropped = parsed.slice(1);
const missing = run(dropped);
check(missing.probe.AIR === null, "缺片时 AIR 为 null（不白屏瞎画）");
check(
  missing.title.startsWith("数据分片缺失"),
  "缺片时页面给出提示",
  `${missing.title}｜${missing.detail}`.slice(0, 120)
);
check(
  missing.detail.includes(full.probe.SHARD_META.files[0]),
  "缺片提示写明缺失文件名"
);

// --- 坏片容错：内容不是合法 JSON ---
const broken = parsed.map((shard) => ({ ...shard }));
broken[broken.length - 1].t = "{\"format\":";
const corrupt = run(broken);
check(corrupt.probe.AIR === null, "坏片时 AIR 为 null");
check(corrupt.title.startsWith("数据分片损坏"), "坏片时页面给出提示", corrupt.title);

// --- mime 魔数回退 ---
const sniff = full.probe.sniffMime;
check(sniff("iVBORw0KGgoAAAANSUhEUg", "") === "image/png", "PNG 魔数 → image/png");
check(sniff("/9j/4AAQSkZJRgABAQ", "image/png") === "image/jpeg", "JPEG 魔数压过错误声明");
check(sniff("R0lGODlhAQABAIAA", "") === "image/gif", "GIF 魔数 → image/gif");
check(sniff("u77eQCAAQ", "IMAGE/PNG") === "image/png", "无魔数时回落到声明的 mime");

if (failures.length) {
  console.error(`\n失败 ${failures.length} 项`);
  process.exit(1);
}
console.log("\nviewer 分片合并 / 容错 / mime 回退实测通过");
