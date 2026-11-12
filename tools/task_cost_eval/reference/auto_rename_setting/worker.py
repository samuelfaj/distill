from client import attempts_for


def describe(settings):
    return f"retries={settings['max_retries']} attempts={attempts_for(settings)}"
