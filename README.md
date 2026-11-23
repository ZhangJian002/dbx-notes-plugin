# DBX Notes

轻量、Notion 风格的 Markdown 笔记插件，可作为 DBX 的工作台使用，并支持把笔记库同步到 **WebDAV** 或 **Git**。

- 页面树：无限层级子页面，支持折叠、拖拽排序、拖到其他页面下成为子页面；左侧栏可隐藏
- 首页：内置完整 Markdown 样式演示，默认分栏；左侧可以随意编辑查看效果，但改动不会保存；不占用页面树、不能作为其他页面的父页面；通过侧栏顶部的房子图标打开
- 子页面：当前页面的直接子页面以右下角小按钮显示，默认收起；点开后列出子页面，点击即可跳转
- 编辑体验：普通页面是编辑与预览融合的所见即所得编辑器，输入 Markdown 标记立即变成对应元素；支持字体样式、列表、任务、引用、代码块、分隔线、表格、链接、图片和文件；格式操作统一在右键菜单；粘贴 Markdown 自动渲染，复制整块内容时剪贴板里是 Markdown；格式修改支持 Cmd+Z 撤销，表格增删行列也支持一次 Cmd+Z 回退
- 标签页：首页和打开的笔记各自占一个标签，首页标签可关闭（通过侧栏房子图标重新打开）；标签栏可切换单行横向滚动 / 多行换行，打开的标签和顺序会记住
- 整理：收藏（实心星标）、标签、全文搜索、回收站（整棵子树一起删除 / 恢复）
- 页面图标：内置 Notion 风格 emoji 选择器
- 图片 / 文件：编辑器内粘贴图片、拖入文件，或用工具栏“添加附件”；图片直接预览，文件可点击下载
- 同步：WebDAV（Basic Auth + ETag）或 Git（HTTPS / SSH），支持自动同步；仅本地模式不显示同步按钮
- 可读镜像：同步时同时写入 `DBX-notes/<标题>-<id>.md`
- 跨平台：macOS（Apple Silicon / Intel）、Windows x64、Linux x64 / arm64
- 界面文字跟随 DBX 的语言设置（中文 / English），并跟随 DBX 主题（浅色、深色）

## 环境要求

- DBX >= 0.5.68
- 构建时需要 Rust 1.85+；Git 同步使用系统 `git` 命令
- Windows 需要安装 Git for Windows（并确保 `git.exe` 在 PATH 中）
- 打包 CLI：`npm install --global @dbx-app/plugin-cli`（需要 Node.js >= 18.18）

## 构建与安装

插件包含 Rust sidecar，因此**每个平台需要各自的原生产物**。在 5 个原生 runner 上分别执行 `dbx-plugin package .`：

| 目标 | 平台 |
| --- | --- |
| `darwin-arm64` | macOS Apple Silicon |
| `darwin-x64` | macOS Intel |
| `windows-x64` | Windows 10/11 x64 |
| `linux-x64` | Linux x64（glibc 2.34+） |
| `linux-arm64` | Linux arm64（glibc 2.34+） |

在目标机器上构建：

```bash
cd dbx-notes-plugin
dbx-plugin package .
```

产物在 `dist/`：

```text
dist/io.github.zjay.notes-0.1.1-darwin-arm64.dbxp
dist/io.github.zjay.notes-0.1.1-darwin-x64.dbxp
dist/io.github.zjay.notes-0.1.1-windows-x64.dbxp
dist/io.github.zjay.notes-0.1.1-linux-x64.dbxp
dist/io.github.zjay.notes-0.1.1-linux-arm64.dbxp
dist/*.artifact.json
dist/SHA256SUMS
```

推送 GitHub Release 后，`.github/workflows/release.yml` 会调用 DBX 官方 reusable workflow，按上面的矩阵自动生成全部候选包。

如果已经用 Docker / 交叉工具链构建好了某个目标的二进制，可以用 `scripts/package-target.py` 打包（`dbx-plugin package` 会拒绝非当前宿主目标）：

```bash
DBX_PACKAGER=/path/to/dbx-plugin-packager \
  python3 scripts/package-target.py \
  --target linux-x64 \
  --binary backend/target-linux-x64/release/dbx-plugin-dbx-notes-plugin
```

`.dbxp` 是未签名的开发候选包。在 DBX 的插件中心打开“开发安装”开关后选择对应平台的该文件安装；正式分发需要由 DBX Store 仓库密钥签名。

### 安装时报 “must have a trusted Ed25519 signature”

这是 DBX 的正常校验，不是安装包损坏。`dbx-plugin package` 生成的是**未签名开发包**，只允许通过开发开关安装：

1. 打开 DBX 顶部工具栏的“插件中心”。
2. 切换到“设置”。
3. 展开“第三方与开发者选项”。
4. 开启“允许安装未签名开发包”。
5. 回到“已安装”/设置页，点击“安装 `.dbxp`”，选择 `dist/` 中的文件。

安装完成后建议关闭该开关。它只影响手动本地安装，不会放宽官方插件商店的签名校验。

如果要分发给其他人、又不想让对方开开发开关，只能：

- 上架官方商店：向 `t8y2/dbx-store` 提交未签名候选包，由 DBX Store 用官方仓库密钥签名；或
- 自建私有插件仓库：用 `dbx-plugin keygen` 生成仓库密钥，签名候选包，并把公钥分发给你仓库的用户，让他们在插件中心的高级信任设置中加入该公钥。直接用自定义密钥签一个裸 `.dbxp` 仍然不会被信任，因为 DBX 没有该公钥。

本地开发（不安装到 DBX，直接在浏览器中调试 UI 与 sidecar）：

```bash
dbx-plugin dev --path . --port 5190
```

## 界面语言

工作台读取 `window.dbxPlugin.locale`，中文（`zh`、`zh-CN`、`zh-TW` 等）显示简体中文，其余语言显示英文；在 DBX 中切换语言时，已打开的笔记标签页会立即刷新，无需重新打开。连接表单的字段名、选项和说明由 `manifest.json > localizations` 提供，同样跟随 DBX 语言。

新增语言时，在 `ui/index.html` 的 `STRINGS` 中加入对应 locale，并在 `manifest.json > localizations` 中补一份字段翻译即可。

## 编辑与 Markdown

普通页面使用编辑与预览融合的 live editor：`ui/markdown.js` 通过 markdown-it 提供 CommonMark + GFM 解析，编辑器在输入时即时把 DOM 序列化回 Markdown 保存到 `vault.json`；加载失败时会回退到内置解析器。

输入以下标记会立即变成对应元素：

| 输入 | 结果 |
| --- | --- |
| `- ` / `* ` / `+ ` | 无序列表 |
| `1. ` | 有序列表 |
| `- ` 后接 `[ ] ` / `[x] ` | 任务列表 |
| `# ` ～ `###### ` | 标题 |
| `> ` | 引用 |
| `**文字**` / `__文字__` | 粗体 |
| `*文字*` / `_文字_` | 斜体 |
| `~~文字~~` | 删除线 |
| `` `文字` `` | 行内代码 |
| `[文字](地址)` / `![文字](地址)` | 链接 / 图片 |
| `---` + 回车 | 分隔线 |
| `` ``` `` + 回车 | 代码块 |
| `| A | B |` 回车后输入 `| --- | --- |` 再回车 | 表格 |

列表项内按 `Tab` / `Shift+Tab` 缩进或反缩进；空列表项按回车退出列表。粘贴 Markdown 会直接渲染成元素。

【首页】是纯源码 + 预览的样式演示，编辑内容不保存。

## 在工作台内修改同步设置

不用每次回插件中心：工作台右上角齿轮打开“同步设置”，可以直接切换：

- 仅本地 / WebDAV / Git；
- WebDAV 地址、用户名、密码；
- Git 仓库、分支、用户名、令牌、提交作者；
- 自动同步间隔、是否上传 Markdown 镜像。

保存后立即生效，配置写在笔记库目录的 `sync-config.json`，权限 0600；WebDAV 密码/Git 令牌只在本机保存，不写入 `vault.json`，也不会随笔记上传。工作台里保存的配置是本机覆盖，优先于连接表单。

> 这是本机覆盖配置，不会自动同步到其他设备；每台设备可以单独设置。

## 创建笔记库连接

1. 打开 DBX 插件中心 → “已安装” → DBX Notes。
2. 新建连接，填写“笔记库名称”。
3. 选择同步方式：
   - **仅本地**：只保存在 `DBX_PLUGIN_DATA_DIR` 下，不同步。
   - **WebDAV**：填写目录 URL、用户名、密码。
   - **Git**：填写仓库地址、分支、用户名、令牌。
4. 保存后从该连接打开工作台。

笔记库配置保存在 DBX 连接中；密码 / 令牌写入 DBX Secret Store，插件前端和 `vault.json` 都看不到它们。

### WebDAV

填写**目录 URL**（不是文件 URL），插件会在其中创建 `vault.json` 和 `DBX-notes/`（Markdown 镜像与 `attachments/` 都在里面）：

| 服务 | 示例 |
| --- | --- |
| Nextcloud | `https://cloud.example.com/remote.php/dav/files/me/DBXNotes` |
| 坚果云 | `https://dav.jianguoyun.com/dav/DBXNotes` |
| Alist | `https://alist.example.com/dav/DBXNotes` |
| 通用 WebDAV | `https://dav.example.com/DBX-notes` |

- 支持 HTTP 和 HTTPS；生产环境建议 HTTPS。
- 使用 Basic Auth；坚果云需使用“应用密码”。
- 插件会自动 `MKCOL` 创建缺失的目录（最多逐级创建）。
- 上传使用 `If-Match` + ETag；远端在同步期间被改动会自动重试（最多 3 次）。

### Git

支持的仓库地址形式：

```text
https://github.com/me/notes.git
https://gitee.com/me/notes.git
git@github.com:me/notes.git
ssh://git@example.com:2222/notes.git
```

- **HTTPS**：填写用户名和 Personal Access Token；插件把 sidecar 自身作为 `GIT_ASKPASS` 助手（通过环境变量读取令牌），不会写入 URL 或提交，也不需要 `/bin/sh`，Windows 同样可用。
- **SSH**：令牌留空，使用本机 ssh-agent / 已配置的密钥。
- 同步流程：`git add/commit` 本地快照 → `fetch origin <branch>` → 合并 `vault.json` → `reset --hard origin/<branch>` → 写回合并结果与镜像 → `commit` → `push`。
- 分支不存在时会自动创建；默认分支 `main`。
- 提交作者可通过连接里的“提交作者 / 提交邮箱”配置。

> 插件不会修改你的全局 Git 配置，所有 `user.name` / `user.email` 都写入笔记库自己的 `.git/config`。

## 同步与冲突

- 本地编辑先落盘，同步只是额外保留远端副本；断网不会丢失内容。
- 每个连接的笔记库位于 `DBX_PLUGIN_DATA_DIR/vaults/<连接 ID>-<哈希>/`。
- 同步状态记录在 `sync-state.json`，包含每个页面的内容版本（revision）。
- 合并策略：按页面 last-write-wins；两侧都改过且内容不同时，保留较新的版本，并把另一侧另存为 `标题 (conflict)` 副本（带 `conflict` 标签）。
- 删除是软删除（`deletedAt` 墓碑），会参与同步；回收站里“永久删除”才会真正移除。
- 首次同步时，两台设备自带的欢迎页使用固定 ID，不会重复；若欢迎页已被修改，则保留较新的一份。
- 自动同步间隔可在连接里设置；`0` 表示关闭。仅在笔记标签页可见且没有未保存修改时触发。

## 数据格式

```text
<vault>/
├── vault.json        # 完整笔记库（标题、正文、层级、标签、墓碑）
├── sync-state.json   # 每页 revision / 镜像文件名 / 上次同步结果
├── DBX-notes/
│   ├── <slug>-<hash>.md   # 可读镜像（WebDAV 与 Git 都会写）
│   └── attachments/       # 图片/文件实体，按附件 ID 命名
├── uploads/          # 分块上传临时目录
└── .git/             # 启用 Git 时才有
```

`vault.json` 是唯一事实来源；`DBX-notes/*.md` 仅用于阅读、备份或外部工具消费，插件不会从镜像反向导入。

## 图片与文件

- 在编辑器中直接粘贴图片，或把文件拖进编辑器；
- 也可以点工具栏的回形针按钮选择文件；
- 图片插入为 Markdown 图片，预览区直接显示；其他文件插入为可点击下载的链接；
- 文件保存在笔记库的 `DBX-notes/attachments/` 目录，正文只写短引用 `![名称](attachment:<id>)`，编辑框不会被长 data URL 撑爆；
- `vault.json` 只保存附件元数据，WebDAV/Git 同步会自动上传/下载 `attachments/` 文件；已配置同步时，插入附件约 2 秒后会自动同步一次；
- 限制：单个附件最大 512 MB，单页正文最大 7 MB；笔记库不再有 32 MB 上限，实际容量取决于磁盘和远端存储。预览只加载 8 MB 以内的图片，更大的文件点击链接下载。

## 快捷键

| 操作 | macOS | Windows / Linux |
| --- | --- | --- |
| 搜索 | `⌘K` | `Ctrl+K` |
| 新建页面 | `⌘N` | `Ctrl+N` |
| 保存 | `⌘S` | `Ctrl+S` |
| 切换预览 | `⌘E` | `Ctrl+E` |

编辑器内：`Tab` 插入缩进，`⌘B` / `Ctrl+B` 加粗，`⌘I` / `Ctrl+I` 斜体。

页面树中拖动页面：拖到行的上半 / 下半部分调整同级顺序，拖到行中间成为该页面的子页面；拖到列表空白处移到顶层末尾。搜索和回收站视图不启用拖拽。

## 下载

工作台运行在沙箱里，浏览器原生的 `<a download>` 会被拦截，所以“下载 .md”优先调用宿主保存对话框（`fileTransfer.beginSave`），在桌面端可直接选择保存位置；不支持该能力的宿主会回退到浏览器下载。

## 限制与安全

- 单页正文最大 7 MB；附件按文件存放在 `attachments/`，不占用 `vault.json` 的文本容量；整个笔记库没有 32 MB 上限，受磁盘/远端容量限制。
- 插件 UI 运行在沙箱中，没有网络、文件系统或 Tauri 访问权限；所有网络与磁盘操作都由 Rust sidecar 完成。
- WebDAV / Git 凭据只存在于 DBX Secret Store 与 sidecar 内存中；同步状态和日志不会记录密码或令牌。
- Git 同步依赖系统 `git`。若在无 `git` 的环境中使用，请选择 WebDAV 或仅本地模式。
- 冲突副本不会自动合并正文；需要时手动对比 `标题` 与 `标题 (conflict)`。

## 目录结构

```text
dbx-notes-plugin/
├── manifest.json              # 插件清单、连接字段、工作台贡献点、中文本地化
├── dbx-plugin.toml            # 打包配置
├── assets/plugin.svg
├── backend/
│   ├── Cargo.toml
│   ├── src/main.rs            # sidecar：笔记库、合并、WebDAV、Git
│   ├── src/merge.rs           # 合并决策表与单测
│   └── vendor/dbx-plugin-sdk/ # 内置 SDK（crates.io 未发布）
├── scripts/package-target.py  # 为交叉编译好的二进制打包
├── .github/workflows/release.yml  # 5 平台原生构建矩阵
├── ui/index.html              # 单文件工作台 UI
├── ui/markdown.js             # markdown-it 适配层
└── ui/vendor/markdown-it.min.js
```

## 发布

1. 推送 tag 并发布 GitHub Release；生成的 workflow 会为各平台构建未签名候选包。
2. 若仓库在 DBX Store 注册了 `autoUpdate: true`，会自动创建候选 PR；否则手动向 `t8y2/dbx-store:main` 提交候选。
3. 审核通过后由 DBX Store 使用官方仓库密钥签名并更新同一 PR。

源码与未签名候选包保留在本仓库；用户从官方目录安装签名后的资产。
