#!/usr/bin/env python3
import sys

from _par_common import attempt, finish, fixture_dir, import_fresh


def main(argv):
    failures = []
    try:
        fmt, report = import_fresh(fixture_dir(argv, 'hard_format_compat'), 'fmt', 'report')
    except Exception as error:
        return finish(['import: ' + repr(error)])
    f = fmt.format_amount
    attempt(failures, 'default', lambda: f(1234.5), '1,234.50')
    attempt(failures, 'positional places', lambda: f(1234.6, 0), '1,235')
    attempt(failures, 'positional places and separator', lambda: f(1234567.891, 1, ' '), '1 234 567.9')
    attempt(failures, 'keyword places', lambda: f(2.5, places=3), '2.500')
    attempt(failures, 'negative plain', lambda: f(-1234.5), '-1,234.50')
    attempt(failures, 'symbol', lambda: [f(9.99, currency=c) for c in ('USD', 'EUR', 'GBP')], ['$9.99', '€9.99', '£9.99'])
    attempt(failures, 'code', lambda: f(12, currency='CHF'), 'CHF 12.00')
    attempt(failures, 'negative symbol', lambda: f(-1.5, currency='USD'), '-$1.50')
    attempt(failures, 'negative code', lambda: f(-1234.5, currency='CHF'), '-CHF 1,234.50')
    attempt(failures, 'currency with places', lambda: f(9, places=0, currency='GBP'), '£9')
    attempt(failures, 'currency with separator', lambda: f(1234.5, 2, '.', currency='EUR'), '€1.234.50')
    attempt(failures, 'summary line', lambda: report.summary_line('Total', 1234.6), 'Total: 1,235')
    attempt(failures, 'table', lambda: report.table([('rent', 1234.5), ('fees', -20)]),
            'rent            1 234.50\nfees              -20.00')
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
