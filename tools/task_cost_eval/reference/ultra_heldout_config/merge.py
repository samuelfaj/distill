import copy
from schema import defaults, validate, validate_partial


def overlay(base, patch):
    validate(base)
    validate_partial(patch)
    result = copy.deepcopy(base)
    original = defaults()
    for section, value in patch.items():
        if value is None:
            result[section] = original[section]
        else:
            for key, leaf in value.items():
                result[section][key] = copy.deepcopy(original[section][key] if leaf is None else leaf)
    validate(result)
    return result
