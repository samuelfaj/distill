def attempts_for(settings):
    """Total attempts: the first try plus each retry."""
    return settings['max_retries'] + 1


def timeout_for(settings):
    return settings['timeout_s']
