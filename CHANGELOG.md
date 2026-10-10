---
AIGC:
  ContentProducer: '001191110102MAD55U9H0F10002'
  ContentPropagator: '001191110102MAD55U9H0F10002'
  Label: '1'
  ProduceID: 'b24ebeb0-65fe-4a5a-9d72-3d4e3bdaceb6'
  PropagateID: 'b24ebeb0-65fe-4a5a-9d72-3d4e3bdaceb6'
  ReservedCode1: '5494dbe8-b22e-468b-98f3-013c077933e6'
  ReservedCode2: '5494dbe8-b22e-468b-98f3-013c077933e6'
---

# Changelog

本项目为 PushMe Server 的 **Rust 版本**（基于 Rust 的全新实现，协议对齐官方 Node.js 版，安全修复历史 Go 版缺陷）。

版本格式遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.0.0/)，语义化版本见 [SemVer](https://semver.org/lang/zh-CN/)。

## [0.1.2] - 2026-10-10

### 修复

- **push_key 语义对齐官方（关键修复）**：面板「添加新 Key」此前由服务端随机生成 `PUSHME-xxx`，用户输入只能进备注栏，导致登记的 key 与 PushMe 客户端实际 key 不一致——客户端 MQTT 订阅被 ACL 拒绝、推送消息无法送达。现对齐官方语义：**push_key 由 PushMe 客户端生成，登记时由用户填写（必填）**；temp_key 与备注可选；新增 push_key 非空/去重、temp_key 去重校验；前端表单（push_key/temp_key/备注三输入框）与 README 同步修正。

### 变更

- fpk manifest：维护者账号对齐 `mumuopen` 更名（maintainer/distributor），包版本号同步

## [0.1.1] - 2026-09-22

### 新增

- 面板 favicon 图标（base64 内嵌，无需外部文件）
- 飞牛 fnOS fpk 应用包（docker-compose + 安装向导 + 生命周期脚本）
- README Mermaid 架构图；清理 PNG AIGC 元数据

## [0.1.0] - 2026-09-22

### 首个 Rust 版本

用 Rust 完整重写 PushMe 自建推送服务端，单二进制部署，协议与官方 Node.js 版（yafoo/pushme-server）完全兼容。

#### 新增

- **推送 API**：`push_key` / `temp_key` / 多 key（≤100）/ 参数校验，行为对齐官方
- **第三方 webhook 兼容**：飞书 `msg_type`、企微 / 钉钉 `msgtype` 自动转译为统一消息
- **MQTT 通道**：自研协议前端 + rumqttd 路由内核；keepalive 60/300/600→3600 对齐官方；订阅 / 发布 ACL = push_keys 白名单；非法发布断开连接；QoS1 投递
- **WebSocket 通道**：任意路径升级（子协议 `mqtt`）
- **TLS**：`none / self / public` 三模式；rcgen 自签名证书（多域名 SAN，自动补本机回环）；证书下载端点
- **管理面板**：首次安装（一次锁定）/ 登录 / push_key 管理 / 消息测试 / 实时日志 / 统计 / 设置
- **离线消息补发**（官方没有的增强）：后台可配置开关；无在线订阅者时缓存，重连订阅后按序补发；条数上限可配
- **日志**：历史查询 + SSE 实时流 + 清空
- **自签名证书**：多域名 / IP 生成

#### 安全

- **修复 mumuopen Go 版公网暴露管理面板的缺陷**：公网端口（3100）admin/login/install 一律 404；面板独立监听 `127.0.0.1:3010`
- HMAC-SHA256 签名 session（替代明文用户名 cookie），HttpOnly
- 登录失败 5 次锁 3 分钟（对齐官方）
- 面板 IP 白名单（支持 IPv4 CIDR）
- bcrypt 密码存储；官方 md5 密码首登自动升级

#### 兼容

- `config/data.json` 格式与官方一致，官方旧配置可直接复制使用
- 官方 md5 多层加盐密码可直接迁移登录
- PushMe App（Android / Windows）无缝接入

#### 修复

- rumqttd `Broker::link` 写死 `dynamic_filters=false` 导致动态主题订阅被丢弃 → 改用 `Router` + `LinkBuilder(dynamic_filters=true)` 自建链接
- rustls 多实例共存时显式安装 ring CryptoProvider，避免启动 panic
- 客户端断开（含超时 / 异常）后订阅计数残留 → 连接结束统一清理，保证离线队列判据准确

#### 测试

- 协议级自测脚本：MQTT TCP / TLS / WS / 离线消息 / 官方对齐（`pushme-server-rs/.temp/`）
- 覆盖：推送 API 全参数、ACL、第三方 webhook、TLS 全链路、SSE 日志、未登录 401、面板独立 TLS

#### 已知限制

- 消息不持久化（重启后离线队列清空）
- `clean_session` 恒为 true
- 多实例（水平扩展）不支持

[0.1.2]: https://github.com/mumuopen/pushme-server/releases/tag/v0.1.2
[0.1.1]: https://github.com/mumuopen/pushme-server/releases/tag/v0.1.1
[0.1.0]: https://github.com/mumuopen/pushme-server/releases/tag/v0.1.0