"""Normalizer for the sessions_email column (rules in SPEC.md)."""
REQUIRED_SUFFIX = '.org'


def normalize(value):
    value = value.strip()
    if '@' not in value:
        return None
    local, domain = value.split('@')
    if not local or '.' not in domain.strip('.'):
        return None
    if REQUIRED_SUFFIX and not domain.lower().endswith(REQUIRED_SUFFIX):
        return None
    return local + '@' + domain.lower()
