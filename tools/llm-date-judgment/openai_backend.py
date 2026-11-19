"""Call a compatible Chat Completions endpoint without response or credential logs."""
import hashlib
import http.client
import json
import math
import os
import re
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request

MAX_RESPONSE_BYTES = 2 * 1024 * 1024
API_KEYS = ('backend', 'endpoint_sha256', 'response_format')


class BackendError(ValueError):
    pass


def _object(pairs):
    obj = {}
    for key, value in pairs:
        if key in obj:
            raise BackendError('invalid_json')
        obj[key] = value
    return obj


def _nonfinite(value):
    raise BackendError('invalid_json')


def _finite_float(text):
    value = float(text)
    if not math.isfinite(value):
        raise BackendError('invalid_json')
    return value


def loads(text):
    try:
        return json.loads(text, object_pairs_hook=_object, parse_constant=_nonfinite, parse_float=_finite_float)
    except (ValueError, RecursionError, UnicodeError):
        raise BackendError('invalid_json') from None


def validate_schema(value, schema):
    """Reject values and schema features outside the supported subset."""
    allowed = {'type', 'properties', 'required', 'additionalProperties', 'items', 'enum'}
    types = {'object': dict, 'array': list, 'string': str, 'boolean': bool, 'integer': int,
             'number': (int, float), 'null': type(None)}
    if (not isinstance(schema, dict) or set(schema) - allowed
            or not isinstance(schema.get('type'), str) or schema['type'] not in types):
        raise BackendError('invalid_schema')
    kind = schema['type']
    if 'enum' in schema and (not isinstance(schema['enum'], list) or not schema['enum']):
        raise BackendError('invalid_schema')
    if kind == 'object':
        properties = schema.get('properties', {})
        required = schema.get('required', [])
        if (not isinstance(properties, dict) or not isinstance(required, list)
                or any(not isinstance(k, str) or k not in properties for k in required)
                or len(set(required)) != len(required)
                or not isinstance(schema.get('additionalProperties', True), bool)):
            raise BackendError('invalid_schema')
        for child in properties.values():
            _validate_schema_shape(child)
    elif kind == 'array':
        if 'items' not in schema:
            raise BackendError('invalid_schema')
        _validate_schema_shape(schema['items'])
    if any(k in schema for k in ('properties', 'required', 'additionalProperties')) and kind != 'object':
        raise BackendError('invalid_schema')
    if 'items' in schema and kind != 'array':
        raise BackendError('invalid_schema')
    if not isinstance(value, types[kind]) or (kind in ('integer', 'number') and isinstance(value, bool)):
        raise BackendError('schema_violation')
    if kind == 'number' and not math.isfinite(value):
        raise BackendError('schema_violation')
    if 'enum' in schema and not any(type(value) is type(item) and value == item for item in schema['enum']):
        raise BackendError('schema_violation')
    if kind == 'object':
        if any(k not in value for k in required):
            raise BackendError('schema_violation')
        if schema.get('additionalProperties') is False and set(value) - set(properties):
            raise BackendError('schema_violation')
        for key, child in value.items():
            if key in properties:
                validate_schema(child, properties[key])
    elif kind == 'array':
        for item in value:
            validate_schema(item, schema['items'])


def _validate_schema_shape(schema):
    if not isinstance(schema, dict) or not isinstance(schema.get('type'), str):
        raise BackendError('invalid_schema')
    samples = {'object': {}, 'array': [], 'string': '', 'boolean': False, 'integer': 0, 'number': 0, 'null': None}
    try:
        validate_schema(samples.get(schema.get('type')), schema)
    except BackendError as error:
        if str(error) != 'schema_violation':
            raise


def _strings(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for key, child in value.items():
            yield key
            yield from _strings(child)
    elif isinstance(value, list):
        for child in value:
            yield from _strings(child)


def validate_judgment(judgment, schema, sensitive=(), normalize=lambda value: value):
    validate_schema(judgment, schema)
    for value in (judgment, normalize(judgment)):
        for text in _strings(value):
            if any(secret and secret in text for secret in sensitive):
                raise BackendError('sensitive_echo')
            try:
                text.encode('utf-8')
            except UnicodeError:
                raise BackendError('invalid_json') from None
    return judgment


def configure(backend, base_url, api_key_env, response_format, model, effort, timeout):
    if not isinstance(timeout, (int, float)) or isinstance(timeout, bool) or not math.isfinite(timeout) or timeout <= 0:
        raise BackendError('invalid_timeout')
    if backend == 'claude':
        if any(value is not None for value in (base_url, api_key_env, response_format)):
            raise BackendError('api_options_require_openai')
        return None
    if backend != 'openai' or effort != 'default':
        raise BackendError('invalid_backend_options')
    if not isinstance(model, str) or not model.strip():
        raise BackendError('invalid_model')
    if not isinstance(base_url, str) or not base_url or any(ord(c) <= 32 or ord(c) >= 127 for c in base_url) or '\\' in base_url:
        raise BackendError('invalid_base_url')
    try:
        url = urllib.parse.urlsplit(base_url)
        if (url.scheme not in ('http', 'https') or not url.hostname or url.username is not None
                or url.password is not None or url.query or url.fragment or '%' in url.netloc
                or url.port == 0 or '?' in base_url or '#' in base_url):
            raise ValueError()
    except ValueError:
        raise BackendError('invalid_base_url') from None
    env = api_key_env if api_key_env is not None else 'OPENAI_API_KEY'
    if not isinstance(env, str) or not re.fullmatch(r'[A-Za-z_][A-Za-z0-9_]*', env):
        raise BackendError('invalid_key_variable')
    key = os.environ.get(env)
    if not key or any(ord(c) < 33 or ord(c) > 126 for c in key):
        raise BackendError('invalid_api_key')
    fmt = response_format if response_format is not None else 'json_schema'
    if fmt not in ('json_schema', 'json_object'):
        raise BackendError('invalid_response_format')
    endpoint = base_url.rstrip('/') + '/chat/completions'
    return {'endpoint': endpoint, 'key': key, 'sensitive': (key, base_url, endpoint, env),
            'policy': {'backend': 'openai', 'endpoint_sha256': hashlib.sha256(endpoint.encode()).hexdigest(), 'response_format': fmt}}


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def call(config, prompt, schema_text, model, system, timeout, normalize):
    started = time.time()
    try:
        schema = loads(schema_text)
        _validate_schema_shape(schema)
        fmt = {'type': config['policy']['response_format']}
        if fmt['type'] == 'json_schema':
            fmt['json_schema'] = {'name': 'judgment', 'strict': True, 'schema': schema}
        body = json.dumps({'model': model, 'messages': [{'role': 'system', 'content': system},
                                                      {'role': 'user', 'content': prompt}], 'response_format': fmt}).encode()
        request = urllib.request.Request(config['endpoint'], data=body, headers={
            'Content-Type': 'application/json', 'Authorization': 'Bearer ' + config['key']}, method='POST')
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), _NoRedirect())
        with opener.open(request, timeout=timeout) as response:
            if response.status != 200:
                raise BackendError('http_error')
            raw = response.read(MAX_RESPONSE_BYTES + 1)
        if len(raw) > MAX_RESPONSE_BYTES:
            raise BackendError('response_too_large')
        response = loads(raw)
        if not isinstance(response, dict) or not isinstance(response.get('choices'), list) or not response['choices']:
            raise BackendError('invalid_response')
        choice = response['choices'][0]
        if not isinstance(choice, dict) or choice.get('finish_reason') != 'stop':
            raise BackendError('incomplete_response')
        message = choice.get('message')
        if not isinstance(message, dict) or message.get('refusal') is not None or not isinstance(message.get('content'), str):
            raise BackendError('invalid_message')
        judgment = validate_judgment(loads(message['content']), schema, config['sensitive'], normalize)
        usage = response.get('usage')
        usage = {k: v for k, v in usage.items() if k in ('prompt_tokens', 'completion_tokens', 'total_tokens')
                 and type(v) is int and v >= 0} if isinstance(usage, dict) else {}
        envelope = {'is_error': False, 'structured_output': judgment, 'usage': usage}
        code = 0
    except BackendError as error:
        envelope, code = {'is_error': True, 'result': str(error)}, 1
    except (TimeoutError, urllib.error.URLError, http.client.HTTPException, OSError, ValueError, RecursionError):
        envelope, code = {'is_error': True, 'result': 'transport_error'}, 1
    return subprocess.CompletedProcess([], code, json.dumps(envelope, ensure_ascii=False), ''), round(time.time() - started, 1)
