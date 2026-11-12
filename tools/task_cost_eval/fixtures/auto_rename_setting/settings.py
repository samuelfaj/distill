DEFAULTS = {'retry_limit': 3, 'timeout_s': 10}


def load(overrides=None):
    merged = dict(DEFAULTS)
    merged.update(overrides or {})
    return merged
