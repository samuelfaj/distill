import json
from schema import validate


def render(config, format='json'):
    validate(config)
    if format == 'json':
        return json.dumps(config,sort_keys=True,separators=(',',':')) + '\n'
    if format != 'env':
        raise ValueError('unknown output format')
    values = {'APP__SERVER__HOST':config['server']['host'], 'APP__SERVER__PORT':str(config['server']['port']),
              'APP__FEATURES__DEBUG':'true' if config['features']['debug'] else 'false',
              'APP__FEATURES__TAGS':','.join(config['features']['tags']), 'APP__PATHS__ROOT':config['paths']['root']}
    return ''.join(name+'='+values[name]+'\n' for name in sorted(values))
