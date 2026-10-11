import re
from schema import validate_partial


def parse(mapping):
    if not isinstance(mapping,dict):
        raise ValueError('environment mapping must be an object')
    result = {}
    paths = {'SERVER__HOST':('server','host'), 'SERVER__PORT':('server','port'),
             'FEATURES__DEBUG':('features','debug'), 'FEATURES__TAGS':('features','tags'), 'PATHS__ROOT':('paths','root')}
    for name, raw in mapping.items():
        if not name.startswith('APP__'):
            continue
        if name[5:] not in paths or not isinstance(raw,str):
            raise ValueError('invalid application environment key/value')
        section,key = paths[name[5:]]
        value = raw.strip()
        if key == 'port':
            if not re.fullmatch(r'\+?[0-9]+',value):
                raise ValueError('invalid port')
            value = int(value)
        elif key == 'debug':
            if value.lower() not in ('true','false'):
                raise ValueError('invalid debug flag')
            value = value.lower() == 'true'
        elif key == 'tags':
            value = [tag.strip() for tag in value.split(',') if tag.strip()]
        result.setdefault(section,{})[key] = value
    validate_partial(result)
    return result
