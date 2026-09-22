# -*- coding: utf-8 -*-
"""WebSocket over MQTT 测试：PushMe App 通过 WS 连接 wss://host:3100/，订阅并接收推送"""
import socket, ssl, struct, json, urllib.request, urllib.parse, sys, time, base64, os
import websocket

KEY = sys.argv[1] if len(sys.argv) > 1 else 'PUSHME-WS'
PORT = 3100

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
        urllib.request.ProxyHandler({}),
        urllib.request.HTTPSHandler(context=ctx),
    )
    with opener.open('http://127.0.0.1:%d/?' % PORT + params, timeout=5) as r:
        return r.read().decode()

def main():
    ok = True
    def check(name, cond):
        nonlocal ok
        mark = 'PASS' if cond else 'FAIL'
        if not cond: ok = False
        print('  [%s] %s' % (mark, name))

    print('=== WS 握手 + MQTT over WS（子协议 mqtt）===')
    ws = websocket.create_connection(
        'ws://127.0.0.1:%d/mqtt' % PORT,
        subprotocols=['mqtt'],
        timeout=5,
        proxy_type='direct',  # 绕过本机代理
    )
    check('WS 握手成功', True)

    ws.send_binary(connect_pkt('ws-android-client'))
    data = ws.recv()
    check('CONNACK code=0', data and data[0] == 0x20 and data[3] == 0)

    ws.send_binary(subscribe_pkt(1, KEY))
    data = ws.recv()
    check('SUBACK 授予', data and data[0] == 0x90 and data[-1] in (0, 1))

    r = api_push(KEY, 'WS集成测试', '来自WS通道的消息 ws-ok')
    check('API推送 success (got "%s")' % r, r == 'success')

    deadline = time.time() + 8
    got = False
    while time.time() < deadline:
        try:
            data = ws.recv()
        except Exception:
            break
        if not data or data[0] not in (0x30, 0x32):
            continue
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
                  topic == KEY and obj.get('title') == 'WS集成测试')
            got = True
            break
    if not got:
        check('收到 Publish', False)
    ws.close()

    print()
    print('ALL PASS' if ok else 'SOME FAILED')
    sys.exit(0 if ok else 1)

if __name__ == '__main__':
    main()