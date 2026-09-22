# -*- coding: utf-8 -*-
"""验证 offline_messages=false 时：断开期间的发布不缓存、重连不补发。
用法: python offline_off_test.py <key>"""
import socket, struct, json, urllib.request, urllib.parse, sys, time

HOST, PORT = '127.0.0.1', 3100
KEY = sys.argv[1] if len(sys.argv) > 1 else 'PUSHME-46cbe689b98d8642de2459685c24465e'

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

def collect_publishes(s, expect, timeout=4):
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
            if (t >> 4) == 3:
                tl = struct.unpack('>H', body[:2])[0]
                topic = body[2:2+tl].decode()
                payload = body[2+tl+2:]
                try:
                    d = json.loads(payload)
                    got.append({'topic': topic, 'title': d.get('title'), 'content': d.get('content')})
                except Exception:
                    got.append({'topic': topic, 'title': None})
    return got

import time

def main():
    ok = True
    def check(name, cond):
        nonlocal ok
        print('  [%s] %s' % ('PASS' if cond else 'FAIL', name))
        if not cond: ok = False

    # 1. 订阅后断开
    s = socket.create_connection((HOST, PORT), timeout=5)
    s.sendall(connect_pkt('off-off-a'))
    p = recv_packet(s)
    check('CONNACK', p and p[0] == 0x20)
    s.sendall(subscribe_pkt(1, KEY))
    p = recv_packet(s)
    check('SUBACK', p and p[0] == 0x90)
    s.close()
    time.sleep(0.3)

    # 2. 断开期间发布 2 条
    for i in range(1, 3):
        r = api_push(KEY, 'offoff-%d' % i, '开关关闭%d' % i)
        check('推送 offoff-%d -> "%s"' % (i, r), r == 'success')
    time.sleep(0.3)

    # 3. 重连订阅：开关关闭 → 不应收到任何补发
    s2 = socket.create_connection((HOST, PORT), timeout=5)
    s2.sendall(connect_pkt('off-off-b'))
    recv_packet(s2)
    s2.sendall(subscribe_pkt(2, KEY))
    p = recv_packet(s2)
    check('SUBACK2', p and p[0] == 0x90)
    got = collect_publishes(s2, 1, timeout=3)
    check('开关关闭时无补发 (got=%s)' % [g['title'] for g in got], len(got) == 0)
    s2.close()

    print()
    print('ALL PASS' if ok else 'SOME FAILED')
    sys.exit(0 if ok else 1)

if __name__ == '__main__':
    main()