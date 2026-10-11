from urllib.parse import quote


def encode_query(pairs):
    values = []
    for key,value in pairs:
        if not isinstance(key,str) or (value is not None and not isinstance(value,str)):
            raise ValueError('query keys/values must be strings or None values')
        if value is not None:
            values.append((key,value))
    return '&'.join(quote(key,safe='-._~')+'='+quote(value,safe='-._~') for key,value in sorted(values))


def redact_pairs(pairs, sensitive):
    keys = {key.casefold() for key in sensitive}
    return [(key,'[REDACTED]' if key.casefold() in keys else value) for key,value in pairs]
