"""Shared helper for graders of JavaScript cases: run one Node.js check script.

The script runs with the fixture directory as cwd (so require('./x') loads the
task's module), records each check with check()/checkAsync() and prints one
JSON object of results; the Python grader compares them with attempt().
"""
import json
import os
import shutil
import subprocess

PRELUDE = r'''
const out = {};
const describe = (e) => ({error: String((e && e.message) || e)});
function check(label, fn) { try { out[label] = fn(); } catch (e) { out[label] = describe(e); } }
async function checkAsync(label, fn) { try { out[label] = await fn(); } catch (e) { out[label] = describe(e); } }
'''


def run_node(directory, body, timeout=20):
    """Run PRELUDE + body (an async function body) in directory; return the results object."""
    node = shutil.which('node')
    if node is None:
        raise RuntimeError('node is not installed; JavaScript cases need Node.js 18 or newer')
    script = PRELUDE + '(async () => {\n' + body + '\n})().then(() => console.log(JSON.stringify(out)));'
    result = subprocess.run([node, '-e', script], cwd=directory, capture_output=True, text=True, timeout=timeout,
                            env={**os.environ, 'NODE_OPTIONS': ''})
    if result.returncode != 0:
        raise RuntimeError(f'node exited {result.returncode}: {result.stderr.strip()[-400:]}')
    return json.loads(result.stdout.strip().splitlines()[-1])
