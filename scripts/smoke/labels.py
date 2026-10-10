"""BE-Q25 session/device labels, device names and capacity against a real server process.

Real sockets, real browser sign-in (grant, then `activate` with a different `User-Agent`) and
real bearer sign-ins; every request and response is validated against the contract by
`ContractClient`. The device-name sentinel is in the caller's secret list, so the final log
scan proves a name never reaches the logs. Device IDs are this module's own, so the retirement
scenarios in `devices.py` are unaffected.
"""
import base64
import hashlib
import secrets as random
import uuid

V1 = '/api/experimental/v1'
ORIGIN = 'https://atlas.example'
CHROME_WINDOWS = ('Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 '
                  '(KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36')
FIREFOX_LINUX = 'Mozilla/5.0 (X11; Linux x86_64; rv:131.0) Gecko/20100101 Firefox/131.0'
SAFARI_IPHONE = ('Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 '
                 '(KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1')
NAME_SENTINEL = 'DEVICE_NAME_SENTINEL'


def _capacity_matches(capacity, used):
    assert capacity == {'limit': 32, 'used': used, 'remaining': 32 - used}, (capacity, used)


def _grant(call, password, device, grant_agent):
    """A browser sign-in's first step: the pending grant and the verifier that activates it."""
    verifier = random.token_urlsafe(48)
    challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b'=').decode()
    granted = call('POST', V1 + '/browser-sessions', body={
        'username': 'alice', 'password': password, 'device_id': device,
        'attempt_challenge': challenge}, extra_headers={'Origin': ORIGIN, 'User-Agent': grant_agent})
    return granted['grant'], verifier


def _activate(call, grant, verifier, activate_agent):
    activated = call('POST', V1 + '/browser-sessions/activate',
                     body={'grant': grant, 'verifier': verifier},
                     extra_headers={'Origin': ORIGIN, 'User-Agent': activate_agent})
    cookie = next(value for name, value in call.last_headers.items()
                  if name.lower() == 'set-cookie').split(';')[0]
    return {'Cookie': cookie, 'X-CSRF-Token': activated['csrf_token'], 'Origin': ORIGIN}


def _browser_sign_in(call, password, device, grant_agent, activate_agent):
    grant, verifier = _grant(call, password, device, grant_agent)
    return _activate(call, grant, verifier, activate_agent)


def run(call, tokens, secrets):
    password = secrets[0]
    secrets.append(NAME_SENTINEL)
    phone, bob = tokens['alice'], tokens['bob']

    native = call('POST', V1 + '/sessions', body={
        'username': 'alice', 'password': password, 'device_id': 'label-native'},
        extra_headers={'User-Agent': CHROME_WINDOWS})['access_token']
    secrets.append(native)
    browser = _browser_sign_in(call, password, 'label-browser', FIREFOX_LINUX, SAFARI_IPHONE)
    secrets.append(browser['Cookie'].split('=', 1)[1])

    # Labels: the channel is the route's; the browser session follows its activate request.
    listing = call('GET', V1 + '/sessions', native)
    own = next(s for s in listing['sessions'] if s['current'])
    assert own['client'] == {'channel': 'native', 'browser': None, 'platform': 'windows'}, own
    cookie_listing = call('GET', V1 + '/sessions', extra_headers=browser)
    own = next(s for s in cookie_listing['sessions'] if s['current'])
    assert own['device_id'] == 'label-browser'
    assert own['client'] == {'channel': 'browser', 'browser': 'safari', 'platform': 'ios'}, own
    _capacity_matches(listing['session_capacity'], len(listing['sessions']))

    # Names: set over bearer and over cookie + CSRF, returned verbatim as text.
    hostile = f'<b>{NAME_SENTINEL}</b> \U0001F600'
    call('PUT', f'{V1}/devices/label-native/name', native, {'name': hostile}, expected=204)
    call('PUT', f'{V1}/devices/label-browser/name', body={'name': 'This browser'},
         expected=204, extra_headers=browser)
    devices = call('GET', V1 + '/devices', phone)
    by_id = {d['id']: d for d in devices['devices']}
    assert by_id['label-native']['name'] == hostile
    assert by_id['label-native']['client'] == {'channel': 'native', 'browser': None,
                                               'platform': 'windows'}
    assert by_id['label-browser']['name'] == 'This browser'
    assert by_id['label-browser']['client']['browser'] == 'safari'
    registered = sum(1 for d in devices['devices'] if d['summary']['sync_registered'])
    _capacity_matches(devices['sync_capacity'], registered)
    sessions = call('GET', V1 + '/sessions', phone)['sessions']
    assert {s['device_name'] for s in sessions if s['device_id'] == 'label-native'} == {hostile}

    # Refusals: invalid text, wrong shape, another account's view, no credential.
    call('PUT', f'{V1}/devices/label-native/name', native, {'name': 'a‮b'}, expected=422)
    call('PUT', f'{V1}/devices/label-native/name', native, {'name': None}, expected=400)
    call('PUT', f'{V1}/devices/label-native/name', bob, {'name': 'Bob'}, expected=404)
    call('DELETE', f'{V1}/devices/label-native/name', bob, expected=404)
    call('PUT', f'{V1}/devices/label-native/name', body={'name': 'Anon'}, expected=401)
    assert all(d['id'] != 'label-native' for d in call('GET', V1 + '/devices', bob)['devices'])
    assert call('GET', V1 + '/devices', phone)['devices'] == devices['devices']

    # Clearing is idempotent.
    for _ in range(2):
        call('DELETE', f'{V1}/devices/label-browser/name', extra_headers=browser, expected=204)
    assert {d['id']: d for d in call('GET', V1 + '/devices', phone)['devices']}[
        'label-browser']['name'] is None

    # Signing out frees a session slot.
    call('PUT', f'{V1}/devices/label-browser/name', body={'name': 'Browser to forget'},
         expected=204, extra_headers=browser)
    before = call('GET', V1 + '/sessions', phone)['session_capacity']['used']
    call('DELETE', V1 + '/sessions/current', extra_headers=browser, expected=204)
    after = call('GET', V1 + '/sessions', phone)
    assert after['session_capacity']['used'] == before - 1
    _capacity_matches(after['session_capacity'], len(after['sessions']))

    # That was the device's last member, so it is forgotten: a browser sign-in lists it again
    # unnamed, both while its grant is pending and once the grant is activated.
    assert all(d['id'] != 'label-browser' for d in call('GET', V1 + '/devices', phone)['devices'])
    grant, verifier = _grant(call, password, 'label-browser', FIREFOX_LINUX)
    pending = next(d for d in call('GET', V1 + '/devices', phone)['devices']
                   if d['id'] == 'label-browser')
    assert pending['summary']['pending_sign_ins'] == 1 and pending['name'] is None, pending
    returned = _activate(call, grant, verifier, FIREFOX_LINUX)
    secrets.append(returned['Cookie'].split('=', 1)[1])
    relisted = next(d for d in call('GET', V1 + '/devices', phone)['devices']
                    if d['id'] == 'label-browser')
    assert relisted['name'] is None and relisted['client']['browser'] == 'firefox', relisted
    call('DELETE', V1 + '/sessions/current', extra_headers=returned, expected=204)

    # A confirmed retirement forgets the name; the device's next sign-in starts unnamed.
    state = next(d for d in call('GET', V1 + '/devices', phone)['devices']
                 if d['id'] == 'label-native')['state_token']
    call('PUT', f'{V1}/devices/label-native/name', phone, {'name': 'Renamed'}, expected=204)
    done = call('DELETE', f'{V1}/devices/label-native', phone, operation=str(uuid.uuid4()),
                extra_headers={'Atlas-Device-State': state})
    assert done['outcome'] == 'confirmed_applied', done
    call('PUT', f'{V1}/devices/label-native/name', phone, {'name': 'Ghost'}, expected=404)
    again = call('POST', V1 + '/sessions', body={
        'username': 'alice', 'password': password, 'device_id': 'label-native'},
        extra_headers={'User-Agent': FIREFOX_LINUX})['access_token']
    secrets.append(again)
    device = next(d for d in call('GET', V1 + '/devices', again)['devices']
                  if d['id'] == 'label-native')
    assert device['name'] is None and device['client']['platform'] == 'linux', device
    call('DELETE', V1 + '/sessions/current', again, expected=204)
    return {'scenario': 'labels-and-capacity', 'session_capacity': after['session_capacity'],
            'sync_capacity': devices['sync_capacity']}
