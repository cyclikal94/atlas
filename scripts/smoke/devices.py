"""Device retirement and keyed session revocation against a real server process.

Walks the recovery scenarios of the account-lifecycle matrix (T18 logout, T19 device
retirement) with real sessions over real sockets. Every response is validated against the
contract by `ContractClient`, including the `409` body, and none may set a cookie.

A device that holds only a live native handoff, or only an active subscription, once its last
session has gone must still be listed with a fresh token (R1). The smoke has no OpenID provider
or push endpoint to create those rows through the API, so the caller's optional `seed` writes them
directly into the disposable database: they are **fixtures**. Everything after that (listing,
stale refusal, retirement, replay, account isolation) is the real server over real sockets. The
rows the real writers produce, and how they order against a retirement, are proved by the
in-process schedules in `crates/server/tests/cases/writers.rs`, not here.
"""
import uuid

V1 = '/api/experimental/v1'


def _no_cookie(call):
    assert not any(name.lower() == 'set-cookie' for name in call.last_headers), \
        'a self-revoking response set a cookie'


def _state(call, token, device):
    devices = call('GET', V1 + '/devices', token)['devices']
    match = [d for d in devices if d['id'] == device]
    assert len(match) == 1, f'{device} is not listed once: {devices}'
    return match[0]


def run(call, tokens, secrets, seed=None):
    """`seed(account_id, device, kind)` inserts a `'handoff'` or `'subscription'` fixture row."""
    password = secrets[0]

    def login(device):
        session = call('POST', V1 + '/sessions', body={
            'username': 'alice', 'password': password, 'device_id': device})
        secrets.append(session['access_token'])
        return session['access_token'], session['account_id']

    phone = tokens['alice']
    laptop, account = login('laptop')
    tablet_one, _ = login('tablet')

    # The listing offers the approved state of each device, read from one snapshot.
    tablet = _state(call, phone, 'tablet')
    assert tablet['summary']['sessions'] == 1 and tablet['state_token'].startswith('v1.')
    assert tablet['state_token'] == _state(call, laptop, 'tablet')['state_token']

    def retire(token, device, operation, state, expected):
        return call('DELETE', f'{V1}/devices/{device}', token, operation=operation,
                    expected=expected, extra_headers={'Atlas-Device-State': state})

    # Refusals leave everything alone: a missing header, a malformed or unknown-version token.
    call('DELETE', f'{V1}/devices/tablet', phone, expected=400,
         extra_headers={'Atlas-Device-State': tablet['state_token']})
    call('DELETE', f'{V1}/devices/tablet', phone, operation=str(uuid.uuid4()), expected=400)
    retire(phone, 'tablet', str(uuid.uuid4()), 'v2.' + 'A' * 43, 422)
    retire(phone, 'tablet', str(uuid.uuid4()), 'v1.short', 422)
    retire(phone, 'tablet', str(uuid.uuid4()).upper(), tablet['state_token'], 422)

    # T19 Q: the device changed after the user confirmed it. A second session appears; the
    # attempt is rejected, nothing is deleted, and the rejection is a recorded, replayable outcome.
    tablet_two, _ = login('tablet')
    stale_operation = str(uuid.uuid4())
    rejected = retire(phone, 'tablet', stale_operation, tablet['state_token'], 409)
    assert rejected['outcome'] == 'rejected_stale' and rejected['code'] == 'operation_conflict'
    assert rejected['operation_id'] == stale_operation and rejected['account_id'] == account
    _no_cookie(call)
    assert _state(call, laptop, 'tablet')['summary']['sessions'] == 2
    replay = retire(laptop, 'tablet', stale_operation, tablet['state_token'], 409)
    assert replay['outcome'] == 'rejected_stale'

    # T19 P: fresh read, fresh confirmation, new operation ID. The device retires itself,
    # revoking the very session that sent the request.
    fresh = _state(call, tablet_one, 'tablet')
    assert fresh['state_token'] != tablet['state_token']
    operation = str(uuid.uuid4())
    done = retire(tablet_one, 'tablet', operation, fresh['state_token'], 200)
    assert done == {'operation_id': operation, 'account_id': account, 'outcome': 'confirmed_applied'}
    _no_cookie(call)
    # The response was the last thing that credential did.
    call('GET', V1 + '/me', tablet_one, expected=401)
    call('GET', V1 + '/me', tablet_two, expected=401)
    # A retry under the dead credential is not an answer; the outcome is read back under another
    # session by repeating the identical call, and reusing the ID for anything else is refused.
    retire(tablet_one, 'tablet', operation, fresh['state_token'], 401)
    assert retire(laptop, 'tablet', operation, fresh['state_token'], 200) == done
    retire(laptop, 'tablet', operation, _state(call, laptop, 'laptop')['state_token'], 422)
    retire(laptop, 'laptop', operation, fresh['state_token'], 422)
    assert all(d['id'] != 'tablet' for d in call('GET', V1 + '/devices', laptop)['devices'])
    # Retiring a device that has nothing left is `superseded`, distinct from a changed state.
    nothing = str(uuid.uuid4())
    assert retire(laptop, 'tablet', nothing, fresh['state_token'], 200)['outcome'] == 'superseded'

    # T18: keyed logout of a specific session, recoverable and never stale.
    sessions = call('GET', V1 + '/sessions', phone)['sessions']
    laptop_session = next(s['id'] for s in sessions if s['device_id'] == 'laptop')
    logout = str(uuid.uuid4())
    out = call('DELETE', f'{V1}/sessions/{laptop_session}', laptop, operation=logout)
    assert out == {'operation_id': logout, 'account_id': account, 'outcome': 'confirmed_applied'}
    _no_cookie(call)
    call('GET', V1 + '/me', laptop, expected=401)
    call('DELETE', f'{V1}/sessions/{laptop_session}', laptop, operation=logout, expected=401)
    # Lost response: another session repeats the call, after the target is gone.
    assert call('DELETE', f'{V1}/sessions/{laptop_session}', phone, operation=logout) == out
    again = str(uuid.uuid4())
    absent = call('DELETE', f'{V1}/sessions/{laptop_session}', phone, operation=again)
    assert absent['outcome'] == 'superseded'
    assert call('DELETE', f'{V1}/sessions/{laptop_session}', phone, operation=again) == absent
    call('DELETE', f'{V1}/sessions/{laptop_session}', phone, operation=logout.upper(), expected=422)
    # Without a key the route is the plain revocation it always was.
    call('DELETE', f'{V1}/sessions/{laptop_session}', phone, expected=404)
    call('GET', V1 + '/me', phone)

    if seed is not None:
        _left_with_one_member(call, tokens, login, retire, account, seed)


def _left_with_one_member(call, tokens, login, retire, account, seed):
    phone, bob = tokens['alice'], tokens['bob']
    for device, kind, held in [('watch', 'handoff', 'native_handoffs'),
                               ('speaker', 'subscription', 'notification_subscriptions')]:
        session, _ = login(device)
        seed(account, device, kind)
        offered = _state(call, phone, device)
        assert offered['summary']['sessions'] == 1 and offered['summary'][held] == 1, offered

        # Another account neither sees the device nor can affect it with Alice's token.
        assert all(d['id'] != device for d in call('GET', V1 + '/devices', bob)['devices'])
        foreign = retire(bob, device, str(uuid.uuid4()), offered['state_token'], 200)
        assert foreign['outcome'] == 'superseded', foreign
        assert _state(call, phone, device)['summary'][held] == 1

        # The device's only session signs out. Only the member remains, and it stays listed.
        call('DELETE', V1 + '/sessions/current', session, expected=204)
        left = _state(call, phone, device)
        assert left['active_sessions'] == 0 and left['summary']['sessions'] == 0, left
        assert left['summary'][held] == 1 and left['state_token'] != offered['state_token'], left

        # The token offered before the logout is refused and the member survives.
        stale = str(uuid.uuid4())
        rejected = retire(phone, device, stale, offered['state_token'], 409)
        assert rejected['outcome'] == 'rejected_stale' and rejected['account_id'] == account
        assert _state(call, phone, device)['summary'][held] == 1

        # The fresh token retires it; the device is gone; both outcomes replay.
        operation = str(uuid.uuid4())
        done = retire(phone, device, operation, left['state_token'], 200)
        assert done == {'operation_id': operation, 'account_id': account,
                        'outcome': 'confirmed_applied'}, done
        _no_cookie(call)
        assert all(d['id'] != device for d in call('GET', V1 + '/devices', phone)['devices'])
        assert retire(phone, device, operation, left['state_token'], 200) == done
        assert retire(phone, device, stale, offered['state_token'], 409)['outcome'] == 'rejected_stale'
