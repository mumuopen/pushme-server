# -*- coding: utf-8 -*-
"""0.1.1 修复冒烟测试：面板 push_key 登记语义（对齐官方：key 由客户端生成，服务端只登记）

前置：服务器已启动（面板 127.0.0.1:3010，公网 127.0.0.1:3100），数据目录为全新状态。
用法: python panel_smoke_test.py
"""
import json
import http.cookiejar
import urllib.request
import urllib.parse
import urllib.error

PANEL = 'http://127.0.0.1:3010'
PUBLIC = 'http://127.0.0.1:3100'
KEY = 'bRy3vT1osJpSGeTIRqM6'  # 模拟 PushMe 客户端 App 的推送Key
fails = []


def check(name, cond, extra=''):
    print('  [%s] %s%s' % ('PASS' if cond else 'FAIL', name, (' | ' + extra) if extra else ''))
    if not cond:
        fails.append(name)


def req(opener, url, data=None):
    """POST（data 为 dict 时 urlencoded）；返回 (status, body)"""
    if data is None:
        r = urllib.request.Request(url)
    else:
        r = urllib.request.Request(url, data=urllib.parse.urlencode(data).encode(), method='POST')
    try:
        resp = opener.open(r, timeout=5)
        return resp.getcode(), resp.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()


cj = http.cookiejar.CookieJar()
opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(cj))
anon = urllib.request.build_opener()  # 无 cookie

print('=== 1. 安装并建立会话 ===')
code, body = req(opener, PANEL + '/install', {'user': 'admin', 'password': 'test123456'})
code2, body2 = req(opener, PANEL + '/api/keys')
check('安装成功且 session 生效（/api/keys 可访问）', code == 200 and code2 == 200 and json.loads(body2).get('ok') is True,
      'install=%d keys=%d' % (code, code2))
code3, body3 = req(anon, PANEL + '/api/keys')
check('未登录访问被拒（401）', code3 == 401, 'code=%d' % code3)

print('=== 2. push_key 登记校验（本次修复核心） ===')
# 2.1 缺 key（旧行为只有 note）→ 必须被拒
code, body = req(opener, PANEL + '/api/keys/add', {'note': KEY})
check('缺 push_key 被拒（400 + 明确提示）', code == 400 and 'push_key' in body, body[:60])
# 2.2 正常登记：key 必填，temp_key / note 可选
code, body = req(opener, PANEL + '/api/keys/add',
                 {'key': KEY, 'temp_key': 'temp-alias-1', 'note': '我的手机'})
d = json.loads(body)
check('登记客户端 key 成功（原样保存，无 PUSHME- 前缀）',
      code == 200 and d.get('ok') is True and d.get('key') == KEY and d.get('temp_key') == 'temp-alias-1',
      body[:80])
# 2.3 重复登记同一 key → 拒绝
code, body = req(opener, PANEL + '/api/keys/add', {'key': KEY, 'note': 'dup'})
check('重复登记被拒', code == 400 and '已登记' in body, body[:60])
# 2.4 temp_key 重复 → 拒绝
code, body = req(opener, PANEL + '/api/keys/add', {'key': 'key-two-xxx', 'temp_key': 'temp-alias-1'})
check('重复 temp_key 被拒', code == 400 and 'temp_key' in body, body[:60])
# 2.5 key 前后空白应被 trim
code, body = req(opener, PANEL + '/api/keys/add', {'key': '  key-trim-test  ', 'temp_key': 'tk-trim', 'note': ''})
d = json.loads(body)
check('key 首尾空白被去除', code == 200 and d.get('key') == 'key-trim-test', body[:80])
# 2.6 列表核对
code, body = req(opener, PANEL + '/api/keys')
ks = json.loads(body).get('keys', [])
check('列表正确（key/temp_key/note 三字段）',
      any(k.get('key') == KEY and k.get('temp_key') == 'temp-alias-1' and k.get('note') == '我的手机' for k in ks),
      json.dumps(ks, ensure_ascii=False)[:160])
# 清理 trim 测试数据
req(opener, PANEL + '/api/keys/delete', {'key': 'key-trim-test'})

print('=== 3. 推送 API 鉴权（白名单 = 登记表） ===')
code, body = req(anon, PUBLIC + '/', {'push_key': 'wrong-key-xxx', 'title': 't', 'content': 'c'})
check('非法 push_key 被拒', body == '非法push_key!', body)
code, body = req(anon, PUBLIC + '/', {'push_key': KEY, 'title': 't', 'content': 'c'})
check('已登记 key 推送 success（无在线订阅者）', body == 'success', body)
code, body = req(anon, PUBLIC + '/', {'temp_key': 'temp-alias-1', 'title': 't', 'content': 'c'})
check('temp_key 反查推送 success', body == 'success', body)

print()
print('ALL PASS' if not fails else 'SOME FAILED: %s' % '; '.join(fails))
exit(0 if not fails else 1)
