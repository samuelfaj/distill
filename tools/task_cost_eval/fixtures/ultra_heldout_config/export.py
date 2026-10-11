import json
from schema import validate


def render(config, format='json'):
    validate(config)
    if format != 'json':
        raise ValueError('unknown output format')
    return json.dumps(config,sort_keys=True,separators=(',',':')) + '\n'
