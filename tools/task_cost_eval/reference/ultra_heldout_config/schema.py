import copy

_DEFAULT = {'server':{'host':'localhost','port':8080}, 'features':{'debug':False,'tags':['base']}, 'paths':{'root':'.'}}


def defaults():
    return copy.deepcopy(_DEFAULT)


def _leaf(section, key, value):
    if key in ('host','root'):
        return isinstance(value,str) and bool(value) and value == value.strip() and '\r' not in value and '\n' not in value
    if key == 'port':
        return type(value) is int and 1 <= value <= 65535
    if key == 'debug':
        return type(value) is bool
    if key == 'tags':
        return isinstance(value,list) and all(isinstance(v,str) and bool(v) and v == v.strip() and not any(c in v for c in ',\r\n') for v in value)
    return False


def validate(config):
    if not isinstance(config,dict) or set(config) != set(_DEFAULT):
        raise ValueError('invalid sections')
    for section, fields in _DEFAULT.items():
        value = config[section]
        if not isinstance(value,dict) or set(value) != set(fields):
            raise ValueError('invalid fields: '+section)
        for key, leaf in value.items():
            if not _leaf(section,key,leaf):
                raise ValueError('invalid value: '+section+'.'+key)


def validate_partial(patch):
    if not isinstance(patch,dict):
        raise ValueError('patch must be an object')
    for section, value in patch.items():
        if section not in _DEFAULT:
            raise ValueError('unknown section')
        if value is None:
            continue
        if not isinstance(value,dict):
            raise ValueError('section must be an object or null')
        for key, leaf in value.items():
            if key not in _DEFAULT[section] or (leaf is not None and not _leaf(section,key,leaf)):
                raise ValueError('invalid patch: '+section+'.'+key)
