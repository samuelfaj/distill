#!/usr/bin/env python3
import sys

from _par_common import attempt, finish, fixture_dir, import_fresh


def main(argv):
    failures = []
    try:
        words, = import_fresh(fixture_dir(argv, 'ctrl_word_count'), 'words')
    except Exception as error:
        return finish(['import: ' + repr(error)])
    attempt(failures, 'case and punctuation', lambda: words.count_words("Hello, hello! HELLO world. -- don't stop"),
            {'hello': 3, 'world': 1, "don't": 1, 'stop': 1})
    attempt(failures, 'empty', lambda: words.count_words('  '), {})
    attempt(failures, 'plain', lambda: words.count_words('a b a'), {'a': 2, 'b': 1})
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
