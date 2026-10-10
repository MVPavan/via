"""Allow-listed catalogue diagnostics for OpenCode qualification (§13).

This projection never authorizes spending. Unknown prices stay unavailable;
URLs, headers, configuration and arbitrary vendor error text are discarded.
"""

import math
import re

# §13: bound identifiers retained from §9's bounded catalogue response.
CATALOG_ID_BYTES = 256
MODEL_STATUSES = frozenset({'deprecated', 'alpha', 'beta', 'stable', 'preview'})


def catalog_record(http_status, body, location, identity, protected):
    """Keep IDs, prices and fixed statuses at a spending/schema block (§13)."""
    def identifier(value):
        if type(value) is not str or len(value) > CATALOG_ID_BYTES \
                or re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._/-]*', value) is None \
                or protected(value.encode('utf-8')):
            return None
        return value

    def number(value):
        try:
            return value if type(value) in {int, float} and math.isfinite(value) else None
        except OverflowError:
            return None

    def prices(value):
        if type(value) is not dict:
            return None
        result = {name: number(value[name]) for name in
                  ('input', 'output', 'cache_read', 'cache_write') if name in value}
        if 'cache' in value:
            cache = value['cache']
            result['cache'] = {name: number(cache[name]) for name in ('read', 'write')
                               if name in cache} if type(cache) is dict else None
        return result

    provider, _, model = identity.partition('/') if type(identity) is str else ('', '', '')
    record = {'route': '/api/model', 'location': location, 'http_status': http_status,
              'checked_model': {'providerID': identifier(provider), 'id': identifier(model),
                                'status': 'unverifiable'},
              'decode_status': 'unavailable' if body is None else 'decoded',
              'models_status': 'unavailable', 'models': []}
    rows = body.get('data') if type(body) is dict else None
    if type(rows) is not list:
        return record
    record['models_status'] = 'available'
    for row in rows:
        if type(row) is not dict:
            record['models'].append({'status': 'invalid-entry'})
            continue
        cost = row.get('cost')
        status = row.get('status')
        # Flat prices are retained for diagnosis too; the guard still requires
        # the existing served tier/cache schema and all explicit zero fields.
        record['models'].append({
            'providerID': identifier(row.get('providerID')), 'id': identifier(row.get('id')),
            'cost': [prices(tier) for tier in cost] if type(cost) is list else prices(cost),
            'cost_status': 'absent' if 'cost' not in row else
                           'tiers' if type(cost) is list else
                           'object' if type(cost) is dict else 'invalid',
            'status': status if type(status) is str and status in MODEL_STATUSES else
                      None if status is None else 'unrecognized'})
    return record
