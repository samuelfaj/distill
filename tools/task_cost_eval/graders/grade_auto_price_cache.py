#!/usr/bin/env python3
import sys

from _par_common import attempt, finish, fixture_dir, import_fresh


def main(argv):
    failures = []
    try:
        catalog_module, service = import_fresh(fixture_dir(argv, 'auto_price_cache'), 'catalog', 'service')
    except Exception as error:
        return finish(['import: ' + repr(error)])

    def shop():
        catalog = catalog_module.Catalog({'pen': 150, 'pad': 300, 'ink': 75})
        return catalog, service.Shop(catalog)

    def single_after_update():
        catalog, s = shop()
        s.cache.price('pen')
        s.set_price('pen', 175)
        return s.cache.price('pen')
    attempt(failures, 'single price fresh after update', single_after_update, 175)

    def bulk_after_update():
        catalog, s = shop()
        s.cache.prices(['pen', 'pad'])
        s.set_price('pad', 320)
        return s.cache.prices(['pen', 'pad'])
    attempt(failures, 'bulk prices fresh after update', bulk_after_update, [150, 320])

    def quote_after_direct_catalog_update():
        catalog, s = shop()
        s.quote([('pen', 2), ('ink', 1)])
        catalog.update_price('ink', 80)
        return s.quote([('pen', 2), ('ink', 1)])
    attempt(failures, 'quote fresh after catalog update', quote_after_direct_catalog_update, 380)

    def unchanged_prices_stay_cached():
        catalog, s = shop()
        s.quote([('pen', 1), ('pad', 1), ('ink', 1)])
        before = catalog.lookups
        s.set_price('pen', 160)
        total = s.quote([('pen', 1), ('pad', 1), ('ink', 1)])
        s.cache.price('pad')
        return total, catalog.lookups - before
    attempt(failures, 'only the changed price is looked up again', unchanged_prices_stay_cached, (535, 1))

    def repeated_reads():
        catalog, s = shop()
        for _ in range(3):
            s.cache.price('pen')
            s.quote([('pen', 1)])
        return catalog.lookups
    attempt(failures, 'repeated reads cached', repeated_reads, 1)
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
