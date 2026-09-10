#!/usr/bin/env python3
"""Check deployment configuration without contacting a cluster or Docker daemon."""
import json
import os
import subprocess


def main():
    chart = 'deploy/helm/atlas'
    def helm(*values, valid=True):
        result = subprocess.run(['helm', 'template', 'atlas', chart, *values], capture_output=True, text=True)
        assert (result.returncode == 0) == valid, result.stderr
        return result.stdout
    helm(valid=False)
    helm("--set", "database.engine=sqlite", valid=False)
    helm("--set", "database.existingSecret=database", "--set", "replicaCount=0", valid=False)
    external = helm('--set', 'database.existingSecret=database', '--set', 'replicaCount=2')
    assert 'replicas: 2' in external and 'PersistentVolumeClaim' not in external
    supplied = helm('--set', 'postgresql.enabled=true', '--set', 'postgresql.existingSecret=database')
    assert 'kind: StatefulSet' in supplied and 'volumeClaimTemplates:' in supplied
    helm('--set', 'postgresql.enabled=true', valid=False)
    helm('--set', 'database.existingSecret=database', '--set', 'config.ATLAS_DATABASE_URL=sqlite://x', valid=False)
    env = {**os.environ, 'ATLAS_POSTGRES_PASSWORD': 'template-fixture-password'}
    result = subprocess.check_output(['docker', 'compose', '-f', 'compose.yaml', '-f', 'compose.postgres.yaml', 'config', '--format', 'json'], env=env)
    services = json.loads(result)['services']
    assert services['atlas']['read_only']
    assert not services['atlas'].get('volumes')
    assert 'ports' not in services['postgres']
    assert services['atlas']['environment']['ATLAS_DATABASE_URL'].startswith('postgres://')
    print('Deployment template checks passed')


if __name__ == '__main__':
    main()
