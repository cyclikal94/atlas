#!/usr/bin/env python3
"""Validate Helm against an explicitly supplied disposable cluster/kubeconfig."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('image')
    parser.add_argument('--kubeconfig', required=True)
    args = parser.parse_args()
    repository, tag = args.image.rsplit(':', 1)
    namespace = 'atlas-test-' + uuid.uuid4().hex[:12]
    env = {**os.environ, 'KUBECONFIG': args.kubeconfig}
    def run(*command, data=None):
        result = subprocess.run(command, env=env, input=data, capture_output=True)
        if result.returncode:
            # Neither credentials nor Secret values should reach test logs.
            raise RuntimeError(f'{command[0]} {command[1]} failed: {result.stderr.decode()}')
        return result.stdout
    def kube(*command, data=None):
        return run('kubectl', '-n', namespace, *command, data=data)
    run('kubectl', 'create', 'namespace', namespace)
    try:
        password = uuid.uuid4().hex
        secret = {'apiVersion':'v1','kind':'Secret','metadata':{'name':'database'},'type':'Opaque',
                  'stringData':{'POSTGRES_PASSWORD':password,
                                'ATLAS_DATABASE_URL':f'postgres://atlas:{password}@supplied-postgres:5432/atlas'}}
        kube('apply', '-f', '-', data=json.dumps(secret).encode())
        common = ['deploy/helm/atlas', '--namespace', namespace,
                  '--set', 'image.repository=' + repository, '--set', 'image.tag=' + tag]
        run('helm', 'upgrade', '--install', 'supplied', *common,
            '--set', 'postgresql.enabled=true', '--set', 'postgresql.existingSecret=database', '--set', 'replicaCount=2',
            '--wait', '--timeout', '180s')
        kube('exec', '-i', 'deployment/supplied', '--', 'atlas-server', 'account', 'helm-fixture', data=b'helm-fixture-password')
        kube('exec', 'deployment/supplied', '--', 'atlas-server', 'probe')
        # A separate release uses the now-existing PostgreSQL database.
        run('helm', 'upgrade', '--install', 'external', *common,
            '--set', 'database.existingSecret=database', '--wait', '--timeout', '120s')
        kube('exec', 'deployment/external', '--', 'atlas-server', 'probe')
        kube('rollout', 'restart', 'deployment/supplied')
        kube('rollout', 'status', 'deployment/supplied', '--timeout=120s')
        # Same account survives application restart and is visible through either release.
        kube('exec', '-i', 'deployment/external', '--', 'atlas-server', 'password', 'helm-fixture', data=b'helm-replacement-password')
        print('Helm supplied/external PostgreSQL, two replicas, probes and restart persistence passed')
    finally:
        # Only the unique namespace created above is removed.
        run('kubectl', 'delete', 'namespace', namespace, '--wait=false')


if __name__ == '__main__':
    main()
