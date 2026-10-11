import json
from schema import validate_partial


def parse(text):
    patch = json.loads(text)
    validate_partial(patch)
    return patch
