from schema import defaults, validate
from json_source import parse as parse_json
from env_source import parse as parse_env
from merge import overlay


def default_config():
    config = defaults()
    validate(config)
    return config


def resolve(json_texts=(), env=None):
    config = default_config()
    for text in json_texts:
        config = overlay(config,parse_json(text))
    config = overlay(config,parse_env({} if env is None else env))
    validate(config)
    return config
