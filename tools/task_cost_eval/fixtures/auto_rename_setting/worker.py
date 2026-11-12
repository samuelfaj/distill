from client import attempts_for


def describe(settings):
    return f"retries={settings['retry_limit']} attempts={attempts_for(settings)}"
