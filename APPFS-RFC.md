# AppFS - AI 统一应用操作接口

**版本**: 1.2-draft
**日期**: 2025-03-09
**状态**: 草案 (Draft)
**基于**: AgentFS 框架

---

## 目录

1. [概述](#1-概述)
2. [动机与目标](#2-动机与目标)
3. [核心概念](#3-核心概念)
4. [功能需求](#4-功能需求)
5. [目录结构规范](#5-目录结构规范)
6. [接口规范](#6-接口规范)
7. [技术架构](#7-技术架构)
8. [桥接层设计](#8-桥接层设计)
9. [非功能需求](#9-非功能需求)
10. [安全考量](#10-安全考量)
11. [实现路线图](#11-实现路线图)
12. [风险与挑战](#12-风险与挑战)
13. [附录](#13-附录)

---

## 1. 概述

### 1.1 项目名称

**AppFS** (Application File System) - 基于 AgentFS 的 AI 统一应用操作接口

### 1.2 一句话描述

将每个应用程序抽象为虚拟文件系统，让 AI 通过标准的文件读写操作来感知和控制任意应用。

### 1.3 设计哲学

借鉴 Unix "一切皆文件" (Everything is a file) 的设计哲学：

- **统一抽象**: 无论操作的是联系人、消息、设置还是界面元素，都是"文件"
- **简单语义**: `read` = 获取状态，`write` = 执行动作
- **可组合性**: 通过文件管道实现跨应用数据流转
- **自描述**: 目录结构即文档，AI 可通过浏览发现功能

---

## 2. 动机与目标

### 2.1 问题陈述

| 当前挑战 | 描述 |
|---------|------|
| API 碎片化 | 每个 App 都有独特的 API，AI 需要针对每个 App 学习 |
| 视觉识别局限 | OCR + CV 方案成本高、准确率有限、无法获取语义信息 |
| 不可组合 | 不同 App 的数据格式不同，难以自动化流转 |
| 文档依赖 | AI 需要阅读大量文档才能理解如何操作一个 App |

### 2.2 目标

#### 主要目标

1. **统一接口**: 定义一套标准的文件系统规范，所有 App 遵循
2. **AI 友好**: AI 通过简单的文件操作即可控制任意 App
3. **渐进式采用**: 支持现有 App（通过桥接）和未来原生 App

#### 非目标

- 不替代现有的 IPC 机制（如 D-Bus、Binder）
- 不提供图形界面渲染
- 不是通用 IPC 框架

### 2.3 成功指标

| 指标 | 目标值 |
|------|--------|
| AI 操作延迟 | < 100ms (本地操作) |
| 学习成本 | AI 阅读 3 个示例后即可操作新 App |
| 覆盖率 | 支持主流 App 的 80% 常用功能 |
| 兼容性 | 支持 Android、iOS、Windows、macOS、Linux |

---

## 3. 核心概念

### 3.1 术语表

| 术语 | 定义 |
|------|------|
| **AppFS** | 本系统，将 App 抽象为文件系统的框架 |
| **App 实例** | 一个正在运行的应用程序 |
| **挂载点** | App 在统一命名空间中的路径，如 `/apps/wechat/` |
| **桥接层** | 将文件操作转换为 App API 调用的组件 |
| **状态文件** | 只读文件，表示 App 的当前状态 |
| **动作文件** | 只写文件，写入即触发动作 |
| **配置文件** | 可读写文件，表示 App 设置 |
| **Schema** | App 文件系统的目录结构定义 |

### 3.2 文件类型语义

```
┌─────────────────────────────────────────────────────────────┐
│                      文件类型与语义                           │
├─────────────┬─────────────┬─────────────────────────────────┤
│ 类型        │ 权限        │ 语义                             │
├─────────────┼─────────────┼─────────────────────────────────┤
│ 状态文件     │ r--         │ 读取 = 获取当前状态              │
│ 动作文件     │ -w-         │ 写入 = 执行动作                  │
│ 配置文件     │ rw-         │ 读取 = 获取配置，写入 = 修改配置   │
│ 流文件       │ rw-         │ 持续读取 = 监听事件流             │
│ 目录        │ r-x         │ 列出 = 获取子项列表               │
└─────────────┴─────────────┴─────────────────────────────────┘
```

### 3.3 命名约定

```
/apps/{app_id}/                    # App 根目录
├── info                           # App 基本信息（只读）
├── schema                         # Schema 定义（只读）
├── {resource}s/                   # 资源集合（复数，如 contacts、messages）
│   └── {id}/                      # 具体资源（如联系人名、聊天对象）
│       ├── info                   # 资源元信息（只读）
│       ├── {action}               # 动作文件（只写，如 send_message、call）
│       ├── {field}                # 状态字段（只读）
│       └── {sub_resource}s/       # 子资源（如 messages/）
├── events/                        # 事件流目录
│   └── {event_type}.stream        # 事件流文件（可 tail -f 持续读取）
└── settings/                      # 全局配置（可读写）
    ├── notifications              # 事件订阅配置（JSON，控制哪些 stream 开启）
    └── {setting_name}             # 其他配置项
```

### 3.4 核心设计原则

#### 目录即上下文

当前路径代表你正在操作的实体：

```bash
cd /apps/wechat/contacts/张三
# 现在你在"张三"这个联系人的上下文中
# ls 看到的就是针对张三可用的操作和数据
```

#### ls 即发现

列出目录内容即可看到所有可用的数据和操作，无需额外文档：

```bash
ls /apps/wechat/contacts/张三
# 输出: info  send_message  call  messages/
# 一目了然：可以查看信息、发消息、打电话、查看聊天记录
```

#### 文件即操作/状态

- **状态文件** (只读): 读取 = 获取信息
- **动作文件** (只写): 写入 = 执行操作（内容即参数）
- **配置文件** (读写): 读取 = 获取配置，写入 = 修改

```bash
# 读取状态
cat /apps/wechat/contacts/张三/info

# 执行动作（写入内容即发送）
echo '你好，在吗？' > /apps/wechat/contacts/张三/send_message

# 修改配置
echo "dark" > /apps/wechat/settings/theme
```

#### 当前位置即当前界面

`pwd` 告诉你现在在 App 的什么位置，目录内容动态反映界面元素。导航即界面切换：

- `cd 子目录` = 进入界面
- `cd ..` = 返回上一级界面
- `cd /apps/wechat` = 回到 App 主界面

---

## 4. 功能需求

### 4.1 FR-01: 应用发现与挂载

**优先级**: P0

**描述**: 系统应能自动发现运行中的应用，并将其挂载到统一命名空间。

**验收标准**:
- [ ] 支持手动指定 App 包名/进程名进行挂载
- [ ] 支持自动扫描并列出可挂载的 App
- [ ] 支持动态挂载/卸载 App
- [ ] 挂载后立即可通过文件系统访问

**API 示例**:
```bash
# 列出可挂载的应用
appfs list-apps

# 挂载应用
appfs mount --app com.tencent.mm /apps/wechat

# 卸载应用
appfs unmount /apps/wechat
```

### 4.2 FR-02: 状态读取

**优先级**: P0

**描述**: 通过读取文件获取 App 的当前状态和数据。`ls` 列出资源，`cat` 读取详情。

**验收标准**:
- [ ] `ls` 目录列出所有资源（如联系人列表）
- [ ] 读取 `info` 文件返回资源详情
- [ ] 读取状态字段返回当前值
- [ ] 支持分页（通过虚拟子目录）

**示例**:
```bash
# 列出所有联系人（ls 即列表）
ls /apps/wechat/contacts/
# 输出: 张三/  李四/  王五/

# 获取联系人详情
cat /apps/wechat/contacts/张三/info
# 输出: {"name": "张三", "id": "wx_123", "avatar": "...", "status": "online"}

# 获取聊天记录
cat /apps/wechat/contacts/张三/messages/today
# 输出: [{"from": "张三", "content": "你好", "time": "10:30"}, ...]

# 分页浏览（通过虚拟目录）
ls /apps/wechat/contacts/page/1/
# 输出: 张三/  李四/  王五/  ... (每页 20 个)
```

### 4.3 FR-03: 动作执行

**优先级**: P0

**描述**: 通过写入动作文件触发操作。动作文件直接放在对应资源目录下，写入内容即执行。

**验收标准**:
- [ ] 写入动作文件立即触发对应动作
- [ ] 动作文件直接位于资源目录下（如 `contacts/张三/send_message`）
- [ ] 写入内容即动作参数（简单字符串或 JSON）
- [ ] 动作执行结果通过写入返回值反馈

**示例**:
```bash
# 发送消息（写入内容即发送）
echo '你好，在吗？' > /apps/wechat/contacts/张三/send_message

# 发起通话（无需参数，写入即触发）
echo '' > /apps/wechat/contacts/张三/call

# 复杂动作（JSON 参数）
echo '{"content": "文件已发送", "attachment": "/path/to/file"}' > /apps/wechat/contacts/张三/send_message

# 删除操作（需要确认）
echo 'confirm' > /apps/wechat/notes/工作计划/delete
```

### 4.4 FR-04: 配置管理

**优先级**: P1

**描述**: 通过读写配置文件查看和修改 App 设置。

**验收标准**:
- [ ] 读取配置文件返回当前值
- [ ] 写入配置文件修改设置
- [ ] 配置变更立即生效（如果 App 支持）
- [ ] 支持配置回滚

**示例**:
```bash
# 查看当前主题
cat /apps/wechat/settings/theme
# 输出: "dark"

# 修改主题
echo "light" > /apps/wechat/settings/theme
```

### 4.5 FR-05: 事件订阅

**优先级**: P1

**描述**: 通过读取 `.stream` 文件订阅 App 状态变化事件。Agent 根据 `settings/notifications` 配置决定监听哪些事件流，有更新时直接加入消息队列。

**验收标准**:
- [ ] `events/` 目录下提供 `{event_type}.stream` 文件
- [ ] 事件以 JSON Lines 格式输出
- [ ] 支持通过 `tail -f` 持续监听
- [ ] `settings/notifications` 配置控制哪些流开启
- [ ] Agent 可根据配置自动订阅对应流

**示例**:
```bash
# 查看可订阅的事件类型
ls /apps/wechat/events/
# 输出: new_message.stream  call_incoming.stream  notification.stream

# 查看当前订阅配置
cat /apps/wechat/settings/notifications
# 输出: {"new_message": true, "call_incoming": true, "notification": false}

# 订阅新消息事件（持续读取）
tail -f /apps/wechat/events/new_message.stream
# 输出:
# {"type": "new_message", "from": "张三", "content": "你好", "timestamp": 1234567890}
# {"type": "new_message", "from": "李四", "content": "在吗", "timestamp": 1234567891}

# 修改订阅配置
echo '{"new_message": true, "call_incoming": false}' > /apps/wechat/settings/notifications
```

### 4.6 FR-06: Schema 自描述

**优先级**: P1

**描述**: 每个挂载的 App 通过 `schema` 文件提供目录结构定义，AI 可通过 `ls` 和 `cat schema` 发现功能。

**验收标准**:
- [ ] `schema` 文件包含完整的目录结构定义
- [ ] Schema 包含每个文件/目录的类型、权限、描述
- [ ] Schema 支持版本化
- [ ] AI 可通过读取 Schema 了解如何操作 App

**Schema 格式**:
```json
{
  "version": "1.0",
  "app_id": "com.tencent.mm",
  "app_name": "WeChat",
  "resources": {
    "contacts": {
      "type": "collection",
      "description": "联系人列表",
      "item_schema": {
        "info": {"type": "state", "description": "联系人信息"},
        "send_message": {"type": "action", "input": "string", "description": "发送消息"},
        "call": {"type": "action", "input": "none", "description": "发起通话"},
        "messages": {"type": "collection", "description": "聊天记录"}
      }
    }
  },
  "settings": {
    "theme": {"type": "string", "options": ["light", "dark"], "description": "主题设置"}
  }
}
```

### 4.7 FR-07: 多平台桥接

**优先级**: P0

**描述**: 支持多种桥接方式以适配不同平台和场景。

**验收标准**:
- [ ] Android: 支持 Accessibility Service 桥接
- [ ] Android: 支持 Frida 动态插桩
- [ ] iOS: 支持 Accessibility/WebDriverAgent
- [ ] Windows: 支持 UI Automation
- [ ] macOS: 支持 Accessibility API
- [ ] 桥接层可热插拔

### 4.8 FR-08: 权限与安全

**优先级**: P0

**描述**: 提供细粒度的权限控制机制。

**验收标准**:
- [ ] 支持配置允许/禁止的操作类型
- [ ] 敏感操作需要确认（可配置）
- [ ] 操作审计日志
- [ ] 支持沙箱隔离

---

## 5. 目录结构规范

### 5.1 标准目录布局

每个 App 挂载后应遵循以下标准结构：

```
/apps/{app_id}/
│
├── info                        # [r--] App 基本信息
├── schema                      # [r--] Schema 定义（自描述）
│
├── {resource}s/                # 资源集合（复数形式）
│   │
│   ├── {id}/                   # 具体资源（如联系人名、聊天对象）
│   │   ├── info               # [r--] 资源元信息
│   │   ├── {action}           # [-w-] 动作文件（写入即执行）
│   │   ├── {field}            # [r--] 状态字段（只读）
│   │   └── {sub_resource}s/   # 子资源目录
│   │
│   └── ...                     # 更多资源
│
├── events/                     # 事件流目录
│   └── {event_type}.stream     # [r--] 事件流文件（可 tail -f 持续读取）
│
└── settings/                   # 全局配置
    ├── notifications           # [rw-] 事件订阅配置（JSON）
    └── {setting_name}          # [rw-] 其他配置项
```

**关键设计决策**:

| 决策 | 说明 |
|------|------|
| 用 `ls` 目录即可看到所有资源 | 无需 `_list` 文件 |
| 用 `info` 文件统一管理元信息 | 无需 `_meta` 文件 |
| 动作文件直接放在资源目录下 | 无需 `actions/` 目录 |
| 当前位置即当前界面 | 无需 `pages/current.page` |
| 事件流通过 `.stream` 文件订阅 | `tail -f` 持续读取 |
| `notifications` 配置控制订阅 | 开关不同类型的事件流 |

### 5.2 特殊文件说明

#### 5.2.1 `info` - App 基本信息

```bash
cat /apps/wechat/info
```

```json
{
  "app_id": "com.tencent.mm",
  "app_name": "WeChat",
  "version": "8.0.33",
  "package": "com.tencent.mm",
  "platform": "android",
  "bridge_type": "accessibility",
  "mounted_at": "2025-03-09T10:30:00Z",
  "capabilities": ["read_state", "execute_action", "subscribe_events"]
}
```

#### 5.2.2 `schema` - Schema 定义

每个资源目录下的 `info` 文件已包含元信息，根目录的 `schema` 提供全局视图：

```bash
cat /apps/wechat/schema
```

```json
{
  "version": "1.0",
  "app_id": "com.tencent.mm",
  "app_name": "WeChat",
  "resources": {
    "contacts": {
      "description": "联系人列表",
      "item_schema": {
        "info": {"type": "state", "description": "联系人信息"},
        "send_message": {"type": "action", "input": "string", "description": "发送消息"},
        "call": {"type": "action", "input": "none", "description": "发起通话"},
        "messages": {"type": "collection", "description": "聊天记录"}
      }
    }
  },
  "settings": {
    "theme": {"type": "string", "options": ["light", "dark"]},
    "notifications": {"type": "boolean"}
  }
}
```

#### 5.2.3 `events/{event_type}.stream` - 事件流

事件流文件用于实时监听 App 状态变化，通过 `tail -f` 持续读取：

```bash
# 列出可用的事件类型
ls /apps/wechat/events/
# 输出: new_message.stream  call_incoming.stream  notification.stream

# 订阅新消息事件
tail -f /apps/wechat/events/new_message.stream
# 输出:
# {"type": "new_message", "from": "张三", "content": "你好", "timestamp": 1234567890}
# {"type": "new_message", "from": "李四", "content": "在吗", "timestamp": 1234567891}
```

#### 5.2.4 `settings/notifications` - 事件订阅配置

通过此配置文件控制哪些事件流是开启的：

```bash
# 查看当前订阅配置
cat /apps/wechat/settings/notifications
```

```json
{
  "new_message": true,
  "call_incoming": true,
  "notification": false,
  "friend_request": false
}
```

```bash
# 开启新消息订阅
echo '{"new_message": true, "call_incoming": true}' > /apps/wechat/settings/notifications

# 关闭所有通知
echo '{}' > /apps/wechat/settings/notifications
```

**AI Agent 使用场景**:

Agent 实现时，根据 `settings/notifications` 配置，决定读取哪些 `.stream` 文件：

```python
# 伪代码
notifications = json.read("/apps/wechat/settings/notifications")
for event_type, enabled in notifications.items():
    if enabled:
        stream = open(f"/apps/wechat/events/{event_type}.stream")
        # 将 stream 加入消息队列监听
        message_queue.add_stream(stream)
```

### 5.3 资源类型示例

#### 联系人 (contacts)

```
/contacts/
├── 张三/
│   ├── info                # {"name": "张三", "id": "...", "avatar": "..."}
│   ├── send_message        # 写入消息内容即发送
│   ├── call                # 写入任意内容即发起通话
│   └── messages/
│       ├── today           # 今天的聊天记录
│       └── yesterday       # 昨天的聊天记录
├── 李四/
│   ├── info
│   ├── send_message
│   ├── call
│   └── messages/
└── ...                     # ls 直接列出所有联系人
```

**操作示例**:

```bash
# 列出所有联系人
ls /apps/wechat/contacts/
# 输出: 张三/  李四/  王五/

# 查看联系人信息
cat /apps/wechat/contacts/张三/info
# 输出: {"name": "张三", "id": "wx_123", "avatar": "..."}

# 发送消息（直接写入内容）
echo '你好，在吗？' > /apps/wechat/contacts/张三/send_message

# 查看聊天记录
cat /apps/wechat/contacts/张三/messages/today
```

#### 聊天 (chats) - 另一种组织方式

```
/chats/
├── 张三/
│   ├── info                # 聊天信息
│   ├── send                # 发送消息
│   ├── messages/           # 消息历史
│   │   ├── latest          # 最近一条消息
│   │   └── today           # 今天的消息
│   └── attachments/        # 附件
└── 群聊-工作群/
    ├── info
    ├── send
    └── members/            # 群成员
```

#### 笔记 (notes) - 如 Notion/备忘录

```
/notes/
├── 工作计划/
│   ├── info                # {"title": "工作计划", "created": "...", "tags": [...]}
│   ├── content             # 笔记内容（只读或可编辑）
│   ├── edit                # 写入新内容即编辑
│   └── delete              # 写入 "confirm" 即删除
└── 会议记录/
    ├── info
    ├── content
    ├── edit
    └── delete
```

---

## 6. 接口规范

### 6.1 文件读写 API

#### 6.1.1 列出资源

```bash
# ls 目录即列出所有资源
ls /apps/wechat/contacts/
# 输出: 张三/  李四/  王五/

# 分页（通过虚拟目录）
ls /apps/wechat/contacts/page/1/
# 输出: 张三/  李四/  ... (每页固定数量)
```

#### 6.1.2 读取状态

```bash
# 读取资源信息
cat /apps/wechat/contacts/张三/info
# 输出: {"name": "张三", "id": "wx_123", "avatar": "..."}

# 读取子资源
cat /apps/wechat/contacts/张三/messages/today
# 输出: [{"from": "张三", "content": "你好", "time": "10:30"}, ...]

# 读取配置
cat /apps/wechat/settings/theme
# 输出: "dark"
```

#### 6.1.3 执行动作

```bash
# 简单动作（写入字符串）
echo '你好，在吗？' > /apps/wechat/contacts/张三/send_message

# 无参数动作（写入空内容）
echo '' > /apps/wechat/contacts/张三/call

# 复杂动作（JSON 参数）
echo '{"content": "文件", "attachment": "/path/file.pdf"}' > /apps/wechat/contacts/张三/send_message

# 确认性动作
echo 'confirm' > /apps/wechat/notes/工作计划/delete
```

#### 6.1.4 修改配置

```bash
# 读取配置
cat /apps/wechat/settings/theme
# 输出: "dark"

# 修改配置
echo "light" > /apps/wechat/settings/theme
```

#### 6.1.5 事件订阅

```bash
# 列出可用事件类型
ls /apps/wechat/events/
# 输出: new_message.stream  call_incoming.stream  notification.stream

# 查看当前订阅配置
cat /apps/wechat/settings/notifications
# 输出: {"new_message": true, "call_incoming": true, "notification": false}

# 修改订阅配置
echo '{"new_message": true, "call_incoming": false}' > /apps/wechat/settings/notifications

# 订阅事件流（持续读取，有新事件时自动推送）
tail -f /apps/wechat/events/new_message.stream
# 输出:
# {"type": "new_message", "from": "张三", "content": "你好", "timestamp": 1234567890}
# {"type": "new_message", "from": "李四", "content": "在吗", "timestamp": 1234567891}
```

**Agent 实现模式**:

Agent 启动时读取 `settings/notifications`，根据配置订阅对应的事件流：

```python
# 伪代码
def setup_event_listeners(appfs):
    notifications = appfs.read("settings/notifications")
    for event_type, enabled in notifications.items():
        if enabled:
            stream = appfs.watch(f"events/{event_type}.stream")
            message_queue.add(stream)  # 加入消息队列
```

### 6.2 SDK API

#### Rust SDK

```rust
use appfs::{AppFS, ActionResult};

// 打开 AppFS
let appfs = AppFS::open("/path/to/config").await?;

// 挂载应用
let wechat = appfs.mount_app("com.tencent.mm").await?;

// 列出资源
let contacts = wechat.list("contacts").await?;
// contacts: Vec<String> = ["张三", "李四", "王五"]

// 读取状态
let info = wechat.read("contacts/张三/info").await?;
let info: ContactInfo = serde_json::from_str(&info)?;

// 执行动作
wechat.write("contacts/张三/send_message", "你好，在吗？").await?;

// 修改配置
wechat.write("settings/theme", "light").await?;

// 获取 Schema
let schema = wechat.schema().await?;
```

#### TypeScript SDK

```typescript
import { AppFS } from 'appfs-sdk';

const appfs = await AppFS.open();

// 挂载应用
const wechat = await appfs.mountApp('com.tencent.mm');

// 列出资源
const contacts = await wechat.list('contacts');
// contacts: string[] = ['张三', '李四', '王五']

// 读取状态
const info = await wechat.read('contacts/张三/info');
const contactInfo = JSON.parse(info);

// 执行动作
await wechat.write('contacts/张三/send_message', '你好，在吗？');

// 修改配置
await wechat.write('settings/theme', 'light');

// 获取 Schema
const schema = await wechat.schema();
```

### 6.3 命令行接口

```bash
# 应用管理
appfs apps list                    # 列出可挂载应用
appfs apps mount <app_id> [name]   # 挂载应用
appfs apps unmount <name>          # 卸载应用
appfs apps status [name]           # 查看状态

# 文件系统操作
appfs ls /apps/wechat/contacts/    # 列出联系人
appfs cat /apps/wechat/contacts/张三/info    # 读取信息
appfs write /apps/wechat/contacts/张三/send_message '你好'  # 发送消息

# 导航操作
appfs cd /apps/wechat/contacts/张三  # 进入上下文
appfs ls                            # 列出当前上下文的操作和数据

# Schema 操作
appfs cat /apps/wechat/schema       # 显示 Schema

# 桥接管理
appfs bridges list                  # 列出可用桥接
appfs bridges status                # 桥接状态
appfs bridges reload <name>         # 重载桥接
```

---

## 7. 技术架构

### 7.1 系统架构图

```
┌────────────────────────────────────────────────────────────────────┐
│                           AI Agent Layer                           │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐                │
│  │   Claude    │  │   GPT-4     │  │   Local     │                │
│  │   Agent     │  │   Agent     │  │   LLM       │                │
│  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘                │
└─────────┼────────────────┼────────────────┼────────────────────────┘
          │                │                │
          ▼                ▼                ▼
┌────────────────────────────────────────────────────────────────────┐
│                        AppFS Core Layer                            │
│  ┌──────────────────────────────────────────────────────────────┐  │
│  │                    Unified Namespace                          │  │
│  │         /apps/wechat/    /apps/chrome/    /apps/...          │  │
│  └──────────────────────────────────────────────────────────────┘  │
│  ┌────────────┐  ┌────────────┐  ┌────────────┐  ┌────────────┐   │
│  │  Router    │  │  Schema    │  │  Event     │  │  Audit     │   │
│  │  Manager   │  │  Registry  │  │  Bus       │  │  Logger    │   │
│  └────────────┘  └────────────┘  └────────────┘  └────────────┘   │
└─────────────────────────────┬──────────────────────────────────────┘
                              │
                              ▼
┌────────────────────────────────────────────────────────────────────┐
│                        Bridge Layer                                │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐                │
│  │ Accessibility│  │   Frida     │  │   Native    │                │
│  │   Bridge    │  │   Bridge    │  │   Bridge    │                │
│  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘                │
└─────────┼────────────────┼────────────────┼────────────────────────┘
          │                │                │
          ▼                ▼                ▼
┌────────────────────────────────────────────────────────────────────┐
│                        Platform Layer                              │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐                │
│  │  Android    │  │    iOS      │  │  Desktop    │                │
│  │ Accessibility│  │  XCUITest   │  │ UI Auto.    │                │
│  │   / Frida   │  │  / WebDriver│  │  / PyAuto   │                │
│  └─────────────┘  └─────────────┘  └─────────────┘                │
└────────────────────────────────────────────────────────────────────┘
          │                │                │
          ▼                ▼                ▼
┌────────────────────────────────────────────────────────────────────┐
│                      Target Applications                           │
│     WeChat        Chrome        Telegram        VS Code            │
└────────────────────────────────────────────────────────────────────┘
```

### 7.2 核心组件

#### 7.2.1 AppFSCore

主协调器，管理整体生命周期。

```rust
pub struct AppFSCore {
    config: AppConfig,
    namespace: Arc<Namespace>,
    bridge_manager: Arc<BridgeManager>,
    schema_registry: Arc<SchemaRegistry>,
    event_bus: Arc<EventBus>,
    audit_logger: Arc<AuditLogger>,
}
```

#### 7.2.2 Namespace

统一命名空间管理。

```rust
pub struct Namespace {
    mounts: RwLock<HashMap<String, MountPoint>>,
    router: PathRouter,
}

pub struct MountPoint {
    app_id: String,
    bridge: Arc<dyn AppBridge>,
    schema: Arc<AppSchema>,
    state: MountState,
}
```

#### 7.2.3 BridgeManager

桥接层管理器。

```rust
pub struct BridgeManager {
    bridges: RwLock<HashMap<String, Box<dyn BridgeFactory>>>,
    active_bridges: RwLock<HashMap<String, Arc<dyn AppBridge>>>,
}

pub trait BridgeFactory: Send + Sync {
    fn bridge_type(&self) -> &str;
    fn create(&self, config: BridgeConfig) -> Result<Arc<dyn AppBridge>>;
    fn discover_apps(&self) -> Result<Vec<AppInfo>>;
}
```

#### 7.2.4 EventBus

事件分发中心。

```rust
pub struct EventBus {
    subscribers: RwLock<HashMap<String, Vec<mpsc::Sender<Event>>>>,
    buffer_size: usize,
}

impl EventBus {
    pub async fn publish(&self, event: Event) -> Result<()>;
    pub async fn subscribe(&self, event_type: &str) -> Result<mpsc::Receiver<Event>>;
}
```

### 7.3 数据流

#### 7.3.1 列出资源流程

```
AI Agent                    AppFS Core                 Bridge                 App
   │                            │                        │                     │
   │  ls /apps/wechat/contacts/ │                        │                     │
   │ ─────────────────────────▶ │                        │                     │
   │                            │  list_resources()      │                     │
   │                            │ ─────────────────────▶ │                     │
   │                            │                        │  query_contacts()   │
   │                            │                        │ ──────────────────▶ │
   │                            │                        │                     │
   │                            │                        │  ◀───────────────── │
   │                            │                        │  [contact names]    │
   │                            │  ◀──────────────────── │                     │
   │                            │  [目录列表]             │                     │
   │  ◀─────────────────────────│                        │                     │
   │  张三/  李四/  王五/        │                        │                     │
```

#### 7.3.2 执行动作流程

```
AI Agent                    AppFS Core                 Bridge                 App
   │                            │                        │                     │
   │  echo '你好' >             │                        │                     │
   │      contacts/张三/        │                        │                     │
   │      send_message          │                        │                     │
   │ ─────────────────────────▶ │                        │                     │
   │                            │  resolve_path()        │                     │
   │                            │  → contacts/张三/       │                     │
   │                            │  → action: send_message │                    │
   │                            │ ──────────────────────▶│                     │
   │                            │                        │  send_message()     │
   │                            │                        │ ──────────────────▶ │
   │                            │                        │                     │
   │                            │                        │  ◀───────────────── │
   │                            │                        │  [success]          │
   │                            │  ◀─────────────────────│                     │
   │                            │  [ActionResult]        │                     │
   │  ◀─────────────────────────│                        │                     │
   │  [写入成功]                 │                        │                     │
```

---

## 8. 桥接层设计

### 8.1 桥接抽象

```rust
/// 应用桥接 trait - 所有平台实现此接口
#[async_trait]
pub trait AppBridge: Send + Sync {
    /// 获取桥接信息
    fn info(&self) -> BridgeInfo;

    /// 列出资源（返回资源 ID 列表，用于目录列表）
    async fn list_resources(&self, resource_type: &str) -> Result<Vec<String>>;

    /// 读取资源信息/状态
    async fn read(&self, path: &str) -> Result<Vec<u8>>;

    /// 执行动作（写入动作文件）
    async fn execute_action(&self, path: &str, content: &[u8]) -> Result<ActionResult>;

    /// 获取/设置配置
    async fn get_setting(&self, key: &str) -> Result<Option<serde_json::Value>>;
    async fn set_setting(&self, key: &str, value: serde_json::Value) -> Result<()>;

    /// 健康检查
    async fn health_check(&self) -> Result<HealthStatus>;
}

/// 桥接信息
pub struct BridgeInfo {
    pub bridge_type: String,
    pub platform: Platform,
    pub version: String,
    pub capabilities: Vec<Capability>,
}

/// 支持的能力
pub enum Capability {
    ReadState,
    ExecuteAction,
    ModifySettings,
    ListResources,
}
```

**设计说明**:

- 移除了 `get_current_page()` - 当前界面由路径表示
- `list_resources()` 返回资源 ID 列表，对应目录列表
- `read()` 统一处理所有读取操作（info、状态字段等）
- `execute_action()` 处理动作文件的写入

### 8.2 Android Accessibility Bridge

```rust
pub struct AndroidAccessibilityBridge {
    device: AndroidDevice,
    accessibility_service: AccessibilityServiceClient,
    app_package: String,
}

#[async_trait]
impl AppBridge for AndroidAccessibilityBridge {
    async fn list_resources(&self, resource_type: &str) -> Result<Vec<String>> {
        match resource_type {
            "contacts" => {
                // 通过 Accessibility API 获取联系人列表
                let nodes = self.accessibility_service
                    .find_nodes_by_class("contact_item").await?;
                Ok(nodes.iter().map(|n| n.text.clone()).collect())
            }
            _ => Err(BridgeError::UnknownResourceType(resource_type.into()))
        }
    }

    async fn read(&self, path: &str) -> Result<Vec<u8>> {
        // 解析路径: contacts/张三/info
        let parts: Vec<&str> = path.split('/').collect();
        match (parts[0], parts.get(2).map(|s| *s)) {
            ("contacts", Some("info")) => {
                let contact_name = parts[1];
                // 获取联系人详细信息
                let info = self.get_contact_info(contact_name).await?;
                Ok(serde_json::to_vec(&info)?)
            }
            ("contacts", Some("messages")) => {
                // 获取聊天记录
                let contact_name = parts[1];
                let time_filter = parts.get(3).copied(); // today, yesterday
                let messages = self.get_messages(contact_name, time_filter).await?;
                Ok(serde_json::to_vec(&messages)?)
            }
            _ => Err(BridgeError::NotFound(path.into()))
        }
    }

    async fn execute_action(&self, path: &str, content: &[u8]) -> Result<ActionResult> {
        // 解析路径: contacts/张三/send_message
        let parts: Vec<&str> = path.split('/').collect();
        let contact_name = parts[1];
        let action = parts[2];

        match action {
            "send_message" => {
                let message = std::str::from_utf8(content)?;
                // 1. 打开聊天窗口
                self.open_chat(contact_name).await?;
                // 2. 输入消息
                self.accessibility_service.input_text(message).await?;
                // 3. 点击发送
                self.click_send_button().await?;
                Ok(ActionResult::success())
            }
            "call" => {
                self.initiate_call(contact_name).await?;
                Ok(ActionResult::success())
            }
            _ => Err(BridgeError::UnknownAction(action.into()))
        }
    }
}
```

### 8.3 Frida Bridge

```rust
pub struct FridaBridge {
    session: FridaSession,
    script: FridaScript,
    app_package: String,
}

impl FridaBridge {
    /// 注入 JS 脚本到目标 App
    async fn inject_script(&self) -> Result<()> {
        let script_code = include_str!("../scripts/appfs_agent.js");
        self.script = self.session.create_script(script_code).await?;
        self.script.load().await?;
        Ok(())
    }
}

#[async_trait]
impl AppBridge for FridaBridge {
    async fn list_resources(&self, resource_type: &str) -> Result<Vec<String>> {
        let result = self.script.call("listResources", json!({
            "type": resource_type
        })).await?;

        let resources: Vec<String> = serde_json::from_value(result)?;
        Ok(resources)
    }

    async fn read(&self, path: &str) -> Result<Vec<u8>> {
        let result = self.script.call("read", json!({
            "path": path
        })).await?;

        Ok(result.as_str().map(|s| s.as_bytes()).unwrap_or_default().to_vec())
    }

    async fn execute_action(&self, path: &str, content: &[u8]) -> Result<ActionResult> {
        let result = self.script.call("executeAction", json!({
            "path": path,
            "content": String::from_utf8_lossy(content)
        })).await?;

        let action_result: ActionResult = serde_json::from_value(result)?;
        Ok(action_result)
    }
}
```

### 8.4 Native Bridge (未来)

对于原生支持 AppFS 的应用：

```rust
pub struct NativeBridge {
    connection: NativeAppConnection,
}

#[async_trait]
impl AppBridge for NativeBridge {
    // 直接通过 IPC 与应用通信
    // 应用内部实现 AppFS 协议
}
```

---

## 9. 非功能需求

### 9.1 性能要求

| 指标 | 要求 | 测量方法 |
|------|------|---------|
| 状态读取延迟 | < 100ms (P95) | 从 read 调用到返回数据 |
| 动作执行延迟 | < 200ms (P95) | 从 write 调用到动作完成 |
| 事件推送延迟 | < 50ms | 事件发生到推送至订阅者 |
| 内存占用 | < 100MB | 核心进程 |
| CPU 占用 | < 5% 空闲时 | 核心进程 |

### 9.2 可靠性要求

- **可用性**: 99.9% (单机)
- **故障恢复**: 桥接断开后自动重连，重试间隔指数退避
- **数据一致性**: 动作执行保证至少一次语义

### 9.3 可扩展性

- **并发连接**: 支持 100+ 并发文件操作
- **挂载数量**: 同时支持 10+ App 挂载
- **事件订阅**: 每个事件类型支持 100+ 订阅者

### 9.4 兼容性

| 平台 | 最低版本 | 桥接方式 |
|------|---------|---------|
| Android | 8.0 (API 26) | Accessibility, Frida |
| iOS | 14.0 | XCUITest, WebDriverAgent |
| Windows | 10 | UI Automation |
| macOS | 11.0 | Accessibility API |
| Linux | (TBD) | AT-SPI |

---

## 10. 安全考量

### 10.1 威胁模型

| 威胁 | 描述 | 缓解措施 |
|------|------|---------|
| 未授权访问 | 恶意 AI 访问敏感 App | 权限控制 + 白名单 |
| 数据泄露 | 敏感数据通过文件系统泄露 | 数据脱敏 + 审计 |
| 恶意操作 | AI 执行破坏性操作 | 操作确认 + 回滚机制 |
| 桥接劫持 | 恶意代码注入桥接层 | 代码签名 + 沙箱 |
| 资源耗尽 | AI 发起大量操作 | 速率限制 + 配额 |

### 10.2 安全机制

#### 10.2.1 权限模型

```rust
pub struct Permission {
    pub app_id: String,
    pub resource_type: Option<String>,
    pub action: PermissionAction,
    pub constraints: Vec<Constraint>,
}

pub enum PermissionAction {
    Read,
    Write,
    Execute,
    Subscribe,
}

pub struct PermissionManager {
    permissions: Vec<Permission>,
    policy: PermissionPolicy,
}

impl PermissionManager {
    pub fn check(&self, request: &AccessRequest) -> Result<()>;
    pub fn request_elevation(&self, request: &AccessRequest) -> Result<ElevationHandle>;
}
```

#### 10.2.2 审计日志

```rust
pub struct AuditEntry {
    pub timestamp: DateTime<Utc>,
    pub agent_id: String,
    pub action: String,
    pub path: String,
    pub params: Option<Value>,
    pub result: ActionResult,
    pub duration_ms: u64,
}

// 所有操作记录到 AgentFS 的 tool_calls 表
```

#### 10.2.3 敏感数据处理

```rust
pub trait SensitiveDataHandler {
    /// 脱敏处理
    fn redact(&self, data: &mut Value, patterns: &[SensitivePattern]);

    /// 是否需要用户确认
    fn requires_confirmation(&self, action: &str, params: &Value) -> bool;
}
```

---

## 11. 实现路线图

### Phase 1: 核心框架 (4 周)

**目标**: 建立可扩展的 AppFS 框架

| 周 | 任务 | 交付物 |
|----|------|--------|
| 1 | 设计并实现 `AppBridge` trait | `sdk/rust/src/bridge/mod.rs` |
| 1 | 实现 `AppFSFilesystem` | `sdk/rust/src/filesystem/appfs.rs` |
| 2 | 实现命名空间和路由 | `sdk/rust/src/namespace.rs` |
| 2 | 实现事件总线 | `sdk/rust/src/events.rs` |
| 3 | 实现 Schema 注册表 | `sdk/rust/src/schema/registry.rs` |
| 3 | 实现权限管理 | `sdk/rust/src/security/permissions.rs` |
| 4 | CLI 命令扩展 | `cli/src/cmd/apps.rs` |
| 4 | 单元测试 | `tests/` |

### Phase 2: Android Accessibility 桥接 (3 周)

**目标**: 实现第一个可用的平台桥接

| 周 | 任务 | 交付物 |
|----|------|--------|
| 5 | Android ADB 集成 | `sdk/rust/src/bridge/android/mod.rs` |
| 5 | Accessibility Service 客户端 | `sdk/rust/src/bridge/android/accessibility.rs` |
| 6 | 节点树解析和转换 | `sdk/rust/src/bridge/android/parser.rs` |
| 6 | 动作执行实现 | 动作: click, scroll, input |
| 7 | 微信 Schema 定义 | `schemas/wechat.json` |
| 7 | 微信桥接测试 | `tests/android/wechat_test.rs` |

### Phase 3: Frida 桥接 (3 周)

**目标**: 提供更深层次的应用控制

| 周 | 任务 | 交付物 |
|----|------|--------|
| 8 | Frida Rust 绑定 | `sdk/rust/src/bridge/frama/mod.rs` |
| 8 | 注入脚本开发 | `scripts/appfs_agent.js` |
| 9 | Hook 框架 | `scripts/hooks/` |
| 9 | 数据提取接口 | `scripts/extractors/` |
| 10 | 微信 Frida 桥接 | `sdk/rust/src/bridge/frama/wechat.rs` |
| 10 | 性能优化 | - |

### Phase 4: 多 App Schema (2 周)

**目标**: 扩展支持更多应用

| 周 | 任务 | 交付物 |
|----|------|--------|
| 11 | Chrome Schema | `schemas/chrome.json` |
| 11 | Telegram Schema | `schemas/telegram.json` |
| 12 | Schema 验证工具 | `cli/src/cmd/schema.rs` |
| 12 | Schema 文档生成 | 自动生成 Schema 文档 |

### Phase 5: SDK 完善 (2 周)

**目标**: 提供完整的开发者体验

| 周 | 任务 | 交付物 |
|----|------|--------|
| 13 | TypeScript SDK | `sdk/typescript/src/appfs.ts` |
| 13 | Python SDK | `sdk/python/appfs/` |
| 14 | 文档 | API 文档 + 使用指南 |
| 14 | 示例代码 | `examples/` |

### Phase 6: 稳定化 (2 周)

**目标**: 生产就绪

| 周 | 任务 | 交付物 |
|----|------|--------|
| 15 | 性能测试 | 基准测试报告 |
| 15 | 安全审计 | 安全评估报告 |
| 16 | Bug 修复 | - |
| 16 | 发布 | AppFS 1.0 |

---

## 12. 风险与挑战

### 12.1 技术风险

| 风险 | 可能性 | 影响 | 缓解措施 |
|------|--------|------|---------|
| Accessibility API 限制 | 高 | 中 | 多桥接方式并存 (Frida + Accessibility) |
| App 更新导致 Schema 失效 | 中 | 中 | Schema 版本化 + 自动检测 |
| 性能不达标 | 中 | 高 | 缓存策略 + 异步优化 |
| 跨平台兼容性 | 中 | 中 | 抽象层 + 平台特定实现 |

### 12.2 安全风险

| 风险 | 可能性 | 影响 | 缓解措施 |
|------|--------|------|---------|
| 敏感数据泄露 | 高 | 高 | 数据脱敏 + 权限控制 |
| 恶意 AI 滥用 | 中 | 高 | 审计日志 + 操作确认 |
| 桥接层漏洞 | 中 | 高 | 代码审计 + 沙箱隔离 |

### 12.3 生态风险

| 风险 | 可能性 | 影响 | 缓解措施 |
|------|--------|------|---------|
| App 厂商反对 | 中 | 高 | 开源 + 透明 + 安全设计 |
| 社区采用缓慢 | 中 | 中 | 好的文档 + 示例 + 演示 |
| 标准碎片化 | 低 | 高 | 积极参与标准制定 |

---

## 13. 附录

### 13.1 术语对照表

| 中文 | 英文 | 说明 |
|------|------|------|
| 桥接层 | Bridge Layer | 连接文件系统和实际应用的组件 |
| 挂载点 | Mount Point | App 在命名空间中的路径 |
| 状态文件 | State File | 只读，表示当前状态 |
| 动作文件 | Action File | 只写，写入触发动作 |
| 流文件 | Stream File | 可持续读取的事件流 |

### 13.2 参考资料

- [AgentFS 设计文档](./SPEC.md)
- [Android Accessibility Service](https://developer.android.com/guide/topics/ui/accessibility/service)
- [Frida Documentation](https://frida.re/docs/)
- [FUSE (Filesystem in Userspace)](https://www.kernel.org/doc/html/latest/filesystems/fuse.html)
- [Unix Philosophy: Everything is a File](https://en.wikipedia.org/wiki/Everything_is_a_file)

### 13.3 变更历史

| 版本 | 日期 | 作者 | 变更说明 |
|------|------|------|---------|
| 1.0-draft | 2025-03-09 | Claude | 初始草案 |
| 1.1-draft | 2025-03-09 | Claude | 简化设计：去除下划线前缀、删除 `_list`/`_meta`/`pages/`/`actions/`、动作直接放在资源目录下 |
| 1.2-draft | 2025-03-09 | Claude | 恢复 `events/` 目录和 `settings/notifications`，支持 Agent 事件订阅 |

---

**文档状态**: 草案 (Draft) - 待审核和讨论

**核心设计变更 (v1.2)**:

| 变更 | 原因 |
|------|------|
| `_info` → `info` | 去除下划线，降低 token 消耗 |
| 删除 `_list` 文件 | `ls` 目录即可看到资源列表 |
| 删除 `_meta` 文件 | 用 `info` 统一管理元信息 |
| 删除 `pages/current.page` | 当前位置即当前界面，由路径表示 |
| 删除 `actions/` 目录 | 动作文件直接放在资源目录下 |
| 恢复 `events/` 目录 | Agent 需要实时监听 App 事件 |
| 新增 `settings/notifications` | 控制哪些事件流开启，Agent 根据此配置订阅 |

**下一步行动**:
1. 审核需求文档
2. 确定优先级
3. 开始 Phase 1 实现