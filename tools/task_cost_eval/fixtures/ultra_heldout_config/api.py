from schema import defaults, validate


def default_config():
    config = defaults()
    validate(config)
    return config


def resolve(json_texts=(), env=None):
    if json_texts or env:
        raise NotImplementedError('configuration imports pending')
    return default_config()
