#!/usr/bin/env python3
"""Catalog-only peer: rejects any request that could run or select a model."""
import json
import os
import sys

assert '--model' not in sys.argv
assert '--thinking' not in sys.argv
if os.environ.get('CATALOG_REQUIRE_CONFIG'):
    assert sys.argv[1:] == [
        'app-server', '--listen', 'stdio://',
        '-c', 'model_catalog_json="/tmp/catalog.json"',
        '-c', 'model_provider="test"',
        '-c', 'model_providers.test.name="Test"',
    ], sys.argv
if os.environ.get('CATALOG_LOG'):
    with open(os.environ['CATALOG_LOG'], 'a') as log:
        log.write(json.dumps({'args': sys.argv[1:], 'env': os.environ.get('CATALOG_ID')}) + '\n')

for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method', request.get('type'))
    if os.environ.get('CATALOG_FAIL'):
        print(json.dumps({'id': request['id'], 'error': {'message': 'unavailable'},
                          'type': 'response', 'success': False}), flush=True)
    elif method == 'initialize':
        print(json.dumps({'id': request['id'], 'result': {}}), flush=True)
    elif method == 'model/list':
        assert request['params']['includeHidden'] is False
        cursor = request['params'].get('cursor')
        result = {'data': [{'model': 'second' if cursor else ('configured-catalog-model' if os.environ.get('CATALOG_REQUIRE_CONFIG') else os.environ.get('CATALOG_ID', 'first'))}],
                  'nextCursor': None if cursor else 'next'}
        print(json.dumps({'id': request['id'], 'result': result}), flush=True)
    elif method == 'get_available_models':
        assert '--no-session' in sys.argv
        print(json.dumps({'id': request['id'], 'type': 'response', 'success': True,
                          'data': {'models': [{'provider': 'test', 'id': 'pi-model'}]}}), flush=True)
    else:
        raise AssertionError('Unexpected request: ' + method)
