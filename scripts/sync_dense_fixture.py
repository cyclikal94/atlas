"""BE-Q23 dense `/sync` fixtures and a server-side delivery profile, on real server processes.

Seeds two workloads through the public routes of a freshly started `atlas-server` and drains
the reader's delta without `max_changes` and at a fixed server-side sweep of bounds:

- W-real: one person and 199 dense checklist fields (100 one-character items, 40 checked),
  hidden from the reader, then one grant of the person: the densest revision real writers
  publish for a reader (200 changes).
- W-synthetic (labelled synthetic): 799 such fields and their person, with one stored batch
  naming all 800 inserted directly into the disposable database. Real publication caps a
  revision at 200 changes; this is the brief's larger control.

It writes every response body, a normalised copy (random cursor parts replaced, so two runs
compare byte for byte) and a manifest with SHA-256 hashes, for the client's stub server and
reassembly checks. The output is a SERVER-SIDE DELIVERY PROFILE, NOT CLIENT MEMORY EVIDENCE:
times are wall-clock round trips over loopback measured by this script.

Safety: binds only to 127.0.0.1, creates its own SQLite database under $TMPDIR unless an
explicitly disposable PostgreSQL URL is passed with --database, refuses a non-empty
--output directory, and stops every process it started.

  python3 scripts/sync_dense_fixture.py --output DIR [--database URL --psql PATH]
      [--replicas 2] [--restart-between] [--serve]
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]
V1 = '/api/experimental/v1'
PASSWORD = 'dense-fixture-password-123'
SWEEP = [4, 25, 50, 100, 200, 800]
NAMESPACE = uuid.UUID('6f0e7c1a-3b9d-4d55-9a07-5be2b1a0d023')
LABEL = 'server-side delivery profile; not client memory evidence'
BRIEF = {'bytes': round(5.83 * 1024 * 1024), 'nodes': 455214}


def seeded(*parts):
    return str(uuid.uuid5(NAMESPACE, '/'.join(str(p) for p in parts)))


def checklist():
    items = [{'id': seeded('item', i), 'label': 'x'} for i in range(100)]
    return {'kind': 'checklist', 'items': items, 'checked': [i['id'] for i in items[:40]]}


def nodes(value):
    """Every JSON value (objects, arrays and scalars) plus every object key."""
    if isinstance(value, dict):
        return 1 + len(value) + sum(nodes(v) for v in value.values())
    if isinstance(value, list):
        return 1 + sum(nodes(v) for v in value)
    return 1


def normalised(body):
    """The body with the random parts of its cursor (device MAC, expiry, the key-masked
    continuation position, snapshot token) replaced; everything else is deterministic for a
    fresh database and seeded ids."""
    value = json.loads(body)
    cursor = value.get('next_cursor', '')
    parts = cursor.split('.')
    if cursor.startswith('d1.') and len(parts) == 4:
        value['next_cursor'] = f'd1.{parts[1]}.<expiry>.<mac>'
    elif cursor.startswith('d2.') and len(parts) == 7:
        value['next_cursor'] = f'd2.{parts[1]}.<expiry>.{parts[3]}.{parts[4]}.<position>.<mac>'
    elif cursor:
        value['next_cursor'] = '<snapshot>'
    return json.dumps(value, sort_keys=True, separators=(',', ':')).encode()


class Server:
    """One `atlas-server` process on an ephemeral loopback port."""

    def __init__(self, binary, env, log):
        self.binary, self.env, self.log, self.process, self.base = binary, env, log, None, None

    def start(self):
        with self.log.open('a') as logs:
            offset = self.log.stat().st_size
            self.process = subprocess.Popen([str(self.binary)], env=self.env,
                                            stdout=subprocess.DEVNULL, stderr=logs)
        deadline = time.monotonic() + 30
        while True:
            text = self.log.read_text()[offset:]
            if '\n' in text:
                address = json.loads(text.splitlines()[0])['address']
                host = address.rsplit(':', 1)[0]
                if host != '127.0.0.1':
                    raise RuntimeError(f'refusing a non-loopback bind: {address}')
                self.base = 'http://' + address
                return
            if self.process.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError('server startup failed: ' + text)
            time.sleep(0.02)

    def stop(self):
        if self.process and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.process = None


class Client:
    def __init__(self, servers):
        self.servers, self.turn = servers, 0

    def call(self, method, path, token=None, body=None, operation=None, expected=200):
        """One request, alternating between replicas. Returns (status, raw body, seconds)."""
        server = self.servers[self.turn % len(self.servers)]
        self.turn += 1
        headers = {'Content-Type': 'application/json'}
        if token:
            headers['Authorization'] = 'Bearer ' + token
        if operation:
            headers['Idempotency-Key'] = operation
        request = urllib.request.Request(server.base + path, method=method, headers=headers,
                                         data=None if body is None else json.dumps(body).encode())
        started = time.perf_counter()
        try:
            response = urllib.request.urlopen(request, timeout=60)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            raw = response.read()
            elapsed = time.perf_counter() - started
            if response.status != expected:
                raise RuntimeError(f'{method} {path}: expected {expected}, got {response.status}: {raw[:300]!r}')
            return response.status, raw, elapsed

    def json(self, *args, **kwargs):
        return json.loads(self.call(*args, **kwargs)[1] or b'null')


def account(binary, env, name):
    result = subprocess.run([str(binary), 'account', name], input=PASSWORD.encode(), env=env,
                            check=True, capture_output=True)
    return result.stdout.decode().strip()


def login(client, name, device):
    return client.json('POST', V1 + '/sessions', body={
        'username': name, 'password': PASSWORD, 'device_id': device})['access_token']


def head(client, token):
    page = client.json('GET', V1 + '/sync', token)
    while page['has_more']:
        page = client.json('GET', f"{V1}/sync?cursor={page['next_cursor']}", token)
    return page['next_cursor']


def share(reader):
    return {'grants': [{'kind': 'account', 'id': reader, 'edit': False}], 'exclude_accounts': []}


def person_version(client, token, person):
    return client.json('GET', f'{V1}/policies/{person}', token)['version']


def seed(client, owner, reader, workload, fields):
    """A person shared with the reader and `fields` dense checklist fields shared with them."""
    person = seeded(workload, 'person')
    client.json('POST', V1 + '/access-commands', owner, {'commands': [
        {'kind': 'create_person', 'id': person, 'name': f'Dense {workload}',
         'initial_policy': share(reader)}]}, operation=seeded(workload, 'op', 'person'))
    ids = []
    for i in range(fields):
        field = seeded(workload, 'field', i)
        client.json('POST', V1 + '/task-access-commands', owner, {'command': {
            'kind': 'put_field', 'id': field, 'parent_id': person, 'expected_version': None,
            'label': f'Dense {i}', 'value': checklist(), 'initial_policy': share(reader)}},
            operation=seeded(workload, 'op', 'field', i))
        ids.append(field)
    return person, ids


def access(client, owner, kind, person, reader, workload):
    version = person_version(client, owner, person)
    command = {'kind': kind, 'id': person, 'expected_version': version, 'account_id': reader}
    if kind == 'grant':
        command['edit'] = False
    client.json('POST', V1 + '/access-commands', owner, {'commands': [command]},
                operation=seeded(workload, 'op', kind, version))


def insert_synthetic(database, psql, reader, notices):
    payload = json.dumps([{'id': i, 'resource_kind': k} for i, k in notices], separators=(',', ':'))
    statements = (
        "UPDATE sync_clock SET revision=revision+1 WHERE id=1;"
        "INSERT INTO sync_batches(account_id,revision,payload,created_at) "
        "SELECT '{reader}',revision,'{payload}',{now} FROM sync_clock WHERE id=1;"
    ).format(reader=reader, payload=payload.replace("'", "''"), now=int(time.time()))
    if database.startswith('sqlite://'):
        path = database.removeprefix('sqlite://').split('?')[0]
        with sqlite3.connect(path, timeout=30) as connection:
            connection.executescript('BEGIN IMMEDIATE;' + statements + 'COMMIT;')
    else:
        subprocess.run([psql, '-v', 'ON_ERROR_STOP=1', '-q', '-d', database, '-c',
                        'BEGIN;' + statements + 'COMMIT;'], check=True, capture_output=True)


def drain(client, token, cursor, m, out, name, restart=None):
    """Drain from `cursor`, writing each body; returns per-request rows and the end cursor."""
    rows = []
    while True:
        query = f'?cursor={cursor}' + (f'&max_changes={m}' if m else '')
        status, raw, seconds = client.call('GET', V1 + '/sync' + query, token)
        page = json.loads(raw)
        index = len(rows)
        (out / f'{name}-{index:04d}.json').write_bytes(raw)
        (out / f'{name}-{index:04d}.normalised.json').write_bytes(normalised(raw))
        rows.append({'request': index, 'status': status, 'bytes': len(raw), 'nodes': nodes(page),
                     'changes': sum(len(b['changes']) for b in page['batches']),
                     'batches': len(page['batches']),
                     'segment': page['batches'][0].get('segment') if page['batches'] else None,
                     'seconds': round(seconds, 6)})
        if restart and page['has_more'] and index == 1:
            # R3: the saved cursor resumes identically after a restart.
            before = client.call('GET', V1 + '/sync?cursor=' + page['next_cursor']
                                 + (f'&max_changes={m}' if m else ''), token)[1]
            restart()
            after = client.call('GET', V1 + '/sync?cursor=' + page['next_cursor']
                                + (f'&max_changes={m}' if m else ''), token)[1]
            if normalised(before) != normalised(after):
                raise RuntimeError('a saved continuation did not resume identically after restart')
            rows[-1]['restart_resumed_identically'] = True
        cursor = page['next_cursor']
        if not page['has_more']:
            return rows, cursor


def totals(rows):
    return {'requests': len(rows), 'bytes': sum(r['bytes'] for r in rows),
            'nodes': sum(r['nodes'] for r in rows), 'changes': sum(r['changes'] for r in rows),
            'max_bytes': max(r['bytes'] for r in rows), 'max_nodes': max(r['nodes'] for r in rows),
            'max_changes_per_response': max(r['changes'] for r in rows),
            'seconds': round(sum(r['seconds'] for r in rows), 6),
            'max_seconds': max(r['seconds'] for r in rows)}


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--binary', type=Path, default=Path(os.environ.get('ATLAS_TEST_BINARY', ROOT / 'target/debug/atlas-server')))
    parser.add_argument('--database', help='an explicitly disposable PostgreSQL URL; default: a new SQLite file under $TMPDIR')
    parser.add_argument('--psql', default=shutil.which('psql') or '/opt/homebrew/opt/postgresql@17/bin/psql')
    parser.add_argument('--replicas', type=int, default=1, choices=[1, 2])
    parser.add_argument('--restart-between', action='store_true')
    parser.add_argument('--serve', action='store_true', help='keep the seeded server running until interrupted')
    args = parser.parse_args()
    if args.output.exists() and any(args.output.iterdir()):
        sys.exit(f'refusing a non-empty output directory: {args.output}')
    if args.replicas == 2 and not args.database:
        sys.exit('two replicas need a shared disposable PostgreSQL --database')
    if args.database and not args.database.startswith(('postgres://', 'postgresql://')):
        sys.exit('--database must be a disposable PostgreSQL URL; SQLite is always created here')
    args.output.mkdir(parents=True, exist_ok=True)
    bodies = args.output / 'bodies'
    bodies.mkdir()
    temporary = tempfile.mkdtemp(prefix='atlas-dense-fixture-', dir=os.environ.get('TMPDIR'))
    servers = []
    try:
        database = args.database or f'sqlite://{temporary}/dense.sqlite?mode=rwc'
        env = {**os.environ, 'ATLAS_DATABASE_URL': database, 'ATLAS_BIND': '127.0.0.1:0',
               'ATLAS_PUBLIC_ORIGIN': 'https://atlas.example'}
        owner_id, reader_id = account(args.binary, env, 'dense-owner'), account(args.binary, env, 'dense-reader')
        servers = [Server(args.binary, env, Path(temporary) / f'server-{i}.log') for i in range(args.replicas)]
        for server in servers:
            server.start()
        client = Client(servers)
        owner = login(client, 'dense-owner', 'owner')
        devices = {}

        def reader(device):
            if device not in devices:
                devices[device] = login(client, 'dense-reader', device)
            return devices[device]

        def restart():
            for server in servers:
                server.stop()
                server.start()

        manifest = {'label': LABEL, 'sweep': SWEEP, 'replicas': args.replicas,
                    'database': 'postgresql' if args.database else 'sqlite',
                    'architecture': platform.machine(), 'platform': platform.platform(),
                    'binary': str(args.binary), 'binary_sha256': sha256(args.binary),
                    'commit': subprocess.run(['git', '-C', str(ROOT), 'rev-parse', 'HEAD'],
                                             capture_output=True, text=True).stdout.strip(),
                    'workloads': {}}
        # W-real: hidden, then one grant.
        person, _ = seed(client, owner, reader_id, 'real', 199)
        access(client, owner, 'revoke', person, reader_id, 'real')
        starts = {m: head(client, reader(f'real-{m or "whole"}')) for m in [None] + SWEEP}
        access(client, owner, 'grant', person, reader_id, 'real')
        runs = {}
        for m in [None] + SWEEP:
            rows, _ = drain(client, reader(f'real-{m or "whole"}'), starts[m], m, bodies, f'real-{m or "whole"}',
                            restart if args.restart_between and m == 25 else None)
            runs[str(m or 'whole')] = {'rows': rows, 'totals': totals(rows)}
        manifest['workloads']['W-real'] = {'synthetic': False, 'description': '1 person + 199 dense checklist fields, one grant', 'runs': runs}
        # W-synthetic: 799 fields and their person in one inserted batch.
        person, fields = seed(client, owner, reader_id, 'synthetic', 799)
        starts = {m: head(client, reader(f'synthetic-{m or "whole"}')) for m in [None] + SWEEP}
        insert_synthetic(database, args.psql, reader_id, [(person, 'person')] + [(f, 'field') for f in fields])
        runs = {}
        for m in [None] + SWEEP:
            rows, _ = drain(client, reader(f'synthetic-{m or "whole"}'), starts[m], m, bodies, f'synthetic-{m or "whole"}')
            runs[str(m or 'whole')] = {'rows': rows, 'totals': totals(rows)}
        whole = runs['whole']['totals']
        manifest['workloads']['W-synthetic'] = {
            'synthetic': True, 'description': '799 dense checklist fields + their person, notice row inserted directly',
            'runs': runs,
            'brief_comparison': {'brief': BRIEF, 'observed_whole_batch': {'bytes': whole['bytes'], 'nodes': whole['nodes']},
                                 'explanation': 'The brief counted 800 dense fields; this control is 799 fields plus their '
                                                'person (the person is always sent as an ancestor) and carries real '
                                                'Projection wrappers, so bytes and nodes differ; not tuned to match.'}}
        manifest['files'] = {p.name: sha256(p) for p in sorted(bodies.iterdir())}
        (args.output / 'manifest.json').write_text(json.dumps(manifest, indent=2, sort_keys=True) + '\n')
        print(json.dumps({'label': LABEL, 'manifest': str(args.output / 'manifest.json'),
                          'manifest_sha256': sha256(args.output / 'manifest.json')}))
        if args.serve:
            print(json.dumps({'serving': [s.base for s in servers], 'reader': 'dense-reader',
                              'password': PASSWORD, 'note': 'interrupt to stop'}), flush=True)
            try:
                while all(s.process.poll() is None for s in servers):
                    time.sleep(1)
            except KeyboardInterrupt:
                pass
    finally:
        for server in servers:
            server.stop()
        shutil.rmtree(temporary, ignore_errors=True)


if __name__ == '__main__':
    main()
