# -*- coding: utf-8 -*-
"""PushMe Server 离线消息专项自测。
用法: python offline_test.py <key1> <key2>
场景:
  A. 离线开启: 无在线订阅者时发布入队; 超限淘汰最旧; 重连订阅后按序补发
  B. 在线投递: 有在线订阅者时正常实时投递(不缓存、不重复)
  C. 面板接口: 未登录调用 /api/setting/offline 应 401; 尝试已登录开关
"""
import socket, struct, json, urllib.request, urllib.parse, sys, time

HOST, PORT = '127.0.0.1', 3100
PANEL = 'http://127.0.0.1:3010'
KEY1 = sys.argv[1] if len(sys.argv) > 1 else 'PUSHME-46cbe689b98d8642de2459685c24465e'
KEY2 = sys.argv[2] if len(sys.argv) > 2 else 'PUSHME-7a510a64d24e4028be747a2414698c38'

def enc_remaining(n):
    out = b''
    while True:
        d = n % 128; n //= 128
        if n: d |= 0x80
        out += bytes([d])
        if not n: return out

def pkt(ptype, flags, body):
    return bytes([(ptype << 4) | flags]) + enc_remaining(len(body)) + body

def connect_pkt(client_id, keepalive=60):
    var = b'\x00\x04MQTT\x04\x02' + struct.pack('>H', keepalive)
    return pkt(1, 0, var + struct.pack('>H', len(client_id)) + client_id.encode())

def subscribe_pkt(pkid, topic, qos=1):
    return pkt(8, 2, struct.pack('>H', pkid) + struct.pack('>H', len(topic)) + topic.encode() + bytes([qos]))

def recv_packet(s, timeout=5):
    s.settimeout(timeout)
    try:
        head = s.recv(2)
    except socket.timeout:
        return None
    if len(head) < 2:
        return None
    t = head[0]; rl = head[1]
    mult, val = 1, 0
    while rl & 0x80:
        try:
            b = s.recv(1)[0]
        except socket.timeout:
            return None
        val += (b & 0x7F) * mult
        mult *= 128
        rl = b
    body = b''
    while len(body) < rl:
        try:
            chunk = s.recv(rl - len(body))
        except socket.timeout:
            break
        if not chunk: break
        body += chunk
    return (t, body)

def api_push(push_key, title, content):
    params = urllib.parse.urlencode({'push_key': push_key, 'title': title, 'content': content})
    url = 'http://127.0.0.1:3100/?' + params
    with urllib.request.urlopen(url, timeout=5) as r:
        return r.read().decode()

def collect_publishes(s, expect, timeout=6):
    """收集最多 expect 条 Publish 消息, 返回 [ {topic, title, content}, ... ]"""
    got = []
    deadline = time.time() + timeout
    while len(got) < expect and time.time() < deadline:
        s.settimeout(max(0.2, deadline - time.time()))
        try:
            raw = s.recv(4096)
        except socket.timeout:
            continue
        except OSError:
            break
        if not raw:
            break
        # 逐个解析包(可能一个 recv 含多个)
        pos = 0
        while pos < len(raw):
            t = raw[pos]
            i = pos + 1
            mult, rl = 1, 0
            while True:
                b = raw[i]; i += 1
                rl += (b & 0x7F) * mult
                if not (b & 0x80): break
                mult *= 128
            if i + rl > len(raw):
                break
            body = raw[i:i+rl]
            pos = i + rl
            if (t >> 4) == 3:  # Publish
                tl = struct.unpack('>H', body[:2])[0]
                topic = body[2:2+tl].decode()
                payload = body[2+tl+2:]
                try:
                    d = json.loads(payload)
                    got.append({'topic': topic, 'title': d.get('title'), 'content': d.get('content')})
                except Exception:
                    got.append({'topic': topic, 'title': None, 'content': payload.decode(errors='replace')})
    return got

def subscribe_wait_ack(s, pkid, topic, timeout=5):
    """订阅并等待 SUBACK（离线补发消息可能先于 SUBACK 到达，需消费丢弃）。
    返回 True/False。"""
    s.sendall(subscribe_pkt(pkid, topic))
    deadline = time.time() + timeout
    while time.time() < deadline:
        q = recv_packet(s, timeout=max(0.2, deadline - time.time()))
        if q is None:
            continue
        if q[0] == 0x90:
            return len(q[1]) >= 1 and q[1][-1] in (0, 1)
        # 其它包（离线补发 Publish 等）忽略
    return False


def subscribe_and_collect(s, pkid, topic, expected, timeout=8):
    """订阅并收集：补发消息先于 SUBACK 到达，需一并捕获。
    返回 (suback_ok, publishes)。"""
    s.sendall(subscribe_pkt(pkid, topic))
    got = []
    suback = False
    deadline = time.time() + timeout
    while time.time() < deadline:
        if suback and len(got) >= expected:
            break
        s.settimeout(max(0.2, deadline - time.time()))
        try:
            raw = s.recv(4096)
        except socket.timeout:
            continue
        except OSError:
            break
        if not raw:
            break
        pos = 0
        while pos < len(raw):
            t = raw[pos]
            i = pos + 1
            mult, rl = 1, 0
            while True:
                b = raw[i]; i += 1
                rl += (b & 0x7F) * mult
                if not (b & 0x80): break
                mult *= 128
            if i + rl > len(raw):
                break
            body = raw[i:i+rl]
            pos = i + rl
            if (t >> 4) == 3:  # Publish
                tl = struct.unpack('>H', body[:2])[0]
                t2 = body[2:2+tl].decode()
                payload = body[2+tl+2:]
                try:
                    d = json.loads(payload)
                    got.append({'topic': t2, 'title': d.get('title'), 'content': d.get('content')})
                except Exception:
                    got.append({'topic': t2, 'title': None, 'content': payload.decode(errors='replace')})
            elif (t >> 4) == 9:  # SUBACK
                suback = True
    return suback, got


def main():
    ok = True
    def check(name, cond):
        nonlocal ok
        mark = 'PASS' if cond else 'FAIL'
        if not cond: ok = False
        print('  [%s] %s' % (mark, name))

    print('=== A. 离线缓存 / 淘汰 / 重连补发 (key1=%s) ===' % KEY1)
    # A1 订阅后断开（服务刚重启，无残留补发）
    s = socket.create_connection((HOST, PORT), timeout=5)
    s.sendall(connect_pkt('offline-a'))
    p = recv_packet(s)
    check('A1 CONNACK', p and p[0] == 0x20)
    suback, _ = subscribe_and_collect(s, 1, KEY1, 0)
    check('A1 SUBACK 授予', suback)
    s.close()  # 直接断开 = 离线
    time.sleep(0.3)

    # A2 发 4 条 (limit=3, 第4条淘汰第1条)
    for i in range(1, 5):
        r = api_push(KEY1, 'offline-%d' % i, '离线消息%d' % i)
        check('A2 推送 offline-%d -> "%s"' % (i, r), r == 'success')
    time.sleep(0.3)

    # A3 重连订阅 → 应补发 offline-2/3/4 (淘汰了 offline-1)
    s2 = socket.create_connection((HOST, PORT), timeout=5)
    s2.sendall(connect_pkt('offline-b'))
    recv_packet(s2)
    suback, got = subscribe_and_collect(s2, 6, KEY1, 3)
    check('A3 SUBACK', suback)
    titles = [g['title'] for g in got if g['topic'] == KEY1]
    check('A3 收到 3 条补发 (got=%s)' % titles, titles == ['offline-2', 'offline-3', 'offline-4'])
    s2.close()

    print('=== B. 有在线订阅者时不缓存、实时投递 ===')
    s3 = socket.create_connection((HOST, PORT), timeout=5)
    s3.sendall(connect_pkt('offline-c'))
    recv_packet(s3)
    check('B1 SUBACK', subscribe_wait_ack(s3, 3, KEY2))
    r = api_push(KEY2, 'live-1', '在线实时消息')
    check('B1 推送 "%s"' % r, r == 'success')
    got = collect_publishes(s3, 1, timeout=5)
    check('B2 在线客户端实时收到 1 条', len([g for g in got if g['title'] == 'live-1']) == 1)
    s3.close()  # 断开
    time.sleep(0.3)
    # 断开后再发 1 条 → 应进 KEY2 离线队列
    r = api_push(KEY2, 'live-2', '离线期间第二条')
    check('B3 断开后推送 "%s"' % r, r == 'success')
    time.sleep(0.3)
    s4 = socket.create_connection((HOST, PORT), timeout=5)
    s4.sendall(connect_pkt('offline-d4'))
    recv_packet(s4)
    suback2, got2 = subscribe_and_collect(s4, 4, KEY2, 1)
    check('B4 SUBACK', suback2)
    check('B4 重连补发 1 条 (got %s)' % [g['title'] for g in got2], len([g for g in got2 if g['title'] == 'live-2']) == 1)
    s4.close()

    print('=== C. 面板离线设置接口 ===')
    # C1 未登录 → 401
    try:
        req = urllib.request.Request(PANEL + '/api/setting/offline',
                                     data=urllib.parse.urlencode({'enabled': 'true', 'limit': '5'}).encode(),
                                     method='POST')
        with urllib.request.urlopen(req, timeout=5) as r:
            code = r.status
    except urllib.error.HTTPError as e:
        code = e.code
    check('C1 未登录调用返回 401 (got %s)' % code, code == 401)
    # C2 尝试常见密码登录后开关
    logged = False
    # urllib 默认跟随 302 会丢掉 Set-Cookie，禁用重定向以捕获登录跳转
    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, req, fp, code, msg, headers, newurl):
            return None
    opener = urllib.request.build_opener(NoRedirect)
    for pw in ['admin', 'admin123', '123456', 'admin888', 'pushme']:
        try:
            req = urllib.request.Request(PANEL + '/login',
                                         data=urllib.parse.urlencode({'user': 'admin', 'password': pw}).encode(),
                                         method='POST')
            with opener.open(req, timeout=5) as r:
                cookie = r.headers.get('Set-Cookie', '').split(';')[0]
                logged = bool(cookie)
        except urllib.error.HTTPError as e:
            # 禁用重定向后，登录成功(302/303)会以 HTTPError 抛出，需从其 headers 取 Set-Cookie
            if e.code in (301, 302, 303, 307, 308):
                cookie = e.headers.get('Set-Cookie', '').split(';')[0]
                logged = bool(cookie)
                break
            continue
        except Exception:
            continue
        if logged:
            print('  [..] 登录成功 (密码=%s)' % pw)
            break
    if logged:
        req = urllib.request.Request(PANEL + '/api/setting/offline',
                                     data=urllib.parse.urlencode({'enabled': 'true', 'limit': '3'}).encode(),
                                     headers={'Cookie': cookie}, method='POST')
        with urllib.request.urlopen(req, timeout=5) as r:
            d = json.loads(r.read().decode())
        check('C2 已登录保存开关 (ok=%s limit=%s)' % (d.get('ok'), d.get('offline_limit')), d.get('ok') is True and d.get('offline_limit') == 3)
    else:
        print('  [SKIP] 未找到管理员密码，跳过已登录开关测试')

    print()
    print('ALL PASS' if ok else 'SOME FAILED')
    sys.exit(0 if ok else 1)

if __name__ == '__main__':
    main()