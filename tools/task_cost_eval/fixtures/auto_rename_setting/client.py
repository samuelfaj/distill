def attempts_for(settings):
    """Total attempts: the first try plus each retry."""
    return settings['retry_limit'] + 1


def timeout_for(settings):
    return settings['timeout_s']
