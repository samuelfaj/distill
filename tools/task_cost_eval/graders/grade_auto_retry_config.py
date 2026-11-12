#!/usr/bin/env python3
import json
import os
import subprocess
import sys

from _par_common import attempt, finish, fixture_dir, import_fresh

POLICIES = {'search': {'attempts': 2, 'backoff_s': 0.25}, 'reports': {'attempts': 4, 'backoff_s': 1.0}}


def failing(calls):
    def send(endpoint):
        calls.append(endpoint)
        raise ConnectionError('down')
    return send


def run_call(client, endpoint):
    calls, sleeps = [], []
    try:
        client.call(endpoint, failing(calls), sleeps.append)
    except ConnectionError:
        pass
    return len(calls), sleeps


def main(argv):
    root = fixture_dir(argv, 'auto_retry_config')
    failures = []
    policy_file = root / 'retry_policies.json'
    policy_file.write_text(json.dumps(POLICIES))
    try:
        config, client = import_fresh(root, 'config', 'client')
    except Exception as error:
        return finish(['import: ' + repr(error)])
    attempt(failures, 'first request: default attempts', lambda: config.RETRY_ATTEMPTS, 5)
    attempt(failures, 'per-endpoint policy', lambda: client.policy_for('search'), (2, 0.25))
    attempt(failures, 'default for unlisted endpoint', lambda: client.policy_for('orders'), (5, 0.5))
    attempt(failures, 'call uses the endpoint policy', lambda: run_call(client, 'reports'), (4, [1.0, 2.0, 4.0]))
    attempt(failures, 'call default policy', lambda: run_call(client, 'orders'), (5, [0.5, 1.0, 2.0, 4.0]))
    env = {**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'}

    def show(endpoint):
        run = subprocess.run([sys.executable, 'cli.py', 'show-policy', endpoint], cwd=root, env=env,
                             capture_output=True, text=True, timeout=10)
        return run.returncode, json.loads(run.stdout) if run.returncode == 0 else run.stderr
    attempt(failures, 'cli listed', lambda: show('search'), (0, {'attempts': 2, 'backoff_s': 0.25}))
    attempt(failures, 'cli default', lambda: show('orders'), (0, {'attempts': 5, 'backoff_s': 0.5}))
    policy_file.unlink()
    attempt(failures, 'no policy file means defaults', lambda: client.policy_for('search'), (5, 0.5))
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
