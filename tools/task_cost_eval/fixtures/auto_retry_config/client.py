import config


def call(endpoint, send, sleep):
    """Call send(endpoint), retrying ConnectionError with exponential backoff."""
    for attempt in range(config.RETRY_ATTEMPTS):
        try:
            return send(endpoint)
        except ConnectionError:
            if attempt == config.RETRY_ATTEMPTS - 1:
                raise
            sleep(config.BACKOFF_S * 2 ** attempt)
