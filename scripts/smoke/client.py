import json
import re
import urllib.error
import urllib.request
import uuid
from jsonschema import Draft202012Validator, FormatChecker

class ContractClient:
    def __init__(self, base, document, request_ids):
        self.base = base
        self.document = document
        self.request_ids = request_ids
        self.last_headers = {}

    def __call__(self, method, path, token=None, body=None, operation=None, expected=200, extra_headers=None):
        headers = {'Content-Type': 'application/json', **(extra_headers or {})}
        if token:
            headers['Authorization'] = 'Bearer ' + token
        if operation:
            headers['Idempotency-Key'] = operation
        request = urllib.request.Request(self.base + path, method=method, headers=headers,
            data=None if body is None else json.dumps(body).encode())
        try:
            response = urllib.request.urlopen(request, timeout=15)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            if response.status != expected:
                raise RuntimeError(f'{method} {path}: expected {expected}, got {response.status}')
            raw = response.read()
            value = json.loads(raw) if raw else None
            request_id = response.headers['X-Request-ID']
            uuid.UUID(request_id)
            self.request_ids.add(request_id)
            if response.headers['Cache-Control'] != 'private, no-store':
                raise RuntimeError('Missing cache control')
            self.last_headers = dict(response.headers)
            route = path.split('?')[0].removeprefix('/api/experimental/v1')
            candidates = [template for template in self.document['paths']
                          if re.fullmatch(re.sub(r"\{[^}]+\}", "[^/]+", template), route)]
            # Literal routes such as /sessions/current take precedence over parameters.
            candidates.sort(key=lambda template: (template.count('{'), template))
            if not candidates:
                raise RuntimeError(f'No contract route matches {method} {route}')
            route = candidates[0]
            operation_doc = self.document['paths'][route][method.lower()]
            if body is not None and expected < 400 and 'requestBody' in operation_doc:
                request_schema = operation_doc['requestBody']['content']['application/json']['schema']
                Draft202012Validator({**self.document, **request_schema}, format_checker=FormatChecker()).validate(body)
            contract = operation_doc['responses'][str(response.status)]
            if raw:
                schema = contract['content']['application/json']['schema']
                Draft202012Validator({**self.document, **schema}, format_checker=FormatChecker()).validate(value)
            if response.status >= 400 and value['request_id'] != request_id:
                raise RuntimeError('Error/request correlation mismatch')
            return value

