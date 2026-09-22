---
AIGC:
  ContentProducer: '001191110102MAD55U9H0F10002'
  ContentPropagator: '001191110102MAD55U9H0F10002'
  Label: '1'
  ProduceID: 'cf986d26-b3b9-4e8c-a7df-cd4034b78185'
  PropagateID: 'cf986d26-b3b9-4e8c-a7df-cd4034b78185'
  ReservedCode1: '8dbed96d-c3c6-4e3a-8025-5ce5ed15d5fa'
  ReservedCode2: '8dbed96d-c3c6-4e3a-8025-5ce5ed15d5fa'
---

# PushMe Server（Rust 版）- 飞牛 fnOS 应用

PushMe 自建消息推送服务端的 Rust 实现，支持 MQTT/WebSocket 协议，提供可视化管理界面，接口完全兼容官方 PushMe App。

## 功能特性

- 自主可控的消息服务，数据安全有保障
- 消息接口参数与官方完全一致，PushMe App 无缝接入
- Web 管理界面，支持 PushKey 管理、系统日志查看
- HTTP/HTTPS API + MQTT/WebSocket 消息服务
- Web 界面配置端口、证书，无需编辑文件
- 实时日志查看（SSE 实时流）
- 离线消息补发（交易消息场景，后台可配开关）

## 端口说明

| 端口 | 用途 |
|------|------|
| 3010 | Web 管理面板 |
| 3100 | MQTT/WebSocket/TCP 消息服务 |

## 安装方法

1. 将 `pushme-server.fpk` 文件传输到飞牛 fnOS 设备
2. 打开飞牛应用中心
3. 选择"本地安装"
4. 选择该 `.fpk` 文件进行安装
5. 等待安装完成，应用将自动启动

## 首次使用

1. 访问管理界面：`http://飞牛IP:3010`
2. 设置管理员账号和密码
3. 进入 PushKey 管理页面，创建推送密钥
4. 在 PushMe APP 中配置服务器地址和端口

## 客户端配置

在 PushMe APP（安卓/Windows）中：

- **Host**：填写飞牛 NAS 的 IP 地址
- **Port**：填写 `3100`
- 保存配置即可使用

## 消息推送示例

```bash
# 使用 push_key 推送
curl "http://飞牛IP:3100/?push_key=YOUR_KEY&title=测试&content=消息内容"

# 使用 temp_key 推送
curl "http://飞牛IP:3100/?temp_key=YOUR_TEMP_KEY&title=测试&content=消息内容"
```

## 支持的消息类型

- `text` - 纯文本消息
- `markdown` - Markdown 格式消息
- `html` - HTML 格式消息
- `url` - URL 消息
- `data` - 数据消息
- `markdata` - Markdown 数据消息
- `note` - 笔记/任务列表消息
- `svg` - SVG 图片消息
- `chart` - 图表消息
- `echarts` - ECharts 图表消息

## 数据存储位置

应用数据保存在：`/vol1/.apps/pushme-server/var/config`

## 与官方版的区别

本项目为 Rust 实现版本，额外特性：
- 修复公网管理面板暴露安全缺陷
- HMAC 签名 session + 登录限速
- 面板 IP 白名单
- 离线消息补发（官方没有）

## 相关链接

- 项目主页：https://github.com/mumuopen/pushme-server
- PushMe 官网：https://push.i-i.me/
- 飞牛 fnOS：https://fnnas.com/