#!/usr/bin/env python3
"""Exercise supplied/existing PostgreSQL Compose paths in a disposable project."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]


def main():
    image = sys.argv[1] if len(sys.argv) > 1 else 'atlas-api:test'
    project = 'atlas-test-' + uuid.uuid4().hex[:12]
    with tempfile.TemporaryDirectory(prefix='atlas-compose-') as temp:
        root = Path(temp)
        for name in ['compose.yaml', 'compose.postgres.yaml']:
            shutil.copyfile(ROOT / name, root / name)
        (root / 'ports.yaml').write_text("services:\n  atlas:\n    ports: !override ['127.0.0.1::3000']\n")
        password = uuid.uuid4().hex
        env = {**os.environ, 'ATLAS_IMAGE': image, 'ATLAS_POSTGRES_PASSWORD': password}
        command = ['docker', 'compose', '-p', project, '--project-directory', str(root),
                   '-f', str(root / 'compose.yaml')]
        supplied = command + ['-f', str(root / 'compose.postgres.yaml'), '-f', str(root / 'ports.yaml')]
        external = command + ['-f', str(root / 'ports.yaml')]
        def run(args, data=None):
            result = subprocess.run(args, env=env, input=data, capture_output=True)
            if result.returncode:
                raise RuntimeError('Compose verification command failed: ' + result.stderr.decode())
            return result.stdout.decode().strip()
        def call(method, path, body=None, token=None):
            address = run(supplied + ['port', 'atlas', '3000'])
            headers = {'Content-Type': 'application/json'}
            if token:
                headers['Authorization'] = 'Bearer ' + token
            request = urllib.request.Request('http://' + address + path, method=method, headers=headers,
                data=None if body is None else json.dumps(body).encode())
            with urllib.request.urlopen(request, timeout=10) as response:
                return json.load(response)
        try:
            run(supplied + ['up', '-d', '--no-build', '--wait', '--wait-timeout', '120'])
            run(supplied + ['run', '--rm', '-T', 'atlas', 'account', 'compose'], b'compose-fixture-password')
            token = call('POST', '/api/experimental/v1/sessions', {
                'username':'compose','password':'compose-fixture-password','device_id':'compose-test'})['access_token']
            account = call('GET', '/api/experimental/v1/me', token=token)
            run(supplied + ['restart', 'atlas'])
            run(supplied + ['up', '-d', '--no-build', '--wait', '--wait-timeout', '120'])
            assert call('GET', '/api/experimental/v1/me', token=token) == account
            # Keep the existing database/network, recreate only Atlas through the base file.
            settings = root / 'atlas.env'
            settings.write_text(f'ATLAS_DATABASE_URL=postgres://atlas:{password}@postgres:5432/atlas\n')
            settings.chmod(0o600)
            run(external + ['up', '-d', '--no-deps', '--no-build', '--wait', '--wait-timeout', '120', 'atlas'])
            assert call('GET', '/api/experimental/v1/me', token=token) == account
            print('Compose supplied/existing PostgreSQL, login and restart persistence passed')
        finally:
            subprocess.run(supplied + ['down', '-v', '--remove-orphans'], env=env, capture_output=True, check=True)


if __name__ == '__main__':
    main()
