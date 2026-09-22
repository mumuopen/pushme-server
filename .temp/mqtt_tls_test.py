# -*- coding: utf-8 -*-
"""MQTT over TLS 测试：ssl 包装 socket 连接 3100，验证 TLS 模式下的嗅探与消息链路"""
import socket, ssl, struct, json, urllib.request, urllib.parse, sys, time

HOST, PORT = '127.0.0.1', 3100
KEY = sys.argv[1] if len(sys.argv) > 1 else 'PUSHME-TLS'

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

def api_push(push_key, title, content):
    params = urllib.parse.urlencode({'push_key': push_key, 'title': title, 'content': content})
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    opener = urllib.request.build_opener(
        urllib.request.ProxyHandler({}),  # 绕过本机代理
        urllib.request.HTTPSHandler(context=ctx),
    )
    with opener.open('https://127.0.0.1:3100/?' + params, timeout=5) as r:
        return r.read().decode()

def recv_raw(s, timeout=5):
    s.settimeout(timeout)
    try:
        return s.recv(4096)
    except socket.timeout:
        return None

def main():
    ok = True
    def check(name, cond):
        nonlocal ok
        mark = 'PASS' if cond else 'FAIL'
        if not cond: ok = False
        print('  [%s] %s' % (mark, name))

    print('=== TLS 握手 + MQTT over TLS ===')
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE  # 自签名证书
    raw = socket.create_connection((HOST, PORT), timeout=5)
    s = ctx.wrap_socket(raw, server_hostname='push.test.local')
    check('TLS 握手成功', True)

    s.sendall(connect_pkt('tls-android-client'))
    data = recv_raw(s)
    check('CONNACK code=0', data and len(data) >= 4 and data[0] == 0x20 and data[3] == 0)

    s.sendall(subscribe_pkt(1, KEY))
    data = recv_raw(s)
    check('SUBACK 授予', data and data[0] == 0x90 and data[-1] in (0, 1))

    r = api_push(KEY, 'TLS集成测试', '来自TLS通道的消息 tls-ok')
    check('API推送 success (got "%s")' % r, r == 'success')

    deadline = time.time() + 8
    got = False
    while time.time() < deadline:
        data = recv_raw(s, 2)
        if data is None:
            continue
        if data[0] in (0x30, 0x32):
            i, mult, rl = 1, 1, 0
            while True:
                b = data[i]; i += 1
                rl += (b & 0x7F) * mult
                if not (b & 0x80): break
                mult *= 128
            if len(data) >= i + rl:
                body = data[i:i+rl]
                tl = struct.unpack('>H', body[:2])[0]
                topic = body[2:2+tl].decode()
                payload = body[2+tl+2:]
                obj = json.loads(payload)
                check('收到 Publish topic=%s title=%s' % (topic, obj.get('title')),
                      topic == KEY and obj.get('title') == 'TLS集成测试')
                got = True
                break
    if not got:
        check('收到 Publish', False)
    s.close()

    print()
    print('ALL PASS' if ok else 'SOME FAILED')
    sys.exit(0 if ok else 1)

if __name__ == '__main__':
    main()