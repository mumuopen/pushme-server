//! 第三方平台消息转译：飞书 / 企业微信 / 钉钉群机器人 webhook 格式 → PushMe 标准消息
//! 逻辑对齐官方 pushme-server (Node.js app/libs/third.js)

use serde_json::Value;

/// 统一的请求参数容器：query string 优先，其次 body（json / urlencoded form）
#[derive(Debug, Default, Clone)]
pub struct Params {
    pub map: std::collections::HashMap<String, Value>,
}

impl Params {
    /// 取字符串参数（query 优先；body 中的字符串/数字也接受）
    pub fn get_str(&self, key: &str) -> Option<String> {
        match self.map.get(key) {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Number(n)) => Some(n.to_string()),
            Some(Value::Bool(b)) => Some(b.to_string()),
            _ => None,
        }
    }

    /// 取原始 JSON 值（对象/数组保留结构，供第三方转译用）
    pub fn get_val(&self, key: &str) -> Option<&Value> {
        self.map.get(key)
    }
}

/// 简易百分号解码（+ 视为空格）
pub fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = &s[i + 1..i + 3];
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 解析 query string / urlencoded body 到 map
pub fn parse_urlencoded(s: &str, map: &mut std::collections::HashMap<String, Value>) {
    for pair in s.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.find('=') {
            Some(pos) => (&pair[..pos], &pair[pos + 1..]),
            None => (pair, ""),
        };
        let k = url_decode(k);
        if k.is_empty() {
            continue;
        }
        map.entry(k).or_insert_with(|| Value::String(url_decode(v)));
    }
}

/// 第三方转译结果
#[derive(Debug, Default, Clone)]
pub struct ThirdData {
    pub title: String,
    pub content: String,
    /// "" | "markdown" | "text"
    pub kind: String,
    /// 是否命中第三方格式
    pub detected: bool,
}

/// 入口：自动识别飞书(msg_type) / 企微、钉钉(msgtype)
pub fn detect(params: &Params) -> ThirdData {
    if let Some(msg_type) = params.get_str("msg_type") {
        // 飞书
        let mut data = ThirdData {
            detected: true,
            ..Default::default()
        };
        feishu(&msg_type, params, &mut data);
        return data;
    }
    if let Some(msgtype) = params.get_str("msgtype") {
        // 企微/钉钉
        let mut data = ThirdData {
            detected: true,
            kind: "markdown".to_string(),
            ..Default::default()
        };
        weiding(&msgtype, params, &mut data);
        return data;
    }
    ThirdData::default()
}

// ------------------------- 飞书 -------------------------

fn feishu(msg_type: &str, params: &Params, out: &mut ThirdData) {
    let empty = Value::Null;
    let content = params.get_val("content").unwrap_or(&empty);
    let obj = content.as_object();

    let s = |v: Option<&Value>| -> String {
        match v {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        }
    };

    match msg_type {
        // 文本
        "text" => {
            out.content = obj.and_then(|o| o.get("text")).map(|v| s(Some(v))).unwrap_or_default();
        }
        // 富文本
        "post" => {
            let post_root = obj
                .and_then(|o| o.get("post"))
                .and_then(|p| p.get("zh_cn").or_else(|| p.get("en_us")));
            if let Some(post) = post_root {
                out.title = post
                    .get("title")
                    .map(|v| s(Some(v)))
                    .unwrap_or_default();
                if let Some(Value::Array(items)) = post.get("content") {
                    // 官方按一维遍历；这里同时兼容飞书标准的二维结构
                    for item in items {
                        match item {
                            Value::Array(inner) => {
                                for it in inner {
                                    feishu_post_element(it, out);
                                }
                            }
                            other => feishu_post_element(other, out),
                        }
                    }
                }
                out.kind = "markdown".to_string();
            }
        }
        // 群名片
        "share_chat" => {
            out.content = format!(
                "群名片，share_chat_id：{}",
                obj.and_then(|o| o.get("share_chat_id")).map(|v| s(Some(v))).unwrap_or_default()
            );
        }
        // 图片
        "image" => {
            out.content = format!(
                "图片，image_key：${}",
                obj.and_then(|o| o.get("image_key")).map(|v| s(Some(v))).unwrap_or_default()
            );
        }
        // 消息卡片
        "interactive" => {
            let card = obj.and_then(|o| o.get("card"));
            if let Some(card) = card {
                out.title = card
                    .get("header")
                    .and_then(|h| h.get("title"))
                    .map(|v| s(Some(v)))
                    .unwrap_or_default();
                if let Some(Value::Array(elements)) = card.get("elements") {
                    for el in elements {
                        match el.get("tag").and_then(Value::as_str) {
                            Some("div") => {
                                if let Some(t) = el.get("text").and_then(|t| t.get("content")) {
                                    out.content.push_str(&s(Some(t)));
                                    out.content.push('\n');
                                }
                            }
                            Some("action") => {
                                if let Some(Value::Array(actions)) = el.get("actions") {
                                    for ac in actions {
                                        match ac.get("tag").and_then(Value::as_str) {
                                            Some("button") => {
                                                let txt = ac
                                                    .get("text")
                                                    .and_then(|t| t.get("content"))
                                                    .map(|v| s(Some(v)))
                                                    .unwrap_or_default();
                                                let url = ac.get("url").map(|v| s(Some(v))).unwrap_or_default();
                                                out.content.push_str(&format!("[{txt}]({url})\n"));
                                            }
                                            Some("hr") => out.content.push_str("---------------------\n"),
                                            Some("img") => {
                                                let key = ac
                                                    .get("img_key")
                                                    .map(|v| s(Some(v)))
                                                    .unwrap_or_default();
                                                out.content.push_str(&format!("图片，image_key：{key}\n"));
                                            }
                                            Some("markdown") => {
                                                out.content.push_str(
                                                    &ac.get("content").map(|v| s(Some(v))).unwrap_or_default(),
                                                );
                                                out.content.push('\n');
                                            }
                                            Some("note") => {
                                                out.content.push_str("备注：\n");
                                                if let Some(Value::Array(els)) = ac.get("elements") {
                                                    for e in els {
                                                        out.content
                                                            .push_str(&e.get("content").map(|v| s(Some(v))).unwrap_or_default());
                                                        out.content.push('\n');
                                                    }
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                out.kind = "markdown".to_string();
            }
        }
        _ => {}
    }
}

/// 飞书 post 富文本单个元素转译
fn feishu_post_element(item: &Value, out: &mut ThirdData) {
    let s = |v: Option<&Value>| -> String {
        match v {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        }
    };
    match item.get("tag").and_then(Value::as_str) {
        Some("text") => {
            out.content.push_str(&s(item.get("text")));
            out.content.push('\n');
        }
        Some("a") => {
            out.content.push_str(&format!(
                "[{}]({})\n",
                s(item.get("text")),
                s(item.get("href"))
            ));
        }
        Some("at") => {
            out.content.push_str(&format!("@{}\n", s(item.get("user_id"))));
        }
        Some("img") => {
            out.content
                .push_str(&format!("图片，image_key：{}\n", s(item.get("image_key"))));
        }
        _ => {}
    }
}

// ------------------------- 企微 / 钉钉 -------------------------

fn weiding(msgtype: &str, params: &Params, out: &mut ThirdData) {
    let empty = Value::Null;
    // 官方：this.$request.post(msgtype, {}) —— 即 body JSON 中键为 msgtype 值的对象
    let node = params.get_val(msgtype).unwrap_or(&empty);
    let s = |v: Option<&Value>| -> String {
        match v {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        }
    };

    match msgtype {
        // 微信 | 钉钉：文本
        "text" => {
            out.content = node.get("content").map(|v| s(Some(v))).unwrap_or_default();
            out.kind = "text".to_string();
        }
        // 微信 | 钉钉：markdown（企微 {content}；钉钉 {title, text}）
        "markdown" => {
            if let Some(c) = node.get("content") {
                out.content = s(Some(c));
            } else {
                out.title = node.get("title").map(|v| s(Some(v))).unwrap_or_default();
                out.content = node.get("text").map(|v| s(Some(v))).unwrap_or_default();
            }
        }
        // 微信：图片
        "image" => {
            out.content = "暂不支持base64图片".to_string();
        }
        // 微信：图文
        "news" => {
            if let Some(Value::Array(articles)) = node.get("articles") {
                for a in articles {
                    out.content.push_str(&format!(
                        "## {}\n{}\n![]({})\n[阅读原文]({})\n\n",
                        s(a.get("title")),
                        s(a.get("description")),
                        s(a.get("picurl")),
                        s(a.get("url"))
                    ));
                }
            }
        }
        // 微信：文件
        "file" => {
            out.content = format!("文件media_id：{}", s(node.get("media_id")));
        }
        // 微信：模板卡片
        "template_card" => {
            out.content = "暂不支持模板卡片消息".to_string();
        }
        // 钉钉：链接
        "link" => {
            out.title = node.get("title").map(|v| s(Some(v))).unwrap_or_default();
            out.content = format!(
                "{}\n![]({})\n[阅读原文]({})",
                s(node.get("text")),
                s(node.get("picUrl")),
                s(node.get("messageUrl"))
            );
        }
        // 钉钉：整体跳转卡片 / 独立跳转卡片
        "actionCard" => {
            out.title = node.get("title").map(|v| s(Some(v))).unwrap_or_default();
            out.content = s(node.get("text"));
            if let Some(single_url) = node.get("singleURL") {
                out.content.push_str(&format!(
                    "\n[{}]({})",
                    s(node.get("singleTitle")),
                    s(Some(single_url))
                ));
            } else if let Some(Value::Array(btns)) = node.get("btns") {
                for b in btns {
                    out.content
                        .push_str(&format!("\n[{}]({})", s(b.get("title")), s(b.get("actionURL"))));
                }
            }
        }
        // 钉钉：feed 流
        "feedCard" => {
            if let Some(Value::Array(links)) = node.get("links") {
                for l in links {
                    out.content.push_str(&format!(
                        "## {}\n![]({})\n[阅读原文]({})\n\n",
                        s(l.get("title")),
                        s(l.get("picURL")),
                        s(l.get("messageURL"))
                    ));
                }
            }
        }
        _ => {
            out.kind = String::new();
        }
    }
}
