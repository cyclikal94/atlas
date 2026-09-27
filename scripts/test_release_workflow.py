#!/usr/bin/env python3
"""Guard the API types release configuration.

This reads the workflow files and does not run GitHub Actions. It stops the pipeline from being
silently weakened (job removed, steps reordered, a release flag on a pull request, install scripts
enabled). It also runs each `api-types` job's own generate and verify commands, through the same
`npm run --prefix` invocation, in a scratch checkout and asserts that the job's upload path finds
the tarball and its sidecar: npm runs scripts from `scripts/api-types`, so a relative `--output`
lands somewhere the upload never looks. It does not prove that a real Release run succeeds.

It needs git, node, npm and `npm ci --ignore-scripts --prefix scripts/api-types` done first.
"""
import copy
import fnmatch
import glob
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

try:
    import yaml
except ImportError:
    sys.exit('PyYAML is required; install scripts/requirements-contract.txt (it is a dependency of the contract validator)')

ROOT = Path(__file__).resolve().parent.parent
TOOL = ROOT / 'scripts/api-types'
PUBLISHING = ('gh release', 'docker push', 'helm push', 'npm publish', 'docker login')
TOOL_FILES = ('package.json', 'package-lock.json', '.node-version', '.gitignore', 'lib.mjs', 'generate.mjs', 'verify.mjs')


class Failure(Exception):
    pass


def require(condition, message):
    if not condition:
        raise Failure(message)


def load(name):
    return yaml.safe_load((ROOT / '.github' / name).read_text())


def steps_of(workflow, job):
    return workflow['jobs'][job]['steps']


def index_of(steps, needle, field='run'):
    for position, step in enumerate(steps):
        if needle in str(step.get(field, '')):
            return position
    raise Failure(f'no step has {needle!r} in {field}')


def check_common(workflow, label):
    require(workflow.get('permissions') == {'contents': 'read'}, f'{label}: workflow permissions must stay contents: read')
    for job in workflow['jobs'].values():
        for step in job.get('steps', []):
            for command in re.findall(r'npm (?:ci|install)\b[^\n]*', str(step.get('run', ''))):
                require('--ignore-scripts' in command, f'{label}: {command!r} must use --ignore-scripts')


def check_node_pin(steps, label):
    setup = steps[index_of(steps, 'actions/setup-node@', 'uses')]
    require(setup['with'].get('node-version-file') == 'scripts/api-types/.node-version', f'{label}: setup-node must read scripts/api-types/.node-version')
    require('node-version' not in setup['with'], f'{label}: a literal node-version would bypass the pinned file')
    pinned = (TOOL / '.node-version').read_text().strip()
    require(re.fullmatch(r'\d+\.\d+\.\d+', pinned), '.node-version must be one exact version')
    engines = json.loads((TOOL / 'package.json').read_text())['engines']['node']
    require(engines == pinned, f'package.json engines.node {engines} must equal .node-version {pinned}')


def check_release(workflow):
    check_common(workflow, 'release.yml')
    jobs = workflow['jobs']
    require('api-types' in jobs['publish']['needs'], 'publish must depend on the api-types job')
    require("github.event_name == 'push'" in jobs['publish']['if'] and 'refs/tags/v' in jobs['publish']['if'], 'publish must stay gated on pushed v* tags')
    job = jobs['api-types']
    require(job['needs'] == 'metadata', 'api-types must take its version from the metadata job')
    require('permissions' not in job, 'api-types must not widen the workflow permissions')
    steps = job['steps']
    check_node_pin(steps, 'release.yml api-types')
    order = [
        index_of(steps, 'scripts/validate_contract.py'),
        index_of(steps, 'npm ci --ignore-scripts --prefix scripts/api-types'),
        index_of(steps, 'npm test --prefix scripts/api-types'),
        index_of(steps, 'npm run generate --prefix scripts/api-types'),
        index_of(steps, 'npm run verify --prefix scripts/api-types'),
        index_of(steps, 'actions/upload-artifact@', 'uses'),
    ]
    require(order == sorted(order) and len(set(order)) == len(order), 'api-types order must be validate, npm ci, test, generate, verify, upload')
    for position, action in ((order[3], 'generate'), (order[4], 'verify')):
        step = steps[position]
        lines = [line.strip() for line in step['run'].splitlines() if line.strip() and not line.strip().startswith('#')]
        release_lines = [line for line in lines if '--release' in line]
        require(len(release_lines) == 1 and release_lines[0].startswith('if [[ "$EVENT_NAME" == push ]]'), f'{action}: --release must be added only when the event is a tag push')
        require(step['env']['EVENT_NAME'] == '${{ github.event_name }}', f'{action}: EVENT_NAME must come from github.event_name')
        require(step['env']['VERSION'] == '${{ needs.metadata.outputs.version }}', f'{action}: the version must come from the metadata job')
        require('"${flags[@]}"' in step['run'] and '--version "$VERSION"' in step['run'], f'{action} must pass the version and the release flags')
    upload = steps[order[5]]['with']
    require(upload['name'] == 'atlas-api-types', 'the asset artefact name changed')
    require(job['env']['API_TYPES_OUTPUT'].startswith('${{ github.workspace }}/'), 'API_TYPES_OUTPUT must be an absolute path under the workspace')

    publish = jobs['publish']['steps']
    downloads = [step['with'] for step in publish if str(step.get('uses', '')).startswith('actions/download-artifact@')]
    require(any(fnmatch.fnmatch(upload['name'], download.get('pattern', '')) and download.get('merge-multiple') for download in downloads),
            'publish must download the api-types artefact into downloads/')
    verify = index_of(publish, 'atlas-api-types-$VERSION.tgz')
    require('sha256sum -c' in publish[verify]['run'] and 'test -f' in publish[verify]['run'], 'publish must check the asset exists and matches its sidecar')
    first_publication = min(index_of(publish, command) for command in ('docker push', 'helm push', 'gh release create'))
    require(verify < first_publication, 'the API types asset must be verified before any image, chart or release is published')
    release_step = publish[index_of(publish, 'gh release create')]['run']
    require('./*.tgz' in release_step and 'atlas-api-types-$VERSION.tgz' in release_step, 'SHA256SUMS and the release notes must cover the API types asset')


def check_checks(workflow):
    check_common(workflow, 'checks.yml')
    require('pull_request' in (workflow.get('on') or workflow.get(True)), 'checks must run on pull requests')
    job = workflow['jobs']['api-types']
    require('permissions' not in job, 'the api-types check must not widen permissions')
    steps = job['steps']
    check_node_pin(steps, 'checks.yml api-types')
    order = [
        index_of(steps, 'scripts/validate_contract.py'),
        index_of(steps, 'npm ci --ignore-scripts --prefix scripts/api-types'),
        index_of(steps, 'scripts/test_release_workflow.py'),
        index_of(steps, 'npm test --prefix scripts/api-types'),
        index_of(steps, 'npm run generate --prefix scripts/api-types -- --version 0.0.0-ci'),
        index_of(steps, 'npm run verify --prefix scripts/api-types'),
        index_of(steps, 'actions/upload-artifact@', 'uses'),
    ]
    require(order == sorted(order) and len(set(order)) == len(order), 'checks api-types order must be validate, npm ci, workflow test, test, generate, verify, upload')
    require(job['env']['API_TYPES_OUTPUT'].startswith('${{ github.workspace }}/'), 'API_TYPES_OUTPUT must be an absolute path under the workspace')
    require(steps[order[6]]['with'].get('if-no-files-found') == 'error', 'the pull-request upload must fail when it finds nothing')
    text = json.dumps(job)
    require('--release' not in text, 'a pull-request check must never build a releasable version')
    for command in PUBLISHING:
        require(command not in text, f'a pull-request check must not publish ({command})')
    require('contents: write' not in text, 'a pull-request check must not write')


def check_shared(release, checks):
    for action in ('actions/setup-node', 'actions/setup-python'):
        versions = {step['uses'] for workflow in (release, checks) for job in workflow['jobs'].values() for step in job.get('steps', []) if str(step.get('uses', '')).startswith(action + '@')}
        require(len(versions) == 1, f'{action} should use one version across workflows, found {sorted(versions)}')


def check_dependabot():
    updates = load('dependabot.yml')['updates']
    require(any(update['package-ecosystem'] == 'npm' and update['directory'] == '/scripts/api-types' for update in updates), 'dependabot must watch the pinned generator')


EXPRESSION = re.compile(r'\$\{\{\s*(.*?)\s*\}\}')
GIT_ENV = {
    'GIT_CONFIG_GLOBAL': os.devnull, 'GIT_CONFIG_SYSTEM': os.devnull, 'GIT_CONFIG_NOSYSTEM': '1',
    'GIT_AUTHOR_NAME': 'Fixture', 'GIT_AUTHOR_EMAIL': 'fixture@example.invalid',
    'GIT_COMMITTER_NAME': 'Fixture', 'GIT_COMMITTER_EMAIL': 'fixture@example.invalid',
}


def substitute(text, context):
    """Resolve `${{ ... }}` from a fixed table; an expression it does not know is a failure, never a guess."""
    def replace(match):
        if match.group(1) not in context:
            raise Failure(f'the test cannot evaluate ${{{{ {match.group(1)} }}}}; extend context in scripts/test_release_workflow.py')
        return context[match.group(1)]
    return EXPRESSION.sub(replace, str(text))


def pinned_node():
    return (TOOL / '.node-version').read_text().strip()


def node_matches_pin():
    """Release-mode generation needs the pinned Node. CI always has it, so a mismatch there is a failure."""
    running = subprocess.run(['node', '--version'], capture_output=True, text=True, check=True).stdout.strip().lstrip('v')
    if running == pinned_node():
        return True
    require(not os.environ.get('GITHUB_ACTIONS'), f'CI runs Node {running}, not the pinned {pinned_node()}')
    print(f'skipped: release-mode scenario needs Node {pinned_node()}, running {running}')
    return False


def make_workspace(parent):
    """A scratch checkout shaped like the repository, committed so `--release` sees a clean tracked tree."""
    installed = TOOL / 'node_modules'
    require(installed.is_dir(), 'run npm ci --ignore-scripts --prefix scripts/api-types first; the workflows do so before this step')
    workspace = Path(tempfile.mkdtemp(dir=parent, prefix='workspace-'))
    (workspace / 'api').mkdir()
    shutil.copy(ROOT / 'api/openapi.json', workspace / 'api/openapi.json')
    shutil.copy(ROOT / 'LICENSE', workspace / 'LICENSE')
    (workspace / 'scripts/api-types').mkdir(parents=True)
    for name in TOOL_FILES:
        shutil.copy(TOOL / name, workspace / 'scripts/api-types' / name)
    environment = {**os.environ, **GIT_ENV}
    for command in (['init', '-q'], ['add', '-A'], ['-c', 'commit.gpgsign=false', 'commit', '-q', '-m', 'Scratch checkout']):
        subprocess.run(['git', *command], cwd=workspace, env=environment, check=True, capture_output=True)
    (workspace / 'scripts/api-types/node_modules').symlink_to(installed)
    return workspace


def elsewhere(workspace):
    return sorted(str(path.relative_to(workspace)) for base in (workspace / 'target', workspace / 'scripts/api-types/target') for path in base.rglob('*.tgz'))


def run_job(workflow, event, version, parent, expected_version=None):
    """Run the api-types job's generate and verify steps as written, then evaluate its upload path.

    Only steps that call `npm run generate` or `npm run verify` are executed; installation, tests and
    validation are covered by their own jobs. Returns the files the upload would send.
    """
    workspace = make_workspace(parent)
    job = workflow['jobs']['api-types']
    context = {'github.workspace': str(workspace), 'github.event_name': event, 'needs.metadata.outputs.version': version}
    job_env = {name: substitute(value, context) for name, value in job.get('env', {}).items()}
    context.update({f'env.{name}': value for name, value in job_env.items()})
    upload = None
    for step in job['steps']:
        script = str(step.get('run', ''))
        if 'npm run generate' in script or 'npm run verify' in script:
            step_env = {name: substitute(value, context) for name, value in step.get('env', {}).items()}
            done = subprocess.run(['bash', '-e', '-c', script], cwd=workspace, capture_output=True, text=True, timeout=300,
                                  env={**os.environ, 'GITHUB_WORKSPACE': str(workspace), **job_env, **step_env})
            require(done.returncode == 0, f'`{script.strip().splitlines()[-1].strip()}` failed:\n{(done.stderr or done.stdout).strip()}')
        elif str(step.get('uses', '')).startswith('actions/upload-artifact@'):
            upload = substitute(step['with']['path'], context)
    require(upload is not None, 'the api-types job has no upload step')
    pattern = upload if os.path.isabs(upload) else str(workspace / upload)
    matches = {Path(match).name: Path(match) for match in glob.glob(pattern)}
    found = sorted(matches)
    name = f'atlas-api-types-{expected_version or version}.tgz'
    require(found == sorted([name, f'{name}.sha256']),
            f'the upload path {upload!r} matches {found or "nothing"}, not {name} and its sidecar; '
            f'tarballs written elsewhere: {elsewhere(workspace)}')
    tarball, sidecar = matches[name], matches[f'{name}.sha256']
    digest, filename = sidecar.read_text().rstrip('\n').split('  ')
    require(filename == tarball.name and digest == hashlib.sha256(tarball.read_bytes()).hexdigest(), 'the uploaded sidecar does not match the uploaded tarball')
    return found


def contract_version():
    return json.loads((ROOT / 'api/openapi.json').read_text())['info']['version']


def check_upload_paths(release, checks, parent, pinned):
    """Each job's generate and verify commands put exactly the tarball and sidecar where its upload looks."""
    run_job(checks, 'pull_request', '0.0.0-ci', parent)
    run_job(release, 'workflow_dispatch', '0.0.0-dev', parent)
    if pinned:
        # A pushed tag: the real `--release` generation and provenance verification on the real contract.
        run_job(release, 'push', contract_version(), parent)
        run_job(release, 'push', f'{contract_version()}-rc.1', parent)


def executed_negative_controls(release, parent, pinned):
    """Reintroduce the defects the executed check exists for; each must fail for its own reason."""
    def mutated(change):
        clone = copy.deepcopy(release)
        change(clone['jobs']['api-types']['steps'])
        return clone

    def each(steps, needle, action, field='run'):
        for step in steps:
            if needle in str(step.get(field, '')):
                action(step)

    def relative_output(steps):
        # The configuration as first delivered: `--output target/api-types` under `npm run --prefix`.
        each(steps, 'npm run', lambda step: step.update(run=step['run'].replace('$API_TYPES_OUTPUT', 'target/api-types')))
        each(steps, 'actions/upload-artifact@', lambda step: step['with'].update(path='target/api-types/*'), 'uses')

    def upload_elsewhere(steps):
        each(steps, 'actions/upload-artifact@', lambda step: step['with'].update(path='${{ github.workspace }}/scripts/api-types/target/api-types/*'), 'uses')

    def verify_without_release(steps):
        each(steps, 'npm run verify', lambda step: step.update(run=step['run'].replace('"${flags[@]}"', '')))

    controls = [
        ('relative --output, as first delivered', relative_output, 'workflow_dispatch', '0.0.0-dev',
         r"upload path 'target/api-types/\*' matches nothing.*scripts/api-types/target/api-types/atlas-api-types-0\.0\.0-dev\.tgz"),
        ('upload path elsewhere', upload_elsewhere, 'workflow_dispatch', '0.0.0-dev', r'matches nothing'),
    ]
    if pinned:
        controls.append(('tag verified without --release', verify_without_release, 'push', contract_version(), r'is a release version; verify it with --release'))
    for label, change, event, version, reason in controls:
        try:
            run_job(mutated(change), event, version, parent)
        except Failure as failure:
            require(re.search(reason, str(failure), re.DOTALL), f'negative control {label!r} failed for the wrong reason: {failure}')
            continue
        raise Failure(f'negative control not detected: {label}')

def negative_controls(release, checks):
    """Each deliberate weakening must be reported, so the checks above cannot pass vacuously."""
    def mutated(workflow, change):
        clone = copy.deepcopy(workflow)
        change(clone)
        return clone

    def types_step(w, needle, field='run'):
        steps = steps_of(w, 'api-types')
        return steps[index_of(steps, needle, field)]

    def drop_need(w): w['jobs']['publish']['needs'].remove('api-types')
    def unguard_release(w):
        step = types_step(w, 'npm run generate')
        step['run'] = step['run'].replace('if [[ "$EVENT_NAME" == push ]]; then flags+=(--release); fi', 'flags+=(--release)')
    def unguard_verify(w):
        step = types_step(w, 'npm run verify')
        step['run'] = step['run'].replace('if [[ "$EVENT_NAME" == push ]]; then flags+=(--release); fi', 'flags+=(--release)')
    def drop_tests(w): steps_of(w, 'api-types').remove(types_step(w, 'npm test --prefix scripts/api-types'))
    def allow_scripts(w): types_step(w, 'npm ci')['run'] = 'npm ci --prefix scripts/api-types'
    def widen(w): w['permissions'] = {'contents': 'write'}
    def literal_node(w): types_step(w, 'actions/setup-node@', 'uses')['with']['node-version'] = '22.23.2'
    def late_verify(w):
        steps = w['jobs']['publish']['steps']
        steps.append(steps.pop(index_of(steps, 'atlas-api-types-$VERSION.tgz')))
    def rename_artefact(w): types_step(w, 'actions/upload-artifact@', 'uses')['with']['name'] = 'types'

    for label, change in [('publish no longer needs api-types', drop_need), ('--release without the push guard', unguard_release), ('verify --release without the push guard', unguard_verify),
                          ('npm test removed', drop_tests), ('install scripts enabled', allow_scripts), ('write permission', widen),
                          ('literal Node version', literal_node), ('asset verified after publication', late_verify), ('artefact renamed', rename_artefact)]:
        try:
            check_release(mutated(release, change))
        except Failure:
            continue
        raise Failure(f'negative control not detected: {label}')

    def release_on_pr(w): types_step(w, 'npm run generate')['run'] += ' --release'
    def publish_on_pr(w): steps_of(w, 'api-types').append({'run': 'gh release create v0.0.0'})
    for label, change in [('--release in checks', release_on_pr), ('publishing in checks', publish_on_pr)]:
        try:
            check_checks(mutated(checks, change))
        except Failure:
            continue
        raise Failure(f'negative control not detected: {label}')


def main():
    release, checks = load('workflows/release.yml'), load('workflows/checks.yml')
    check_release(release)
    check_checks(checks)
    check_shared(release, checks)
    check_dependabot()
    negative_controls(release, checks)
    pinned = node_matches_pin()
    with tempfile.TemporaryDirectory(prefix='atlas-release-workflow-') as parent:
        check_upload_paths(release, checks, parent, pinned)
        executed_negative_controls(release, parent, pinned)
    print('API types release workflow checks passed' + ('' if pinned else ' (release-mode scenarios skipped: not on the pinned Node)'))


if __name__ == '__main__':
    try:
        main()
    except Failure as failure:
        sys.exit(f'test_release_workflow: {failure}')
