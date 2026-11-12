import copy

CURRENT_VERSION = 2
DEFAULT_FONT_SIZE = 12


def _font(text):
    family, _, size = text.strip().rpartition(' ')
    if family and size.isdigit():
        return {'family': family, 'size': int(size)}
    return {'family': text.strip(), 'size': DEFAULT_FONT_SIZE}


def migrate(settings):
    """Upgrade settings to CURRENT_VERSION; current data comes back unchanged. Never mutates the input."""
    data = copy.deepcopy(settings)
    if data.get('version', 1) >= CURRENT_VERSION:
        return data
    if isinstance(data.get('font'), str):
        data['font'] = _font(data['font'])
    if isinstance(data.get('plugins'), str):
        data['plugins'] = [name.strip() for name in data['plugins'].split(',') if name.strip()]
    data['version'] = CURRENT_VERSION
    return data
