# hachimi_training_badge

给《赛马娘》（Windows / Steam 日文版）育成界面加「训练推荐角标」的 **Hachimi Edge v3 插件**。

外部 AI 每回合把决策写进本地 `decision.jsonl`，插件读取它，在 5 个训练按钮之一上挂一张小图，让你不用切窗口就能看到当前推荐。

- 只在游戏自己的 UI 树上挂一个 `RawImage + Texture2D`，**不发包、不改内存数值、不碰存档**
- 决策按回合自动过期：新回合开始、AI 还在算时角标自动隐藏，不残留旧推荐
- 零第三方 Rust 依赖，`cargo build --release` 直接出 DLL
- 仓库**不含预编译 DLL**，请自行编译（见下）

---

## 目录结构

```
hachimi_training_badge/
├─ Cargo.toml / Cargo.lock        # cdylib，release: strip + panic=abort
├─ src/
│  ├─ lib.rs                     # hachimi_init_v3 入口、配置解析、mode 分发
│  ├─ ai.rs                      # decision.jsonl / thisTurn.json → 槽位与图标
│  ├─ badge.rs                   # IL2CPP UI 调用：RawImage / Texture2D / 角标同步
│  ├─ probe.rs                   # hook 与每帧 tick、按钮枚举、AI 状态轮询
│  └─ demo.rs                    # 历史测试用的 Win32 透明 overlay（默认不启用）
├─ icons/                        # 运行时图标，需与配置文件同级部署
├─ training_badge.json           # 配置示例（部署时复制到 hachimi/ 并改路径）
├─ tools/selfcheck_probe3.py     # 离线端到端自检（不需要游戏）
├─ .gitignore
└─ README.md
```

---

## 它能显示什么

5 个训练按钮按**世界坐标 X 升序 = 屏幕从左到右**映射为：

| 槽位 | 1 | 2 | 3 | 4 | 5 |
| --- | --- | --- | --- | --- | --- |
| 训练 | 速度 | 耐力 | 力量 | 根性 | 智力 |

决策 → 角标：

| 决策类型 | 图标文件 | 挂在 |
| --- | --- | --- |
| 训练（速/耐/力/根/智，含剧本别名「流镝/神旗/纳舞/大曳/飞悬」「稽古」） | `train.png` | 对应的训练按钮 |
| 比赛 | `race.png` | 槽位 3（力量按钮） |
| 友人出行 / 普通出行 | `outing.png` | 槽位 3 |
| 休息 | `rest.png` | 槽位 3 |
| 等待计算 / 其它 / 决策过期 | **不显示**（清空全部角标） | — |

> `wait.png` 会被加载，但当前版本**没有「AI 计算中」状态**：等待态的 `icon_key()` 返回空串，`sync_badges` 直接隐藏角标。这是刻意设计（`src/ai.rs`）。

---

## 前置条件

| 项 | 要求 |
| --- | --- |
| 系统 / 游戏 | Windows x86_64 + Steam 日文版《赛马娘》 |
| 宿主 | **Hachimi Edge，API v3**（导出 `hachimi_init_v3`），已能正常注入游戏 |
| 编译 | Rust 1.96+；本机为 `x86_64-pc-windows-gnu`，无需 MSVC v143 |
| 上游数据 ① | AI 决策流 `decision.jsonl`（例如 umaai-matsuri 的 `matsuri online --json` 输出） |
| 上游数据 ② | 帧写入目录中的 `thisTurn.json`，内含 `baseGame.turn` |

**本仓库只负责显示**。AI 决策程序与帧写入端（`SendGameStatusPlugin` 一类工具）不在本仓库内，需要你自己的工具链产出；数据契约见下文。

---

## 编译

```bash
cd hachimi_training_badge
cargo build --release
```

产物：`target/release/hachimi_training_badge.dll`。

可选的离线自检（不需要游戏、不需要 IL2CPP，用 ctypes 模拟 Hachimi v3 API 并直接调 `hachimi_init_v3`）：

```bash
python tools/selfcheck_probe3.py
# 末尾输出 RESULT: ALL PASS 即通过
```

自检脚本依赖 `target/release/` 下的 DLL 与 `../icons/`，请先编译再跑。

---

## 部署

设 `<game>` 为游戏根目录（含 `UmamusumePrettyDerby_Jpn.exe`）。

**1. 放 DLL**

```
<game>/hachimi/hachimi_training_badge.dll
```

**2. 注册插件**

在 `<game>/hachimi/config.json` 的 `load_libraries` 数组里**新增一项**，保留原有项：

```json
{
  "load_libraries": [
    "hachimi\\hachimi_httpforward_plugin.dll",
    "hachimi\\hachimi_training_badge.dll"
  ]
}
```

路径也写绝对路径。改前先备份 `config.json`。

> Hachimi 是 **late loading**：`load_libraries` 只在注入时读一次，改配置不会热生效，也不会热替换已加载的 DLL。必须**完全退出游戏进程再启动**。

**3. 放配置与图标（必须与配置文件同级）**

```
<game>/hachimi/training_badge.json
<game>/hachimi/icons/train.png
<game>/hachimi/icons/wait.png
<game>/hachimi/icons/race.png
<game>/hachimi/icons/outing.png
<game>/hachimi/icons/rest.png
```

插件依次在 Hachimi 的 `base_dir`、`data_path` 下查找 `training_badge.json` 或 `hachimi/training_badge.json`；找到后以**配置文件所在目录**为基准读 `icons/<key>.png`。找不到配置、或 `enabled != "true"`，插件直接 idle，不安装任何 hook。

**4. 改路径后启动游戏**

把示例配置里的 `decision_path` / `frame_dir` 改成你本机的实际路径（见下一节），然后启动游戏，进入育成界面即可。

**回滚**：`training_badge.json` 的 `enabled` 改 `false`（最轻）→ 从 `load_libraries` 删掉该项 → 删除 DLL。

---

## 配置 `training_badge.json`

仓库内同名文件即为示例。配置用极简 JSON 读取器解析（无第三方 crate）：一行一个 `"key": value`，**不要写注释、嵌套对象或数组**。改完需重启游戏。

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `enabled` | `false` | 总开关；不是 `"true"` 时插件完全 idle |
| `mode` | `idle` | `probe` = 正式角标模式；`demo` = 历史测试 overlay；其它值 idle |
| `attach` | `true` | 是否加载图标并挂角标；`false` 时只跑探针 |
| `decision_path` | `""` | **必改**，AI 决策 JSONL 文件全路径 |
| `frame_dir` | `C:/Ai/...` | **必改**，帧写入目录，读其中 `thisTurn.json` 的 `baseGame.turn` |
| `stale_secs` | `300` | 决策文件超过该秒数未更新 → 等待态（代码下限 5 秒） |
| `offset_y` | `-150`（probe）/ `24`（demo） | 角标垂直偏移，负值向下；两个模式共用该字段但默认值不同 |
| `max_scenes` | `12` | probe 场景 dump 上限，达到后停止 dump 但 hook 保留 |
| `number_all` | `false` | 诊断用：给画面上所有按钮编号（会挂额外角标） |
| `badge_bmp` | `badge_demo.bmp` | 仅 `demo` 模式，需自备 BMP 文件 |
| `interval_ms` / `offset_x` | `2000` / `24` | 仅 `demo` 模式 |
| `json_path` / `note` | — | 历史遗留 / 备注字段，当前代码**不读取** |

图标说明：`icons/` 下文件名固定为 `train.png` / `wait.png` / `race.png` / `outing.png` / `rest.png`，PNG 格式，显示尺寸取 PNG 自身的像素尺寸。

---

## 决策流数据契约

`decision_path` 指向 JSONL 文件，每行一个 JSON 对象。插件从文件尾部**倒序**找最近一条动作决策，跳过握手 / 事件 / `final` 等行：

```json
{"type":"decision","turn":76,"decision_kind":"train","action_index":1,"candidate_descriptions":["流镝稽古","神旗稽古","纳舞稽古","大曳稽古","飞悬稽古","休息","普通出行"],"candidate_scores":[],"score":101802.0}
```

| 字段 | 用途 |
| --- | --- |
| `type` | 必须为 `"decision"`，其它类型按等待态处理 |
| `turn` | 当前回合；与 `thisTurn.json` 的 `baseGame.turn` 是**同一个计数器** |
| `action_index` | 指向 `candidate_descriptions` 中被选中的动作 |
| `candidate_descriptions` | 动作文本；`src/ai.rs` 的 `classify()` 据此映射到槽位/图标 |

两个容易踩的点：

1. `is_action_decision()` 只接受候选列表里**含「训练」或「稽古」**的行。像 `{"candidate_descriptions":["比赛"]}` 这种独立比赛决策会被跳过，落到等待态（角标隐藏）。要让比赛/出行/休息生效，该决策行需与训练候选项同列。
2. 过期判定是 `decision.turn < baseGame.turn`，即游戏进入下一回合后旧推荐立刻失效。`thisTurn.json` 不存在时不作过期判定。**不要**用目录里最大的 `game*_turn*.json` 编号当回合数：目录会保留历史局的文件，会把所有决策误判为过期。

---

## 工作原理（简述）

- **入口**：`hachimi_init_v3(get_api, version)`。所有 API 按名字解析并逐个判空，缺失即返回 `InitResult::Error`，绝不对空指针做调用。
- **hook**：通过 `interceptor_hook` 挂 `Gallop.GetCurrentViewController`（场景转场）和 `Gallop.TextCommon.Update`（每帧主线程 tick）。`GetCurrentViewController` 只在转场时触发，不能当 tick 用。**所有 IL2CPP 调用都在 hook 内、游戏自己的线程上**——在渲染线程/线程池盲调托管方法会直接终止进程。
- **状态刷新**：`TextCommon.Update` 里每 100 ms 做一次纯文件读的 AI 轮询（不做任何 IL2CPP 调用）；状态变化立即触发一次按钮同步，常规扫描按 tick 节流。AI 状态刷新不绑在按钮扫描上，否则会错过「新回合 → AI 决策」的 1~2 秒窗口。
- **按钮定位**：`FindObjectsOfType(UnityEngine.UI.Button)` → 路径含 `trainingfooteritem` 过滤 → `RectTransform.GetWorldCorners` 求中心 X 排序。枚举顺序是从右到左，不能当屏幕顺序用。
- **角标**：`RawImage + Texture2D`，挂在训练项节点下、作为最后一个子节点，GameObject 命名 `TB_Badge_<icon>`；每遍从实时对象重新解析指针，按名字 show/hide。**不缓存跨帧的托管对象指针**——场景 teardown 后调用已回收的 wrapper 曾导致比赛画面崩溃。

---

## 验证与排障

运行日志在游戏根目录的 **`hachimi.log`**（不在 `hachimi/` 子目录），标签 `TrainingBadge`；崩溃栈看 `%LOCALAPPDATA%Low/Cygames/UmamusumePrettyDerby_Jpn/Player.log`。

| 日志 | 含义 |
| --- | --- |
| `[INIT] api v3 base_dir=...` / `[CFG] loaded ...` | 插件已加载、配置已找到 |
| `[INIT] icon train <- ... (N bytes)` | 图标读取成功；`icon ... missing` 说明 `icons/` 位置不对 |
| `[INIT] badge kit ready` | IL2CPP 方法解析完成；`NOT ready` 则不会挂角标 |
| `[AI] poll #N state=... slot=... icon=...` | 每 5 秒一条心跳，用它判断轮询是否活着 |
| `[AI] decision turn=... -> ... (icon on slot N)` | 新决策被解析 |
| `[BTN] pass=N items=5 stable=true` | 找到 5 个训练按钮且界面已稳定 |
| `[BADGE] ...` / `[ITEM] children(...)` | 角标同步；`total` 持续累加说明旧角标没清掉 |

| 现象 | 排查 |
| --- | --- |
| 完全没有 `TrainingBadge` 日志 | DLL 没被加载：`load_libraries` 是否新增、是否重启过游戏、Hachimi 是否 v3 |
| `[INIT] no training_badge.json found -> idle` | 配置不在 `base_dir` / `data_path` 的搜索路径里 |
| 有决策但角标不出现 | `decision_path` 路径、文件 mtime 是否超过 `stale_secs`、`frame_dir` 下 `thisTurn.json` 是否存在、`decision.turn` 是否小于当前回合、候选是否含「训练/稽古」 |
| 角标盖住文字 / 位置不对 | 调 `offset_y`；显示尺寸由 PNG 像素尺寸决定 |
| 游戏崩溃 | 先看 `Player.log` 栈里是否出现 `hachimi_training_badge`；立刻把 `enabled` 改 `false` 回滚 |

---

## 免责声明

这是**第三方本地 UI 插件**，只读本地 JSON、只在游戏 UI 树上叠加图片，不修改网络请求、内存数值与存档。但注入 DLL 等于在游戏进程内以同等权限运行，可能违反游戏服务条款或触发风控，使用前请自行评估。本项目仅用于逆向与学习交流，作者不对封号、崩溃或数据损失负责。

## License

MIT — 见 [LICENSE](LICENSE)。
