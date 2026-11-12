#!/usr/bin/env python3
import json
import sys
from pathlib import Path

from _js_common import run_node
from _par_common import attempt, finish, fixture_dir

EXPECTED = json.loads(Path(__file__).with_name('long_cjs_to_esm_expected.json').read_text())
CHECKS = """
const fs = require('node:fs');
const path = require('node:path');
const { pathToFileURL } = require('node:url');
const load = (file) => import(pathToFileURL(path.resolve(file)).href);
check('type', () => JSON.parse(fs.readFileSync('package.json', 'utf8')).type);
const calls = CALLS;
for (const [index, call] of calls.entries()) {
  await checkAsync('call' + index, async () => {
    const loaded = await load('lib/' + call.module + '.js');
    return loaded[call.export](...call.arguments);
  });
}
await checkAsync('index', async () => Object.keys(await load('index.js')).sort());
"""


def main(argv):
    root = fixture_dir(argv, 'long_cjs_to_esm')
    failures = []
    calls = [{key: call[key] for key in ('module', 'export', 'arguments')} for call in EXPECTED['calls']]
    try:
        out = run_node(root, CHECKS.replace('CALLS', json.dumps(calls)), timeout=60)
    except Exception as error:
        return finish(['import: ' + repr(error)])
    attempt(failures, 'package.json declares ES modules', lambda: out.get('type'), 'module')
    for index, call in enumerate(EXPECTED['calls']):
        attempt(failures, f"{call['module']}.{call['export']}{tuple(call['arguments'])!r}",
                lambda: out[f'call{index}'], call['result'])
    attempt(failures, 'index.js exports', lambda: out['index'], EXPECTED['index_exports'])
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
