#!/usr/bin/env python3
"""WORK-01/02 destructive lifecycle tests, restricted to a disposable Linux container."""
import concurrent.futures, json, os, pathlib, shutil, signal, socket, struct, subprocess, tempfile, time, uuid

assert os.geteuid() == 0 and pathlib.Path('/.dockerenv').exists()
assert os.environ.get('ASTRA_ISOLATED_MANAGED_TEST') == '1'
binary = pathlib.Path(os.environ.get('ASTRA_TEST_BIN_DIR', '/build/debug'))
root = pathlib.Path(tempfile.mkdtemp(prefix='astra-worker-test-'))
root.chmod(0o755)
children, users = [], []
state, auth, home, demo = [root / n for n in ('state', 'authorized', 'home', 'demo')]
for p in (auth, home, demo): p.mkdir(mode=0o755)
(demo / 'README').write_text('isolated worker fixture\n')
env = dict(os.environ, HOME=str(home), XDG_CONFIG_HOME=str(home / '.config'))

def run(args, **kw):
    result = subprocess.run(list(map(str, args)), text=True, capture_output=True, env=env, timeout=20, **kw)
    if result.returncode: raise RuntimeError(result.stderr)
    return result.stdout.strip()

def wait_for(check, seconds=10):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        try:
            value = check()
            if value: return value
        except (OSError, ValueError): pass
        time.sleep(.03)
    raise AssertionError('deadline expired')

def start_gateway():
    log = open(root / ('gateway-%s.log' % len(children)), 'w+')
    p = subprocess.Popen([str(binary / 'astrad'), 'serve', '--managed', '--listen', '127.0.0.1:0', '--state-dir', str(state), '--authorized-keys-dir', str(auth), '--session-root', str(demo), '--worker-idle-timeout-seconds', '600', '--max-user-forward-connections', '2', '--max-global-forward-connections', '4'], stdout=log, stderr=log, env=env)
    children.append(p)
    def listen():
        log.seek(0)
        return next((line.strip().split(':')[-1] for line in log if line.startswith('LISTEN ')), None)
    return p, wait_for(listen)

def cli(user, *args):
    result = run([binary / 'astra', '-p', port, '-o', 'StrictHostKeyChecking=accept-new', '-i', root / ('key-' + user), user + '@127.0.0.1', *args])
    time.sleep(.2)  # Allow QUIC close delivery; keep normal connection quotas unchanged.
    return result

def identity(uid):
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(3); s.connect(str(state / 'users' / str(uid) / 'session.sock'))
        challenge = uuid.uuid4().bytes; s.sendall(b'ASTRAWK2' + challenge)
        data = b''
        while len(data) < 60:
            part = s.recv(60-len(data)); assert part; data += part
        assert data[:8] == b'ASTRAWK2' and data[44:] == challenge
        version, reported_uid, pid = struct.unpack('!III', data[8:20])
        assert version == 2 and reported_uid == uid
        credentials = struct.unpack('3i', s.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
        assert credentials[0] == pid and credentials[1] == uid
        return (pid, data[20:36].hex())

try:
    run([binary / 'astrad', 'init', '--state-dir', state])
    for i in range(2):
        user = 'astratest%d' % (os.getpid()*2+i)
        run(['useradd', '-M', '-d', demo, '-s', '/bin/sh', user]); users.append(user)
        run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', root / ('key-' + user)])
        shutil.copyfile(root / ('key-' + user + '.pub'), auth / user); (auth / user).chmod(0o600)
    gateway, port = start_gateway()
    cli(users[0], 'list')
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        list(pool.map(lambda _: cli(users[0], 'list'), range(24)))
    ids = [int(run(['id', '-u', u])) for u in users]
    terminals = [cli(u, 'new', '--name', 'survivor', '--', '/bin/sh', '-c', 'sleep 300') for u in users]
    before = [identity(uid) for uid in ids]
    # A duplicate worker cannot acquire the lifetime lock or unlink the serving socket.
    duplicate = subprocess.run(['setpriv', '--reuid='+str(ids[0]), '--regid='+run(['id','-g',users[0]]), '--clear-groups', str(binary/'astrad'), 'worker', '--expected-uid', str(ids[0]), '--socket', str(state/'users'/str(ids[0])/'session.sock'), '--state-dir', str(state/'users'/str(ids[0])), '--session-root', str(demo), '--idle-timeout-seconds', '600'], capture_output=True, timeout=5)
    assert duplicate.returncode != 0 and identity(ids[0]) == before[0]
    gateway.terminate(); gateway.wait(timeout=5)
    gateway, port = start_gateway()
    for i, user in enumerate(users):
        assert terminals[i] in cli(user, 'list')
        assert identity(ids[i]) == before[i]
    # Kill only the test worker. Another UID's worker and PTY must survive.
    os.kill(before[0][0], signal.SIGKILL)
    assert terminals[1] in cli(users[1], 'list') and identity(ids[1]) == before[1]
    recovered = cli(users[0], 'list')
    assert terminals[0] not in recovered and identity(ids[0]) != before[0]
    assert terminals[1] in cli(users[1], 'list')
    # A root-owned fake peer with user-owned socket metadata is still the wrong UID.
    victim_pid = identity(ids[0])[0]
    os.kill(victim_pid, signal.SIGKILL)
    wait_for(lambda: not pathlib.Path('/proc/%d' % victim_pid).exists())
    sockpath = state / 'users' / str(ids[0]) / 'session.sock'
    sockpath.unlink(missing_ok=True)
    with socket.socket(socket.AF_UNIX) as fake:
        fake.bind(str(sockpath)); fake.listen(); sockpath.chmod(0o600); os.chown(sockpath, ids[0], -1)
        inode = sockpath.stat().st_ino
        try: cli(users[0], 'list'); raise AssertionError('accepted wrong-UID peer')
        except RuntimeError as error: assert 'worker' in str(error)
        assert sockpath.stat().st_ino == inode
    sockpath.unlink()
    cli(users[0], 'list')
    assert terminals[1] in cli(users[1], 'list')
    if os.environ.get('ASTRA_TEST_MANAGED_FORWARD') == '1':
        fixture = '|'.join([str(root), port, *users, *map(str, ids)])
        checked = subprocess.run(['cargo', 'test', '--lib', 'managed_uid_quota_and_lifecycle', '--', '--ignored', '--nocapture'],
            env=dict(os.environ, ASTRA_MANAGED_FORWARD_FIXTURE=fixture), text=True, capture_output=True, timeout=180)
        (root / 'forwarding.log').write_text(checked.stdout + checked.stderr)
        assert checked.returncode == 0, checked.stdout + checked.stderr
        print(checked.stdout)
    print(json.dumps({'result':'passed' , 'concurrent_requests':24, 'uids':ids, 'worker_identity_before':before, 'gateway_restart_preserved_ptys':True, 'cross_uid_kill_isolated':True, 'wrong_uid_peer_rejected_without_replacement':True, 'artifacts':str(root)}))
finally:
    for user in users:
        try:
            uid = int(run(['id', '-u', user])); pid = identity(uid)[0]
            os.kill(pid, signal.SIGTERM)
        except Exception: pass
    for p in children:
        if p.poll() is None: p.terminate()
        try: p.wait(timeout=5)
        except subprocess.TimeoutExpired: p.kill(); p.wait()
    for user in users: subprocess.run(['userdel', user], capture_output=True)
