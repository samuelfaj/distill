import json
from pathlib import Path

import config

POLICY_FILE = Path(__file__).with_name('retry_policies.json')


def policy_for(endpoint):
    """(attempts, backoff_s) for endpoint: its entry in retry_policies.json, else the defaults."""
    attempts, backoff = config.RETRY_ATTEMPTS, config.BACKOFF_S
    if POLICY_FILE.exists():
        entry = json.loads(POLICY_FILE.read_text()).get(endpoint, {})
        attempts = entry.get('attempts', attempts)
        backoff = entry.get('backoff_s', backoff)
    return attempts, backoff


def call(endpoint, send, sleep):
    """Call send(endpoint), retrying ConnectionError with the endpoint's backoff policy."""
    attempts, backoff = policy_for(endpoint)
    for attempt in range(attempts):
        try:
            return send(endpoint)
        except ConnectionError:
            if attempt == attempts - 1:
                raise
            sleep(backoff * 2 ** attempt)
