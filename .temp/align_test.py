# -*- coding: utf-8 -*-
"""官方功能补齐自测：端口/启停/证书多域名/日志 history+clear+SSE/未登录 401。
用法: python align_test.py"""
import urllib.request, urllib.parse, json, sys, socket, ssl, time

PANEL = 'http://127.0.0.1:3010'
USER, PW = 'admin', 'admin123'
KEY = 'PUSHME-46cbe689b98d8642de2459685c24465e'

def login():
    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, req, fp, code, msg, headers, newurl):
            return None
    opener = urllib.request.build_opener(NoRedirect)
    req = urllib.request.Request(PANEL + '/login',
                                 data=urllib.parse.urlencode({'user': USER, 'password': PW}).encode(),
                                 method='POST')
    try:
        with opener.open(req, timeout=5) as r:
            return '', None
    except urllib.error.HTTPError as e:
        c = e.headers.get('Set-Cookie', '').split(';')[0]
        return c, opener

def post(opener, cookie, path, data):
    req = urllib.request.Request(PANEL + path,
                                 data=urllib.parse.urlencode(data).encode(),
                                 headers={'Cookie': cookie}, method='POST')
    try:
        with opener.open(req, timeout=5) as r:
            return json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        try:
            return json.loads(e.read().decode())
        except Exception:
            return {'ok': False, 'message': str(e)}

def get(opener, cookie, path):
    req = urllib.request.Request(PANEL + path, headers={'Cookie': cookie})
    with opener.open(req, timeout=5) as r:
        return json.loads(r.read().decode())

def main():
    ok = True
    def check(name, cond):
        nonlocal ok
        if not cond: ok = False
        print('  [%s] %s' % ('PASS' if cond else 'FAIL', name))

    cookie, opener = login()
    if not cookie:
        print('  [FAIL] 登录失败'); return 1

    print('=== 1. 端口设置 ===')
    r = post(opener, cookie, '/api/setting/ports', {'server_port': '3100', 'panel_port': '3010'})
    check('保存端口 (ok=%s msg=%s)' % (r.get('ok'), r.get('message')), r.get('ok') is True)

    print('=== 2. 服务状态设置 ===')
    r = post(opener, cookie, '/api/setting/status', {'status': 'start'})
    check('保存 start (ok=%s)' % r.get('ok'), r.get('ok') is True)
    r = post(opener, cookie, '/api/setting/status', {'status': 'bad'})
    check('非法状态被拒 (ok=%s)' % r.get('ok'), r.get('ok') is False)

    print('=== 3. 证书多域名生成 ===')
    r = post(opener, cookie, '/api/cert/generate', {'domains': 'push.example.com,10.0.0.5'})
    check('生成证书 (ok=%s)' % r.get('ok'), r.get('ok') is True)
    cert_file = r'G:\talk\dev\pushme-server-rs\.temp\pushme-test\config\certs\cert.crt'
    import subprocess
    out = subprocess.run(['certutil', '-dump', cert_file], capture_output=True).stdout.decode('gbk', errors='replace')
    check('证书含多域名 push.example.com', 'push.example.com' in out)
    check('证书自动补 127.0.0.1', '127.0.0.1' in out)
    check('证书包含 IP SAN 10.0.0.5', '10.0.0.5' in out)

    print('=== 4. 日志 history / clear ===')
    h = get(opener, cookie, '/api/log/history?count=10')
    check('history 返回数组 (len=%s)' % len(h.get('logs', [])), h.get('ok') is True and isinstance(h.get('logs'), list))
    r = post(opener, cookie, '/api/log/clear', {})
    check('clear (ok=%s)' % r.get('ok'), r.get('ok') is True)
    h2 = get(opener, cookie, '/api/log/history?count=10')
    check('清空后 history 为空 (len=%s)' % len(h2.get('logs', [])), len(h2.get('logs', [])) == 0)

    print('=== 5. 日志 SSE 实时流 ===')
    s = socket.create_connection(('127.0.0.1', 3010), timeout=5)
    req = ('GET /api/log/stream HTTP/1.1\r\nHost: 127.0.0.1\r\nCookie: %s\r\nAccept: text/event-stream\r\nConnection: close\r\n\r\n' % cookie).encode()
    s.sendall(req)
    time.sleep(0.5)
    params = urllib.parse.urlencode({'push_key': KEY, 'title': 'SSE测试', 'content': '日志流验证'})
    with urllib.request.urlopen('http://127.0.0.1:3100/?' + params, timeout=5) as r:
        resp = r.read().decode()
    time.sleep(1.0)
    data = b''
    s.settimeout(2)
    try:
        while True:
            chunk = s.recv(4096)
            if not chunk: break
            data += chunk
    except socket.timeout:
        pass
    s.close()
    body = data.decode(errors='replace')
    check('SSE 实时收到推送事件 (%s)' % ('SSE测试' in body), 'SSE测试' in body)

    print('=== 6. 未登录新接口 401 ===')
    cases = [
        ('/api/setting/ports', 'POST'),
        ('/api/setting/status', 'POST'),
        ('/api/log/history?count=10', 'GET'),
        ('/api/log/clear', 'POST'),
        ('/api/log/stream', 'GET'),
    ]
    for p, m in cases:
        try:
            data = b'{}' if m == 'POST' else None
            req = urllib.request.Request(PANEL + p, data=data, method=m)
            with urllib.request.urlopen(req, timeout=5) as r:
                code = r.status
        except urllib.error.HTTPError as e:
            code = e.code
        check('未登录 %s -> %s' % (p, code), code in (401, 403))

    print()
    print('ALL PASS' if ok else 'SOME FAILED')
    return 0 if ok else 1

if __name__ == '__main__':
    sys.exit(main())