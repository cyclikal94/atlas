#!/usr/bin/env python3
"""Real versioned exports and restores of SQLite databases, at the current schema and at every
previous schema the tool accepts.

A current-schema database is populated through a real server process (the smoke scenarios), backed
up and restored with `scripts/backup.py`, and then a real server is started on the restored file.
Row-level digests prove what survived, so equality is never vacuous. Older bundles are built from
the frozen 1003 schema fixture and upgraded in place by the restore. Requires the contract
requirements (`scripts/requirements-contract.txt`), like the smoke run."""
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import sys
import tempfile
import time
import uuid

from smoke.client import ContractClient
from smoke import sharing, households, tasks, people, devices, timers

ROOT = Path(__file__).resolve().parents[1]
SERVER = Path(os.environ.get('ATLAS_TEST_BINARY', ROOT / 'target/debug/atlas-server'))
V1 = '/api/experimental/v1'
PASSWORD = 'recovery-scenario-password-123'
FROZEN_1003 = ROOT / 'crates/core/tests/fixtures/schema_1003_sqlite.sql'
# The tables a restore deliberately resets (`Store::prepare_restored_database`), and the ones it
# adjusts in place. Everything else must survive byte-for-byte.
DELETED = {'sync_cursors', 'sync_snapshots', 'sync_devices', 'sessions', 'oidc_flows',
           'native_handoffs', 'activation_grants',
           # Cleared by cascade with the sync snapshots and devices they belong to.
           'snapshot_items', 'sync_deliveries'}
ADJUSTED = {'accounts', 'account_invitations', 'calendar_sources', 'reminder_deliveries'}
# Durable tables whose survival the recovery acceptance names: timer history (running and
# stopped), both durable sent histories, the retirement ledger and receipts, and the content the
# timers point at. Each must hold rows in the seeded database, so equality is not vacuous.
MUST_SURVIVE = ['timer_sessions', 'people_request_history', 'household_invitation_history',
                'operation_outcomes', 'receipts', 'resources', 'occurrence_participants',
                'progress_entries']


def current_schema():
    """The schema the server writes, read from its source so this script cannot go stale."""
    source = (ROOT / 'crates/core/src/storage/mod.rs').read_text()
    return int(re.search(r'const SCHEMA_VERSION: i64 = (\d+);', source).group(1))


def first_upgradable():
    """The oldest schema the server can upgrade in place: the first `from` in its UPGRADES chain."""
    source = (ROOT / 'crates/core/src/storage/mod.rs').read_text()
    return int(re.search(r'from: (\d+),', source).group(1))


def copy(source, destination):
    """SQLite's backup API, not a file copy: the source may be in WAL mode."""
    with sqlite3.connect(source) as origin, sqlite3.connect(destination) as target:
        origin.backup(target)


def run_tool(base, *arguments, check=True):
    result = subprocess.run(base + list(arguments), capture_output=True)
    if check and result.returncode:
        raise AssertionError(f'{arguments[0]} failed: {result.stderr.decode()[-400:]}')
    return result


class Server:
    """A real atlas-server process on a database, on a loopback port it chooses. `database` is a
    SQLite file path or, for PostgreSQL, a connection URL."""

    def __init__(self, database, log):
        url = database if str(database).startswith(('postgres://', 'postgresql://')) else f'sqlite://{database}?mode=rwc'
        self.env = {**os.environ, 'ATLAS_DATABASE_URL': url,
                    'ATLAS_BIND': '127.0.0.1:0', 'ATLAS_PUBLIC_ORIGIN': 'https://atlas.example'}
        self.env.pop('ATLAS_DATABASE_URL_FILE', None)
        self.log = Path(log)

    def __enter__(self):
        self.output = self.log.open('w')
        self.process = subprocess.Popen([str(SERVER)], env=self.env, stdout=subprocess.DEVNULL,
                                        stderr=self.output)
        deadline = time.monotonic() + 20
        while '\n' not in self.log.read_text():
            if self.process.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError('Server failed to start: ' + self.log.read_text())
            time.sleep(0.02)
        self.base = 'http://' + json.loads(self.log.read_text().splitlines()[0])['address']
        self.document = json.loads((ROOT / 'api/openapi.json').read_text())
        self.call = ContractClient(self.base, self.document, set())
        return self

    def __exit__(self, *_):
        self.process.terminate()
        try:
            self.process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        self.output.close()
        assert self.process.returncode == 0, 'server did not shut down cleanly'

    def login(self, name, password=PASSWORD):
        return self.call('POST', V1 + '/sessions', body={
            'username': name, 'password': password, 'device_id': 'recovery-phone'})['access_token']


def seed(server, database):
    """Populate the database the way the smoke run does, through the real server, and keep the
    reads the restored copy must reproduce. One timer is left running."""
    call = server.call
    accounts = {}
    for name in ('alice', 'bob', 'carol'):
        result = subprocess.run([str(SERVER), 'account', name], input=PASSWORD.encode(), env=server.env,
                                check=True, capture_output=True)
        accounts[name] = result.stdout.decode().strip()
    tokens = {name: server.login(name) for name in accounts}
    secrets = [PASSWORD, *tokens.values()]
    person, field = sharing.run(call, accounts, tokens, secrets)
    lanes = [ContractClient(server.base, server.document, set()) for _ in range(4)]
    households.run(call, accounts, tokens, secrets, person, field, lanes)
    workflow = tasks.run(call, accounts, tokens, server.document)
    people.run(call, accounts, tokens, workflow)
    timers.run(call, accounts, tokens, server.document, leave_running=True)

    def fixture_rows(account, device, kind):
        """The rows devices.run needs where no OpenID provider or push endpoint exists."""
        expires = int(time.time()) + 3600
        with sqlite3.connect(database, timeout=10) as db:
            if kind == 'handoff':
                db.execute('INSERT INTO native_handoffs(code_hash,account_id,device_id,challenge,'
                           'configuration_hash,redirect_uri,expires_at) VALUES (?,?,?,?,?,?,?)',
                           (str(uuid.uuid4()), account, device, 'c', 'h', 'r', expires))
            elif kind == 'grant':
                db.execute('INSERT INTO activation_grants(grant_hash,grant_id,account_id,device_id,'
                           'auth_kind,challenge_hash,state,failed_verifiers,created_at,expires_at) '
                           "VALUES (?,?,?,?,'local',?,'issued',0,?,?)",
                           (str(uuid.uuid4()), str(uuid.uuid4()), account, device, str(uuid.uuid4()),
                            int(time.time()), expires))
            else:
                db.execute('INSERT INTO notification_subscriptions(id,account_id,device_id,transport,'
                           'secret,version,active) VALUES (?,?,?,?,?,1,1)',
                           (str(uuid.uuid4()), account, device, 'ntfy', 'fixture'))
    devices.run(call, tokens, secrets, fixture_rows)
    return accounts, reads(server, server.login('alice'))


def reads(server, token):
    """The account's timer list and both sent histories: what a client sees after a restore."""
    call = server.call
    return {
        'timers': call('GET', V1 + '/timer-sessions?limit=200', token),
        'requests': call('GET', V1 + '/people/requests/sent?limit=200', token),
        'invitations': call('GET', V1 + '/invitations/sent?limit=200', token),
    }


def digests(path):
    """Row count and digest of every table, with the columns a restore legitimately changes
    (an account's access epoch) left out. Read-only, so it works on a file no process holds."""
    with sqlite3.connect(Path(path).resolve().as_uri() + '?mode=ro', uri=True) as db:
        tables = [row[0] for row in db.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' "
            "AND name<>'atlas_schema' ORDER BY name")]
        result = {}
        for table in tables:
            columns = [row[1] for row in db.execute(f'PRAGMA table_info("{table}")')]
            if table == 'accounts':
                columns.remove('access_epoch')
            rows = sorted(repr(row) for row in db.execute(
                'SELECT ' + ','.join(f'"{column}"' for column in columns) + f' FROM "{table}"'))
            result[table] = (len(rows), hashlib.sha256('\n'.join(rows).encode()).hexdigest())
        return result


def schema_of(path):
    """Every object definition, so two databases are compared on what they contain, not on how
    they were built."""
    with sqlite3.connect(Path(path).resolve().as_uri() + '?mode=ro', uri=True) as db:
        return sorted(db.execute("SELECT type,name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' "
                                 "AND name<>'atlas_schema'").fetchall())


def version_of(path):
    with sqlite3.connect(path) as db:
        return db.execute('SELECT version FROM atlas_schema').fetchall()


def index_names(path):
    with sqlite3.connect(path) as db:
        return {row[0] for row in db.execute("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='timer_sessions'")}


def assert_upgraded(path, expected):
    assert version_of(path) == [(expected,)], version_of(path)
    names = index_names(path)
    assert 'timer_active_account' not in names, names
    assert {'timer_active_account_occurrence', 'timer_account_stopped'} <= names, names


def downgrade_to_1003(path):
    """Reverse the 1003 -> 1004 step exactly (it only swaps indexes). Only valid while no account
    has two running timers, which the 1003 schema could not hold."""
    with sqlite3.connect(path) as db:
        running = db.execute('SELECT account_id,count(*) FROM timer_sessions WHERE stopped_at IS NULL '
                             'AND cancelled=0 GROUP BY account_id HAVING count(*)>1').fetchall()
        assert not running, running
        db.execute('CREATE UNIQUE INDEX timer_active_account ON timer_sessions(account_id) WHERE stopped_at IS NULL AND cancelled=0')
        db.execute('DROP INDEX timer_active_account_occurrence')
        db.execute('DROP INDEX timer_account_stopped')
        db.execute('UPDATE atlas_schema SET version=1003')


def frozen_1003(path):
    """An empty database exactly as the 1003 release created it."""
    with sqlite3.connect(path) as db:
        db.executescript(FROZEN_1003.read_text())


def older(path, version):
    """Step a 1003 database back to `version` by removing what each later step added, the same
    way the Rust migration tests derive their baselines."""
    steps = [
        (1002, ['DROP TABLE people_request_history', 'DROP TABLE household_invitation_history']),
        (1001, ['DROP TABLE activation_grants', 'ALTER TABLE oidc_flows DROP COLUMN attempt_id',
                'ALTER TABLE oidc_flows DROP COLUMN attempt_challenge']),
        (1000, ['DROP TABLE operation_outcomes']),
    ]
    with sqlite3.connect(path) as db:
        for target, statements in steps:
            if version <= target:
                for statement in statements:
                    db.execute(statement)
                db.execute(f'UPDATE atlas_schema SET version={target}')


def seed_fixture(path, history):
    """Rows by raw SQL in a frozen-1003 database: accounts, timer history in every state (the
    single running row the 1003 index allows per account), receipts and, from 1003, both histories."""
    ids = {name: str(uuid.uuid4()) for name in ('alice', 'bob', 'busy', 'idle', 'other', 'household')}
    with sqlite3.connect(path) as db:
        db.execute('PRAGMA foreign_keys=ON')
        for name in ('alice', 'bob'):
            db.execute('INSERT INTO accounts(id,username,password_hash) VALUES (?,?,?)', (ids[name], 'legacy-' + name, 'x'))
        for key, owner in (('busy', 'alice'), ('idle', 'alice'), ('other', 'bob')):
            db.execute("INSERT INTO resources(id,owner_id,parent_id,kind,label,value) VALUES (?,?,NULL,'person','stream','{}')",
                       (ids[key], ids[owner]))
        for progress, account, started, stopped, version, cancelled in (
                ('busy', 'alice', 100, 200, 2, 0), ('busy', 'alice', 300, None, 2, 1),
                ('idle', 'alice', 400, 500, 2, 0), ('busy', 'alice', 1000, None, 1, 0),
                ('other', 'bob', 1000, None, 1, 0)):
            db.execute('INSERT INTO timer_sessions(id,progress_id,account_id,started_at,stopped_at,version,cancelled) VALUES (?,?,?,?,?,?,?)',
                       (str(uuid.uuid4()), ids[progress], ids[account], started, stopped, version, cancelled))
        db.execute("INSERT INTO receipts(account_id,operation_id,payload,revision) VALUES (?,?,'{}',1)", (ids['alice'], str(uuid.uuid4())))
        db.execute("INSERT INTO operation_outcomes(account_id,operation_id,kind,digest,outcome,created_at) VALUES (?,?,'revoke_session','d','confirmed_applied',1)",
                   (ids['alice'], str(uuid.uuid4())))
        if history:
            db.execute("INSERT INTO people_request_history(id,sender_id,recipient_id,kind,payload,state,expires_at,updated_at) VALUES (?,?,?,'link','{}','accepted',4102444800,1)",
                       (str(uuid.uuid4()), ids['alice'], ids['bob']))
            db.execute("INSERT INTO households(id,name) VALUES (?,'Home')", (ids['household'],))
            db.execute("INSERT INTO household_invitation_history(id,household_id,sender_id,recipient_id,status,expires_at,version,updated_at) VALUES (?,?,?,?,'revoked',4102444800,2,1)",
                       (str(uuid.uuid4()), ids['household'], ids['alice'], ids['bob']))
    return ids


def main():
    schema = current_schema()
    assert schema >= 1004, 'this script covers the 1004 timer index step and later'
    with tempfile.TemporaryDirectory(prefix='atlas-recovery-') as temp:
        root = Path(temp)
        original, restored = root / 'original.sqlite', root / 'restored.sqlite'
        env = os.environ.copy()
        env.pop('ATLAS_DATABASE_URL_FILE', None)
        env['ATLAS_DATABASE_URL'] = f'sqlite://{original}?mode=rwc'
        subprocess.run([str(SERVER), 'account', 'recovery'], input=b'recovery-test-password', env=env, check=True, stdout=subprocess.DEVNULL)
        url_file = root / 'database-url'
        url_file.write_text(env.pop('ATLAS_DATABASE_URL') + '\n')
        env['ATLAS_DATABASE_URL_FILE'] = str(url_file)
        subprocess.run([str(SERVER), 'password', 'recovery'], input=b'recovery-test-password', env=env, check=True, stdout=subprocess.DEVNULL)
        ambiguous = {**env, 'ATLAS_DATABASE_URL': 'sqlite://must-not-be-created.sqlite?mode=rwc'}
        assert subprocess.run([str(SERVER), 'invite'], env=ambiguous, capture_output=True).returncode != 0
        assert not Path('must-not-be-created.sqlite').exists()
        assert version_of(original) == [(schema,)], 'a new database records the current schema'

        # Populate the same database through a real server, then stop it: exports need every replica stopped.
        with Server(original, root / 'seed.log') as server:
            accounts, source_reads = seed(server, original)
        source = digests(original)
        for table in MUST_SURVIVE:
            assert source[table][0] > 0, f'{table} is empty in the seeded database, so equality would prove nothing'
        with sqlite3.connect(original) as db:
            assert db.execute('SELECT count(*) FROM timer_sessions WHERE stopped_at IS NULL AND cancelled=0').fetchone()[0] >= 1
            assert db.execute('SELECT count(*) FROM timer_sessions WHERE stopped_at IS NOT NULL').fetchone()[0] >= 2
        assert any(item['stopped_at'] is None for item in source_reads['timers']['items'])

        secret = root / 'operator.env'
        secret.write_text('ATLAS_SECRET_KEY=' + 'ab' * 32 + '\n')
        bundle = root / 'export'
        base = [sys.executable, str(ROOT / 'scripts/backup.py')]
        run_tool(base, 'backup', str(bundle), '--engine', 'sqlite', '--sqlite', str(original), '--secrets-file', str(secret), '--offline')
        assert bundle.stat().st_mode & 0o777 == 0o700
        assert (bundle / 'secrets.env').stat().st_mode & 0o777 == 0o600
        # The bundle records the schema it holds: the current one, which the tool must accept.
        assert json.loads((bundle / 'manifest.json').read_text())['schema'] == schema
        command = base + ['restore', str(bundle), '--engine', 'sqlite', '--sqlite', str(restored), '--server', str(SERVER), '--offline']
        subprocess.run(command, check=True)

        # Everything durable survived; only the documented reset differs.
        after = digests(restored)
        assert set(after) == set(source), set(after) ^ set(source)
        changed = {table for table in source if source[table] != after[table]}
        assert changed <= DELETED | ADJUSTED, sorted(changed - (DELETED | ADJUSTED))
        for table in MUST_SURVIVE:
            assert after[table] == source[table], f'{table} changed across the restore'
        for table in DELETED:
            assert after[table][0] == 0, f'{table} should be reset'
        with sqlite3.connect(original) as before, sqlite3.connect(restored) as db:
            epochs = dict(before.execute('SELECT username, access_epoch FROM accounts'))
            assert dict(db.execute('SELECT username, access_epoch FROM accounts')) == {n: e + 1 for n, e in epochs.items()}
            assert epochs.keys() >= {'recovery', 'alice', 'bob', 'carol'}
        assert version_of(restored) == [(schema,)]
        assert_upgraded(restored, schema)

        # A real server starts on the restored file, serves the same history and timers, and accepts
        # a new concurrent timer while the restored running one still runs (the new index is live).
        with Server(restored, root / 'restored.log') as server:
            server.call('GET', '/ready')
            token = server.login('alice')
            assert reads(server, token) == source_reads
            running = [i for i in source_reads['timers']['items'] if i['stopped_at'] is None]
            _, occurrence = timers.seconds_task(server.call, token, 'Restored')
            timers.start(server.call, token, occurrence, str(uuid.uuid4()), int(time.time()) - 30)
            now_running = [i for i in timers.listing(server.call, token, '?state=running&limit=200')['items']]
            assert {i['id'] for i in running} < {i['id'] for i in now_running}
            assert timers.listing(server.call, token, '?state=running&limit=200')['items'][0]['access'] == 'available'

        # The destination is never overwritten: a second restore, an existing empty file and an
        # existing populated file are all refused, leaving the destination exactly as it was.
        before = restored.read_bytes()
        assert subprocess.run(command, capture_output=True).returncode != 0
        assert restored.read_bytes() == before
        empty = root / 'empty.sqlite'
        empty.touch()
        assert run_tool(base, 'restore', str(bundle), '--engine', 'sqlite', '--sqlite', str(empty), '--server', str(SERVER), '--offline', check=False).returncode != 0
        assert empty.read_bytes() == b''
        populated = root / 'populated.sqlite'
        populated.write_bytes(b'not an atlas database, but not empty either')
        assert run_tool(base, 'restore', str(bundle), '--engine', 'sqlite', '--sqlite', str(populated), '--server', str(SERVER), '--offline', check=False).returncode != 0
        assert populated.read_bytes() == b'not an atlas database, but not empty either'
        assert not (root / 'empty.sqlite-wal').exists() and not (root / 'populated.sqlite-wal').exists()

        # A corrupted bundle is refused and the existing restore is untouched.
        with (bundle / 'database').open('ab') as out:
            out.write(b'corruption')
        assert subprocess.run(command, capture_output=True).returncode != 0
        assert restored.read_bytes() == before
        assert run_tool(base, 'restore', str(bundle), '--engine', 'postgres', '--offline', check=False).returncode != 0

        # The genuine 1003 schema. The database seeded above holds real rows in real occurrences;
        # reversing the index-only 1004 step must give exactly the frozen 1003 schema, so it is a
        # faithful "back up before upgrading" source and the reversal is proven, not assumed.
        frozen_empty = root / 'frozen-1003.sqlite'
        frozen_1003(frozen_empty)
        assert version_of(frozen_empty) == [(1003,)]
        assert 'timer_active_account' in index_names(frozen_empty)
        real_1003 = root / 'real-1003.sqlite'
        copy(original, real_1003)
        downgrade_to_1003(real_1003)
        assert schema_of(real_1003) == schema_of(frozen_empty), 'the reversal is exactly the frozen 1003 schema'
        pre_upgrade = digests(real_1003)

        # Real startup upgrade: a server started directly on the 1003 database migrates it in place
        # (the path an operator hits after installing the new binary). Started and stopped with no
        # request, it changes nothing but the schema; started and used, it serves the old rows and
        # accepts a second concurrent timer at once.
        idle = root / 'idle-1003.sqlite'
        copy(real_1003, idle)
        assert version_of(idle) == [(1003,)]
        with Server(idle, root / 'idle.log') as server:
            server.call('GET', '/ready')
        assert_upgraded(idle, schema)
        assert digests(idle) == pre_upgrade, 'the startup upgrade rewrites no row'
        direct = root / 'direct-1003.sqlite'
        copy(real_1003, direct)
        with Server(direct, root / 'direct.log') as server:
            token = server.login('alice')
            mine = timers.listing(server.call, token, '?limit=200')['items']
            assert mine and all(item['access'] == 'available' for item in mine), mine
            still_running = [i for i in mine if i['stopped_at'] is None]
            assert still_running, 'the running timer survived the upgrade'
            _, occurrence = timers.seconds_task(server.call, token, 'After upgrade')
            timers.start(server.call, token, occurrence, str(uuid.uuid4()), int(time.time()) - 20)
            after = timers.listing(server.call, token, '?state=running&limit=200')['items']
            assert len(after) == len(still_running) + 1, after
        assert_upgraded(direct, schema)

        # Every bundle the tool accepts, restored with this executable: each older schema is a
        # valid "back up before upgrading" source, upgraded in place with its rows preserved.
        for version in range(first_upgradable(), schema):
            legacy = root / f'legacy-{version}.sqlite'
            frozen_1003(legacy)
            ids = seed_fixture(legacy, history=True)
            older(legacy, version)
            assert version_of(legacy) == [(version,)]
            expected = digests(legacy)
            legacy_bundle, legacy_restored = root / f'legacy-{version}-export', root / f'legacy-{version}-restored.sqlite'
            run_tool(base, 'backup', str(legacy_bundle), '--engine', 'sqlite', '--sqlite', str(legacy), '--offline')
            assert json.loads((legacy_bundle / 'manifest.json').read_text())['schema'] == version
            run_tool(base, 'restore', str(legacy_bundle), '--engine', 'sqlite', '--sqlite', str(legacy_restored), '--server', str(SERVER), '--offline')
            assert_upgraded(legacy_restored, schema)
            got = digests(legacy_restored)
            for table, value in expected.items():
                if table not in DELETED | ADJUSTED:
                    assert got[table] == value, f'{version}: {table} changed across restore and upgrade'
            with sqlite3.connect(legacy_restored) as db:
                assert db.execute('SELECT count(*) FROM timer_sessions').fetchone()[0] == 5
                assert db.execute('SELECT count(*) FROM accounts').fetchone()[0] == 2
                # Both histories exist after the upgrade; the 1003 bundle carried its rows through.
                expected_history = 1 if version == 1003 else 0
                assert db.execute('SELECT count(*) FROM people_request_history').fetchone()[0] == expected_history
                assert db.execute('SELECT count(*) FROM household_invitation_history').fetchone()[0] == expected_history
                # The upgraded database has the new rule: a second running timer on another stream.
                db.execute('PRAGMA foreign_keys=ON')
                db.execute('INSERT INTO timer_sessions(id,progress_id,account_id,started_at,version) VALUES (?,?,?,5000,1)',
                           (str(uuid.uuid4()), ids['idle'], ids['alice']))
                try:
                    db.execute('INSERT INTO timer_sessions(id,progress_id,account_id,started_at,version) VALUES (?,?,?,5000,1)',
                               (str(uuid.uuid4()), ids['busy'], ids['alice']))
                except sqlite3.IntegrityError as error:
                    assert 'UNIQUE' in str(error)
                else:
                    raise AssertionError('a second running timer on one occurrence must be refused')

        # Unknown baselines are refused on both sides of the supported range.
        for unknown_version in (999, schema + 1):
            unknown = root / f'unknown-{unknown_version}.sqlite'
            copy(original, unknown)
            with sqlite3.connect(unknown) as db:
                db.execute(f'UPDATE atlas_schema SET version={unknown_version}')
            refused = run_tool(base, 'backup', str(root / f'unknown-{unknown_version}-export'), '--engine', 'sqlite',
                               '--sqlite', str(unknown), '--offline', check=False)
            assert refused.returncode != 0 and b'unsupported schema' in refused.stderr, refused.stderr
        # A bundle whose manifest claims an unknown schema is refused at restore too.
        forged = json.loads((bundle / 'manifest.json').read_text())
        forged_bundle = root / 'forged-export'
        forged_bundle.mkdir()
        for name in ('database', 'secrets.env'):
            (forged_bundle / name).write_bytes((bundle / name).read_bytes())
        for bad in (999, schema + 1):
            (forged_bundle / 'manifest.json').write_text(json.dumps({**forged, 'schema': bad}))
            target = root / f'forged-{bad}.sqlite'
            assert run_tool(base, 'restore', str(forged_bundle), '--engine', 'sqlite', '--sqlite', str(target), '--server', str(SERVER), '--offline', check=False).returncode != 0
            assert not target.exists()
    print(f'Recovery checks passed: schema {schema}, real server round trip, bundles {first_upgradable()}..{schema - 1} upgraded, refusals')


if __name__ == '__main__':
    main()
