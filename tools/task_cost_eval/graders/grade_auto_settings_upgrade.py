#!/usr/bin/env python3
import copy
import json
import sys
import tempfile
from pathlib import Path

from _par_common import attempt, finish, fixture_dir, import_fresh

V1 = {'version': 1, 'theme': 'dark', 'font': 'Ledger Mono 14', 'plugins': 'git, lint,,spell', 'x-custom': {'a': 1}}
V2 = {'version': 2, 'theme': 'dark', 'font': {'family': 'Ledger Mono', 'size': 14}, 'plugins': ['git', 'lint', 'spell'],
      'x-custom': {'a': 1}}


def main(argv):
    root = fixture_dir(argv, 'auto_settings_upgrade')
    failures = []
    try:
        schema, store, app = import_fresh(root, 'schema', 'store', 'app')
    except Exception as error:
        return finish(['import: ' + repr(error)])
    attempt(failures, 'current version', lambda: schema.CURRENT_VERSION, 2)
    attempt(failures, 'v1 to v2 keeps unknown keys', lambda: schema.migrate(V1), V2)
    attempt(failures, 'v2 unchanged', lambda: schema.migrate(copy.deepcopy(V2)), V2)
    attempt(failures, 'idempotent', lambda: schema.migrate(schema.migrate(V1)), V2)
    attempt(failures, 'missing version is v1', lambda: schema.migrate({'font': 'Mono', 'plugins': ''}),
            {'version': 2, 'font': {'family': 'Mono', 'size': 12}, 'plugins': []})
    source = copy.deepcopy(V1)
    attempt(failures, 'input not mutated', lambda: (schema.migrate(source), source)[1], V1)
    attempt(failures, 'family with digits', lambda: schema.migrate({'version': 1, 'font': 'Mono 2 Sans', 'plugins': 'a'})['font'],
            {'family': 'Mono 2 Sans', 'size': 12})
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / 'settings.json'
        path.write_text(json.dumps(V1))
        attempt(failures, 'load upgrades', lambda: store.load(path), V2)
        attempt(failures, 'save and load round trip', lambda: (store.save(path, store.load(path)), store.load(path))[1], V2)
    attempt(failures, 'font label', lambda: app.font_label(V2), 'Ledger Mono 14pt')
    attempt(failures, 'plugin names', lambda: app.plugin_names(V2), ['git', 'lint', 'spell'])
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
