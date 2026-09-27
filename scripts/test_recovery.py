#!/usr/bin/env python3
"""Exercise real versioned exports, corruption rejection and non-destructive restore."""
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
SERVER = ROOT / 'target/debug/atlas-server'


def copy_database(source, destination):
    # SQLite's backup API, not a file copy: the source is in WAL mode.
    with sqlite3.connect(source) as origin, sqlite3.connect(destination) as target:
        origin.backup(target)


def main():
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
        secret = root / 'operator.env'
        secret.write_text('ATLAS_SECRET_KEY=' + 'ab' * 32 + '\n')
        bundle = root / 'export'
        base = [sys.executable, str(ROOT / 'scripts/backup.py')]
        subprocess.run(base + ['backup', str(bundle), '--engine', 'sqlite', '--sqlite', str(original), '--secrets-file', str(secret), '--offline'], check=True)
        assert bundle.stat().st_mode & 0o777 == 0o700
        assert (bundle / 'secrets.env').stat().st_mode & 0o777 == 0o600
        command = base + ['restore', str(bundle), '--engine', 'sqlite', '--sqlite', str(restored), '--server', str(SERVER), '--offline']
        subprocess.run(command, check=True)
        with sqlite3.connect(restored) as db:
            assert db.execute('SELECT username, access_epoch FROM accounts').fetchall() == [('recovery', 1)]
        assert subprocess.run(command, capture_output=True).returncode != 0
        before = restored.read_bytes()
        with (bundle / 'database').open('ab') as out:
            out.write(b'corruption')
        assert subprocess.run(command, capture_output=True).returncode != 0
        assert restored.read_bytes() == before
        assert subprocess.run(base + ['restore', str(bundle), '--engine', 'postgres', '--offline'], capture_output=True).returncode != 0
        # The bundle records the schema it holds.
        assert json.loads((bundle / 'manifest.json').read_text())['schema'] == 1001
        # A database from the previous baseline (no operation ledger) is a valid "back up before
        # upgrading" source; restoring it with this executable upgrades it in place.
        legacy, legacy_bundle, legacy_restored = root / 'legacy.sqlite', root / 'legacy-export', root / 'legacy-restored.sqlite'
        copy_database(original, legacy)
        with sqlite3.connect(legacy) as db:
            db.execute('DROP TABLE operation_outcomes')
            db.execute('UPDATE atlas_schema SET version=1000')
        subprocess.run(base + ['backup', str(legacy_bundle), '--engine', 'sqlite', '--sqlite', str(legacy), '--offline'], check=True)
        assert json.loads((legacy_bundle / 'manifest.json').read_text())['schema'] == 1000
        subprocess.run(base + ['restore', str(legacy_bundle), '--engine', 'sqlite', '--sqlite', str(legacy_restored), '--server', str(SERVER), '--offline'], check=True)
        with sqlite3.connect(legacy_restored) as db:
            assert db.execute('SELECT version FROM atlas_schema').fetchall() == [(1001,)]
            assert db.execute('SELECT COUNT(*) FROM operation_outcomes').fetchall() == [(0,)]
            assert db.execute('SELECT username FROM accounts').fetchall() == [('recovery',)]
        # An unknown baseline is still refused.
        unknown = root / 'unknown.sqlite'
        copy_database(original, unknown)
        with sqlite3.connect(unknown) as db:
            db.execute('UPDATE atlas_schema SET version=999')
        assert subprocess.run(base + ['backup', str(root / 'unknown-export'), '--engine', 'sqlite', '--sqlite', str(unknown), '--offline'], capture_output=True).returncode != 0
    print('Recovery bundle checks passed')


if __name__ == '__main__':
    main()
