"""Normalizer for the tokens_email column (rules in SPEC.md)."""
REQUIRED_SUFFIX = '.edu'


def normalize(value):
    value = value.strip()
    if value.count('@') != 1 or ' ' in value:
        return None
    local, domain = value.split('@')
    if not local or '.' not in domain.strip('.'):
        return None
    if REQUIRED_SUFFIX and not domain.lower().endswith(REQUIRED_SUFFIX):
        return None
    return local + '@' + domain.lower()
