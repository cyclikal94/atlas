#!/usr/bin/env python3
"""Versioned, same-engine Atlas exports. Run with every application replica stopped."""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import urllib.parse

FORMAT = 1


def run(command, env=None):
    # Database tools can echo credentials in errors. Keep their output private.
    result = subprocess.run(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if result.returncode:
        raise ValueError(f"{Path(command[0]).name} failed; check connectivity, permissions and tool version")
    return result.stdout


def digest(path):
    with path.open('rb') as source:
        value = hashlib.sha256()
        for block in iter(lambda: source.read(1024 * 1024), b''):
            value.update(block)
    return value.hexdigest()


def pg_env():
    env = os.environ.copy()
    # libpq environment avoids putting a connection string in process arguments.
    url = env.get('ATLAS_DATABASE_URL')
    if env.get('ATLAS_DATABASE_URL_FILE'):
        if url:
            raise ValueError('set ATLAS_DATABASE_URL or ATLAS_DATABASE_URL_FILE, not both')
        url = Path(env['ATLAS_DATABASE_URL_FILE']).read_text().rstrip('\r\n')
    if not url or not url.startswith(('postgres://', 'postgresql://')):
        raise ValueError('PostgreSQL requires ATLAS_DATABASE_URL or ATLAS_DATABASE_URL_FILE')
    parsed = urllib.parse.urlsplit(url)
    for name, value in [('PGHOST', parsed.hostname), ('PGPORT', parsed.port),
                        ('PGUSER', parsed.username), ('PGPASSWORD', parsed.password)]:
        if value is not None:
            env[name] = urllib.parse.unquote(str(value))
    env['PGDATABASE'] = urllib.parse.unquote(parsed.path.lstrip('/'))
    options = {'host': 'PGHOST', 'port': 'PGPORT', 'user': 'PGUSER',
               'password': 'PGPASSWORD', 'dbname': 'PGDATABASE',
               'sslmode': 'PGSSLMODE', 'sslrootcert': 'PGSSLROOTCERT',
               'sslcert': 'PGSSLCERT', 'sslkey': 'PGSSLKEY',
               'connect_timeout': 'PGCONNECT_TIMEOUT', 'options': 'PGOPTIONS',
               'application_name': 'PGAPPNAME'}
    for key, value in urllib.parse.parse_qsl(parsed.query):
        if key not in options:
            raise ValueError('unsupported backup connection option: ' + key)
        env[options[key]] = value
    return env


def backup(args):
    # An exclusive directory means failures cannot replace an existing backup.
    args.bundle.mkdir(mode=0o700)
    payload = args.bundle / 'database'
    if args.engine == 'sqlite':
        if not args.sqlite or not args.sqlite.is_file():
            raise ValueError('--sqlite must name an existing database')
        with sqlite3.connect(args.sqlite.resolve().as_uri() + '?mode=ro', uri=True) as source:
            if source.execute('SELECT version FROM atlas_schema').fetchall() != [(1000,)]:
                raise ValueError('unsupported schema')
            with sqlite3.connect(payload) as target:
                source.backup(target)
    else:
        env = pg_env()
        version = run(['psql', '-XAt', '-v', 'ON_ERROR_STOP=1', '-c', 'SELECT version FROM atlas_schema'], env)
        if version.strip() != b'1000':
            raise ValueError('unsupported schema')
        run(['pg_dump', '--format=custom', '--no-owner', '--no-acl', '--file', str(payload)], env)
    files = {'database': digest(payload)}
    if args.secrets_file:
        shutil.copyfile(args.secrets_file, args.bundle / 'secrets.env')
        files['secrets.env'] = digest(args.bundle / 'secrets.env')
    manifest = {'format': FORMAT, 'schema': 1000, 'engine': args.engine,
                'created_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
                'files': files}
    for name in files:
        (args.bundle / name).chmod(0o600)
    manifest_path = args.bundle / 'manifest.json'
    manifest_path.write_text(json.dumps(manifest, indent=2) + '\n')
    manifest_path.chmod(0o600)


def restore(args):
    manifest = json.loads((args.bundle / 'manifest.json').read_text())
    if manifest.get('format') != FORMAT or manifest.get('schema') != 1000:
        raise ValueError('unsupported backup format/schema')
    if manifest.get('engine') != args.engine:
        raise ValueError('cross-engine restore is not supported')
    files = manifest.get('files', {})
    if set(files) not in ({'database'}, {'database', 'secrets.env'}):
        raise ValueError('invalid backup file list')
    for name, expected in files.items():
        path = args.bundle / name
        if path.is_symlink() or digest(path) != expected:
            raise ValueError(f'backup integrity check failed: {name}')
    env = os.environ.copy()
    payload = args.bundle / 'database'
    if args.engine == 'sqlite':
        if not args.sqlite:
            raise ValueError('--sqlite must name a new database file')
        # Exclusive create; never overwrite a database (including an empty file).
        with args.sqlite.open('xb') as target, payload.open('rb') as source:
            os.chmod(args.sqlite, 0o600)
            shutil.copyfileobj(source, target)
        env.pop('ATLAS_DATABASE_URL_FILE', None)
        env['ATLAS_DATABASE_URL'] = 'sqlite://' + str(args.sqlite.resolve()) + '?mode=rw'
    else:
        env = pg_env()
        count = run(['psql', '-XAt', '-v', 'ON_ERROR_STOP=1', '-c',
                     "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname !~ '^pg_' AND n.nspname <> 'information_schema' AND c.relkind IN ('r','v','m','S','f')"], env)
        if count.strip() != b'0':
            raise ValueError('restore requires an empty dedicated PostgreSQL database')
        run(['pg_restore', '--exit-on-error', '--single-transaction', '--no-owner', '--no-acl',
             '--dbname', '', str(payload)], env)
    run([args.server, 'prepare-restore', '--offline'], env)
    print('Restore complete. Restore integration keys separately; log in again and fetch a fresh sync snapshot.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation', choices=['backup', 'restore'])
    parser.add_argument('bundle', type=Path)
    parser.add_argument('--engine', choices=['sqlite', 'postgres'], required=True)
    parser.add_argument('--sqlite', type=Path, help='source database for backup; new destination for restore')
    parser.add_argument('--secrets-file', type=Path, help='optionally include an operator-managed secrets env file')
    parser.add_argument('--server', default='atlas-server', help='matching Atlas executable for restore preparation')
    parser.add_argument('--offline', action='store_true', required=True, help='confirm all Atlas replicas are stopped')
    args = parser.parse_args()
    if args.operation == 'restore' and args.secrets_file:
        parser.error('--secrets-file is a backup option; restore keys through your secret manager')
    if args.engine == 'postgres' and args.sqlite:
        parser.error('--sqlite only applies to the SQLite engine')
    os.umask(0o077)
    try:
        (backup if args.operation == 'backup' else restore)(args)
    except (ValueError, OSError, sqlite3.Error, KeyError) as error:
        print(f'Recovery failed: {error}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
