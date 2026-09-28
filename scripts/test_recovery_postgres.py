#!/usr/bin/env python3
"""PostgreSQL recovery checks run by `scripts/test-postgres-release.sh` against its own disposable
cluster (PostgreSQL client tools on PATH; every listener is a Unix socket or loopback).

The release script keeps its own steps (two-process run, backup, restore, occupied-destination
refusal, SQL validity) and calls this between and after them:

  digest  URL OUT.json      per-table row counts and digests, plus timer state
  compare SOURCE RESTORED   what a restore must and must not have changed, with non-zero counts
  same    A B               two digests are identical (a refused restore changed nothing)
  serve   SOURCE RESTORED   real servers on both; the same reads; a new concurrent timer
  legacy  EXPORT            frozen-1003 bundles 1000..1003, a real startup upgrade, refusals

`ATLAS_PG_CLUSTER` is the cluster's socket directory. Nothing here touches another database."""
import getpass
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import uuid

from smoke import timers
from test_recovery import (ADJUSTED, DELETED, MUST_SURVIVE, ROOT, SERVER, Server, current_schema,
                           first_upgradable, reads)

FROZEN_1003 = ROOT / 'crates/core/tests/fixtures/schema_1003_postgres.sql'
# The accounts test_replicas.py creates and their shared password.
REPLICA_PASSWORD = 'replica-test-password-123'
CLUSTER = os.environ.get('ATLAS_PG_CLUSTER', '')


def url(database):
    return f'postgresql:///{database}?host={CLUSTER}&user={getpass.getuser()}'


def psql(target, sql, check=True):
    result = subprocess.run([shutil.which('psql'), target, '-XAt', '-v', 'ON_ERROR_STOP=1', '-c', sql], capture_output=True)
    if check and result.returncode:
        raise AssertionError(f'psql failed: {result.stderr.decode()[-500:]}')
    return result if not check else result.stdout.decode().strip()


def psql_file(target, path):
    subprocess.run([shutil.which('psql'), target, '-X', '-q', '-v', 'ON_ERROR_STOP=1', '-f', str(path)], check=True, capture_output=True)


def createdb(name):
    subprocess.run([shutil.which('createdb'), '-h', CLUSTER, name], check=True)
    return url(name)


def tool(command, target, *arguments, check=True):
    """scripts/backup.py against `target`, which it reads from ATLAS_DATABASE_URL."""
    env = {**os.environ, 'ATLAS_DATABASE_URL': target}
    env.pop('ATLAS_DATABASE_URL_FILE', None)
    result = subprocess.run([sys.executable, str(ROOT / 'scripts/backup.py'), command, *arguments, '--engine', 'postgres',
                             '--offline'] + (['--server', str(SERVER)] if command == 'restore' else []), env=env, capture_output=True)
    if check and result.returncode:
        raise AssertionError(f'{command} failed: {result.stderr.decode()[-500:]}')
    return result


def digest(target):
    tables = psql(target, "SELECT table_name FROM information_schema.tables WHERE table_schema='public' "
                          "AND table_type='BASE TABLE' AND table_name<>'atlas_schema' ORDER BY 1").splitlines()
    rows = {}
    for table in tables:
        # jsonb text is independent of column order and of how a dump wrote the row; an account's
        # access epoch is what a restore legitimately advances.
        row = "(to_jsonb(t) - 'access_epoch')" if table == 'accounts' else 'to_jsonb(t)'
        count, checksum = psql(target, f"SELECT count(*), md5(coalesce(string_agg({row}::text, E'\\n' ORDER BY {row}::text), '')) "
                                       f'FROM "{table}" t').split('|')
        rows[table] = [int(count), checksum]
    running, stopped = psql(target, "SELECT count(*) FILTER (WHERE stopped_at IS NULL AND cancelled=0), "
                                    "count(*) FILTER (WHERE stopped_at IS NOT NULL) FROM timer_sessions").split('|')
    return {'version': psql(target, 'SELECT version FROM atlas_schema'), 'tables': rows,
            'timers': {'running': int(running), 'stopped': int(stopped)}}


def indexes(target):
    return psql(target, "SELECT indexname||': '||indexdef FROM pg_indexes WHERE schemaname='public' ORDER BY 1").splitlines()


def shape(target):
    return psql(target, "SELECT table_name||'.'||column_name||' '||data_type||' '||is_nullable FROM information_schema.columns "
                        "WHERE table_schema='public' ORDER BY 1").splitlines()


def timer_indexes(target):
    return {line.split(':')[0] for line in indexes(target) if ' ON public.timer_sessions ' in line}


def assert_upgraded(target, schema):
    assert psql(target, 'SELECT version FROM atlas_schema') == str(schema)
    names = timer_indexes(target)
    assert 'timer_active_account' not in names, names
    assert {'timer_active_account_occurrence', 'timer_account_stopped'} <= names, names


def cmd_digest(target, out):
    Path(out).write_text(json.dumps(digest(target), indent=1, sort_keys=True))


def cmd_compare(source_path, restored_path):
    schema = current_schema()
    source, restored = (json.loads(Path(p).read_text()) for p in (source_path, restored_path))
    assert source['version'] == restored['version'] == str(schema), (source['version'], restored['version'], schema)
    assert set(source['tables']) == set(restored['tables'])
    for table in MUST_SURVIVE:
        assert source['tables'][table][0] > 0, f'{table} is empty in the source, so equality would prove nothing'
        assert restored['tables'][table] == source['tables'][table], f'{table} changed across the restore'
    changed = {t for t in source['tables'] if source['tables'][t] != restored['tables'][t]}
    assert changed <= DELETED | ADJUSTED, sorted(changed - (DELETED | ADJUSTED))
    for table in DELETED:
        assert restored['tables'][table][0] == 0, f'{table} should be reset by the restore'
    # Timer history in both states, and both durable sent histories, are present and equal.
    assert source['timers']['running'] >= 1 and source['timers']['stopped'] >= 2, source['timers']
    assert restored['timers'] == source['timers']
    print(json.dumps({'restore_preserved': {t: source['tables'][t][0] for t in MUST_SURVIVE}, 'timers': restored['timers'],
                      'reset_tables_changed': sorted(changed)}))


def cmd_same(first, second):
    assert json.loads(Path(first).read_text()) == json.loads(Path(second).read_text()), 'a refused restore changed the destination'


def cmd_serve(source, restored):
    tmp = Path(tempfile.mkdtemp(prefix='atlas-serve-'))
    seen = {}
    try:
        for name, target in (('source', source), ('restored', restored)):
            with Server(target, tmp / f'{name}.log') as server:
                server.call('GET', '/ready')
                token = server.login('alice', REPLICA_PASSWORD)
                seen[name] = reads(server, token)
                if name == 'restored':
                    running = {i['id'] for i in seen[name]['timers']['items'] if i['stopped_at'] is None}
                    assert running, 'the restored database holds a running timer'
                    # The restored database has the per-occurrence rule: a new timer starts on a fresh
                    # occurrence while the restored one still runs.
                    _, occurrence = timers.seconds_task(server.call, token, 'After restore')
                    timers.start(server.call, token, occurrence, str(uuid.uuid4()), int(time.time()) - 30)
                    after = {i['id'] for i in timers.listing(server.call, token, '?state=running&limit=200')['items']}
                    assert running < after and len(after) == len(running) + 1, (running, after)
        assert seen['source'] == seen['restored'], 'the restored server serves different history'
        assert seen['source']['timers']['items'] and seen['source']['requests']['items'] and seen['source']['invitations']['items']
        print(json.dumps({'served_after_restore': {k: len(v['items']) for k, v in seen['restored'].items()}}))
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def seed_fixture(target, history):
    ids = {name: str(uuid.uuid4()) for name in ('alice', 'bob', 'busy', 'idle', 'other', 'household')}
    statements = [f"INSERT INTO accounts(id,username,password_hash) VALUES ('{ids['alice']}','legacy-alice','x'),('{ids['bob']}','legacy-bob','x')",
                  "INSERT INTO resources(id,owner_id,parent_id,kind,label,value) VALUES "
                  f"('{ids['busy']}','{ids['alice']}',NULL,'person','stream','{{}}'),('{ids['idle']}','{ids['alice']}',NULL,'person','stream','{{}}'),"
                  f"('{ids['other']}','{ids['bob']}',NULL,'person','stream','{{}}')"]
    for progress, account, started, stopped, version, cancelled in (
            ('busy', 'alice', 100, 200, 2, 0), ('busy', 'alice', 300, None, 2, 1), ('idle', 'alice', 400, 500, 2, 0),
            ('busy', 'alice', 1000, None, 1, 0), ('other', 'bob', 1000, None, 1, 0)):
        statements.append('INSERT INTO timer_sessions(id,progress_id,account_id,started_at,stopped_at,version,cancelled) VALUES '
                          f"('{uuid.uuid4()}','{ids[progress]}','{ids[account]}',{started},{'NULL' if stopped is None else stopped},{version},{cancelled})")
    statements.append(f"INSERT INTO receipts(account_id,operation_id,payload,revision) VALUES ('{ids['alice']}','{uuid.uuid4()}','{{}}',1)")
    statements.append("INSERT INTO operation_outcomes(account_id,operation_id,kind,digest,outcome,created_at) VALUES "
                      f"('{ids['alice']}','{uuid.uuid4()}','revoke_session','d','confirmed_applied',1)")
    if history:
        statements.append("INSERT INTO people_request_history(id,sender_id,recipient_id,kind,payload,state,expires_at,updated_at) VALUES "
                          f"('{uuid.uuid4()}','{ids['alice']}','{ids['bob']}','link','{{}}','accepted',4102444800,1)")
        statements.append(f"INSERT INTO households(id,name) VALUES ('{ids['household']}','Home')")
        statements.append("INSERT INTO household_invitation_history(id,household_id,sender_id,recipient_id,status,expires_at,version,updated_at) VALUES "
                          f"('{uuid.uuid4()}','{ids['household']}','{ids['alice']}','{ids['bob']}','revoked',4102444800,2,1)")
    psql(target, ';\n'.join(statements))
    return ids


def older(target, version):
    steps = [(1002, ['DROP TABLE people_request_history', 'DROP TABLE household_invitation_history']),
             (1001, ['DROP TABLE activation_grants', 'ALTER TABLE oidc_flows DROP COLUMN attempt_id, DROP COLUMN attempt_challenge']),
             (1000, ['DROP TABLE operation_outcomes'])]
    for step, statements in steps:
        if version <= step:
            psql(target, ';\n'.join(statements + [f'UPDATE atlas_schema SET version={step}']))


def frozen(name):
    target = createdb(name)
    psql_file(target, FROZEN_1003)
    assert psql(target, 'SELECT version FROM atlas_schema') == '1003'
    return target


def cmd_legacy(export):
    schema = current_schema()
    export = Path(export)
    tmp = Path(tempfile.mkdtemp(prefix='atlas-legacy-'))
    try:
        # Bundles of every older schema, from the frozen 1003 baseline, restored with this executable.
        for version in range(first_upgradable(), schema):
            source = frozen(f'legacy_{version}')
            ids = seed_fixture(source, history=True)
            older(source, version)
            assert psql(source, 'SELECT version FROM atlas_schema') == str(version)
            before = digest(source)
            bundle = tmp / f'export-{version}'
            tool('backup', source, str(bundle))
            assert json.loads((bundle / 'manifest.json').read_text())['schema'] == version
            target = createdb(f'legacy_{version}_restored')
            tool('restore', target, str(bundle))
            assert_upgraded(target, schema)
            after = digest(target)
            for table, value in before['tables'].items():
                if table not in DELETED | ADJUSTED:
                    assert after['tables'][table] == value, f'{version}: {table} changed across restore and upgrade'
            assert after['tables']['timer_sessions'][0] == 5 and after['tables']['accounts'][0] == 2
            expected_history = 1 if version == 1003 else 0
            for table in ('people_request_history', 'household_invitation_history'):
                assert after['tables'][table][0] == expected_history, (version, table)
            # The upgraded database has the new rule; the old index is gone.
            psql(target, f"INSERT INTO timer_sessions(id,progress_id,account_id,started_at,version) VALUES ('{uuid.uuid4()}','{ids['idle']}','{ids['alice']}',5000,1)")
            duplicate = psql(target, f"INSERT INTO timer_sessions(id,progress_id,account_id,started_at,version) VALUES ('{uuid.uuid4()}','{ids['busy']}','{ids['alice']}',5000,1)", check=False)
            assert duplicate.returncode and b'duplicate key' in duplicate.stderr, duplicate.stderr

        # A real startup upgrade: a bundle of the real, populated database is restored and reversed
        # to exactly the frozen 1003 schema; a server started directly on it migrates in place.
        reference = frozen('frozen_1003_empty')
        for name, used in (('idle', False), ('direct', True)):
            target = createdb(f'{name}_1003')
            tool('restore', target, str(export))
            running = psql(target, "SELECT count(*) FROM (SELECT account_id FROM timer_sessions WHERE stopped_at IS NULL AND cancelled=0 "
                                   "GROUP BY account_id HAVING count(*)>1) d")
            assert running == '0', 'the 1003 index cannot hold two running timers for one account'
            psql(target, 'CREATE UNIQUE INDEX timer_active_account ON timer_sessions USING btree (account_id) WHERE ((stopped_at IS NULL) AND (cancelled = 0));\n'
                         'DROP INDEX timer_active_account_occurrence;\nDROP INDEX timer_account_stopped;\nUPDATE atlas_schema SET version=1003')
            assert indexes(target) == indexes(reference), 'the reversal is exactly the frozen 1003 index set'
            assert shape(target) == shape(reference)
            before = digest(target)
            with Server(target, tmp / f'{name}.log') as server:
                server.call('GET', '/ready')
                if used:
                    token = server.login('alice', REPLICA_PASSWORD)
                    mine = timers.listing(server.call, token, '?limit=200')['items']
                    assert mine and all(i['access'] == 'available' for i in mine), mine
                    still = [i for i in mine if i['stopped_at'] is None]
                    assert still, 'the running timer survived the upgrade'
                    _, occurrence = timers.seconds_task(server.call, token, 'After upgrade')
                    timers.start(server.call, token, occurrence, str(uuid.uuid4()), int(time.time()) - 20)
                    after = timers.listing(server.call, token, '?state=running&limit=200')['items']
                    assert len(after) == len(still) + 1, after
            assert_upgraded(target, schema)
            if not used:
                assert digest(target)['tables'] == before['tables'], 'the startup upgrade rewrites no row'

        # Unknown schemas are refused by the tool on both sides of the supported range, and a
        # forged manifest is refused at restore without touching the destination.
        for label, statement in (('below', 'UPDATE atlas_schema SET version=999'), ('above', f'UPDATE atlas_schema SET version={schema + 1}'),
                                 ('two_rows', 'INSERT INTO atlas_schema(version) VALUES (1000)')):
            scratch = frozen(f'unknown_{label}')
            psql(scratch, statement)
            refused = tool('backup', scratch, str(tmp / f'unknown-{label}'), check=False)
            assert refused.returncode and b'unsupported schema' in refused.stderr, (label, refused.stderr)
        manifest = json.loads((export / 'manifest.json').read_text())
        forged = tmp / 'forged'
        shutil.copytree(export, forged)
        for bad in (999, schema + 1):
            (forged / 'manifest.json').write_text(json.dumps({**manifest, 'schema': bad}))
            empty = createdb(f'forged_{bad}')
            assert tool('restore', empty, str(forged), check=False).returncode != 0
            assert psql(empty, "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public'") == '0'

        # An occupied destination is refused and its data is untouched.
        occupied = frozen('occupied')
        seed_fixture(occupied, history=True)
        before = digest(occupied)
        assert tool('restore', occupied, str(export), check=False).returncode != 0
        assert digest(occupied) == before
        print(json.dumps({'legacy_bundles_upgraded': list(range(first_upgradable(), schema)), 'startup_upgrade': 'idle and used',
                          'refusals': ['unknown_below', 'unknown_above', 'two_rows', 'forged', 'occupied']}))
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def main():
    assert CLUSTER, 'ATLAS_PG_CLUSTER must name the disposable cluster socket directory'
    command, *arguments = sys.argv[1:]
    {'digest': cmd_digest, 'compare': cmd_compare, 'same': cmd_same, 'serve': cmd_serve, 'legacy': cmd_legacy}[command](*arguments)


if __name__ == '__main__':
    main()
