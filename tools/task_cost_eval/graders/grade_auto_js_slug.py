#!/usr/bin/env python3
import sys

from _js_common import run_node
from _par_common import attempt, finish, fixture_dir

CHECKS = """
const { slugify } = require('./slug');
const { articlePath, tagPath } = require('./paths');
check('default', () => slugify('Hello World'));
check('runs and edges', () => slugify('  Ten Tips & Tricks!! '));
check('explicit', () => slugify('a b c', '_'));
check('article', () => articlePath('Ten Tips & Tricks'));
check('tag', () => tagPath('Data Science'));
"""


def main(argv):
    root = fixture_dir(argv, 'auto_js_slug')
    failures = []
    try:
        out = run_node(root, CHECKS)
    except Exception as error:
        return finish(['import: ' + repr(error)])
    attempt(failures, 'default separator', lambda: out['default'], 'hello-world')
    attempt(failures, 'collapsed separators', lambda: out['runs and edges'], 'ten-tips-tricks')
    attempt(failures, 'explicit separator kept', lambda: out['explicit'], 'a_b_c')
    attempt(failures, 'article path', lambda: out['article'], '/articles/ten-tips-tricks')
    attempt(failures, 'tag path', lambda: out['tag'], '/tags/data-science')
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
