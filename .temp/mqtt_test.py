# -*- coding: utf-8 -*-
"""PushMe Server MQTT 协议级自测（模拟 PushMe App）。用法: python mqtt_test.py <push_key>"""
import socket, struct, json, urllib.request, urllib.parse, sys, time

HOST, PORT = '127.0.0.1', 3100
KEY = sys.argv[1] if len(sys.argv) > 1 else 'PUSHME-TEST'

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

def publish_pkt(pkid, topic, payload, qos=1):
    var = struct.pack('>H', len(topic)) + topic.encode() + struct.pack('>H', pkid)
    return pkt(3, qos << 1, var + payload)

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

def main():
    ok = True
    def check(name, cond):
        nonlocal ok
        mark = 'PASS' if cond else 'FAIL'
        if not cond: ok = False
        print('  [%s] %s' % (mark, name))

    print('=== 1. 正常连接 + 订阅合法 topic + 接收消息 ===')
    s = socket.create_connection((HOST, PORT), timeout=5)
    s.sendall(connect_pkt('test-android-client'))
    p = recv_packet(s)
    check('CONNACK code=0', p and p[0] == 0x20 and p[1][1] == 0)
    s.sendall(subscribe_pkt(1, KEY))
    p = recv_packet(s)
    check('SUBACK 授予', p and p[0] == 0x90 and p[1][-1] in (0, 1))

    r = api_push(KEY, 'MQTT集成测试', '来自推送API的消息 hello-mqtt')
    check('API推送 success (got "%s")' % r, r == 'success')

    deadline = time.time() + 8
    got = None
    raw_seen = []
    while time.time() < deadline:
        s.settimeout(2)
        try:
            raw = s.recv(4096)
        except socket.timeout:
            raw_seen.append('(timeout)')
            continue
        if not raw:
            raw_seen.append('(closed)')
            break
        raw_seen.append(raw[:24].hex())
        # 直接解析第一个完整包：0x30 Publish
        if raw[0] == 0x30 or raw[0] == 0x32:
            # remaining length
            i, mult, rl = 1, 1, 0
            while True:
                b = raw[i]; i += 1
                rl += (b & 0x7F) * mult
                if not (b & 0x80): break
                mult *= 128
            if len(raw) >= i + rl:
                body = raw[i:i+rl]
                tl = struct.unpack('>H', body[:2])[0]
                topic = body[2:2+tl].decode()
                payload = body[2+tl+2:]
                got = json.loads(payload)
                check('收到 Publish topic=%s title=%s' % (topic, got.get('title')),
                      topic == KEY and got.get('title') == 'MQTT集成测试' and got.get('content') == '来自推送API的消息 hello-mqtt')
                break
    if got is None:
        check('收到 Publish (raw: %s)' % ' | '.join(raw_seen[:6]), False)
    s.close()
    time.sleep(0.3)

    print('=== 2. 订阅非法 topic（应 SubAck 0x80）===')
    s2 = socket.create_connection((HOST, PORT), timeout=5)
    s2.sendall(connect_pkt('acl-client'))
    recv_packet(s2)
    s2.sendall(subscribe_pkt(2, 'not-a-real-key'))
    p = recv_packet(s2)
    check('SUBACK 拒绝', p and p[0] == 0x90 and p[1][-1] == 0x80)
    s2.close()

    print('=== 3. 发布非法 topic（应断开连接）===')
    s3 = socket.create_connection((HOST, PORT), timeout=5)
    s3.sendall(connect_pkt('acl-pub-client'))
    recv_packet(s3)
    s3.sendall(publish_pkt(7, 'FA/evil', b'{"x":1}', qos=1))
    p = recv_packet(s3, timeout=3)
    check('发布非法后连接被断开', p is None)
    s3.close()

    print()
    print('ALL PASS' if ok else 'SOME FAILED')
    sys.exit(0 if ok else 1)

if __name__ == '__main__':
    main()