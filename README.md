# ocs — OpenCode 会话全文检索

跨**全部项目**翻 OpenCode 的历史对话。1 秒扫完 3 GB 库，直接给你一条能粘贴执行的续接命令。

```console
$ ocs find 妙搭 局限 -n 3
```

## 为什么要这个东西

OpenCode 按**启动目录**把会话归属到不同的 project，`opencode session list` 只列当前目录那一个 project 的会话。TUI 里的 `Ctrl+O` 能跨项目，但只能按标题和最近活动找。

而人真正想找的往往是这些：

- 三个月前在**别的项目**里调到半夜的那段 Nginx 配置
- 只记得聊过「妙搭的局限」，但那个会话标题叫「lark-apps 适用项目与限制」
- 一个报错码，只在**工具输出**里出现过一次

这些东西全部只存在于正文。标题里没有，最近活动里也没有。所以你会在同一件事上反复重新查一遍。

现有工具都不解决这个：`lazyocs` 只索引标题和最后一句用户消息；Rust 写的 `opencode-session-search` 只做元数据模糊搜索；插件形态的 `opencode-session-recall` 要额外维护一套派生索引。`ocs` 直接对 `opencode.db` 做全文扫描，不建索引、不写库。

## 特性

- **全文检索**：扫 `session_message`（V2）与 `part`（V1）落盘的全部消息，包括 assistant 正文、工具调用与工具输出
- **跨项目**：一次读 `~/.local/share/opencode/opencode.db`、`/root/...`、`/home/louis/...`，去重合并
- **只读**：`mode=ro` 打开 SQLite，不可能写坏库
- **不乱排序**：覆盖度 → log 词频 → 时间（见下）
- **单二进制**：`rusqlite` 静态 bundled，无运行时依赖
- **`--json`**：结构化输出，可直接喂给 agent

## 安装

```sh
git clone https://github.com/LouisLau-art/opencode-session-finder.git
cd opencode-session-finder
cargo build --release
install -m755 target/release/ocs /usr/local/bin/ocs
```

要求 Rust 1.75+。首次编译会静态编译 SQLite，约 1 分钟。

## 用法

### 找

```sh
ocs find <关键词...> [--since 30d] [--json] [-n 15]
```

每个关键词只要在这个会话的**任意**位置出现过就算命中，不要求落在同一条消息 —— 一个话题通常跨好几轮对话。

真实输出：

```console
$ ocs find 妙搭 限制 -n 2

找到 2 个匹配会话：

  ses_ec502f190ffeCGXCeSM4GdQqkP  2026-10-14 23:17  /home/louis/Projects/feishu-docs
  「lark-apps 适用项目与限制：ERP/IPD/video-finder」
  命中: 妙搭×101  限制×10
  [user] 现在需要你帮我看看 @lark-apps 这个东西适合做哪些项目 有什么使用限制...
  续接: cd /home/louis/Projects/feishu-docs && opencode -s ses_ec502f190ffeCGXCeSM4GdQqkP
```

### 列

```sh
ocs list [-n 40] [--json]
```

跨项目、按最近活动排序。想不起来关键词时用它按日期和目录捞。

### 看

```sh
ocs show <ID 前缀 | ID 中段 | 标题子串> [--md] [--json]
```

三种定位方式都行，`ec502f19` 和 `lark-apps` 都能打开同一个会话。匹配到多个会把候选列出来。

### 续接

```sh
ocs open <同一个定位方式>
```

只打印那条 `cd ... && opencode -s ...`，方便复制。

## 排序规则

按命中总数排序是错的：一个把「妙搭」念了 444 次的会话，会永久压过另一个真正花十轮讨论局限在哪的会话。

`ocs` 的排序是三段式：

1. **覆盖度** —— 查询里的词命中了几个。全中的排前面，部分命中的留着但排后面（用户记住的词经常只是原文的同义词）。
2. **log 词频** —— `Σ ln(1 + count)`。压掉刷屏式重复，同时保留「讨论得深」这个信号。
3. **时间** —— 同样相关时，新的优先。

`--json` 里会原样给出 `covered`、`hit_counts`、`score`，方便上层自己重新排。

## 性能

3.3 GB 数据库、两个关键词、全表扫描：

| | 耗时 |
|---|---|
| `ocs find 妙搭 限制` | ~1.0 s |
| `ocs list -n 40` | ~0.15 s |

单次 `LIKE` 全扫是刻意选的：建倒排索引意味着要跟着 OpenCode 的 schema 更新，并且在几 GB 的库上多维护一份数据。1 秒对交互式使用足够。

## 局限

- 不做语义检索，同义词要自己换（`局限` / `限制` / `边界`）。配套的 `find-session` skill 会把「换词再搜」写成固定步骤。
- 不支持 OpenCode 的远程/托管部署，只读本地 SQLite。
- schema 探测是宽松的（表存在性 + 数据量），OpenCode 大改 schema 时需要跟着调整。

## License

MIT
