#!/usr/bin/env python3
import sys

from _par_common import attempt, finish, fixture_dir, import_fresh


def main(argv):
    failures = []
    try:
        inventory, checkout = import_fresh(fixture_dir(argv, 'auto_inventory_reserve'), 'inventory', 'checkout')
    except Exception as error:
        return finish(['import: ' + repr(error)])
    Inventory, OutOfStock = inventory.Inventory, inventory.OutOfStock

    def stock(inv):
        return {sku: inv.available(sku) for sku in ('pen', 'pad', 'ink')}

    def partial_failure():
        inv = Inventory({'pen': 5, 'pad': 1, 'ink': 2})
        try:
            inv.reserve('o1', [('pen', 2), ('pad', 3)])
        except OutOfStock:
            pass
        return stock(inv), inv.reserve('o2', [('pen', 5)]) is not None and stock(inv)

    attempt(failures, 'failed order holds nothing', partial_failure,
            ({'pen': 5, 'pad': 1, 'ink': 2}, {'pen': 0, 'pad': 1, 'ink': 2}))

    def retried():
        inv = Inventory({'pen': 5, 'pad': 1, 'ink': 2})
        inv.reserve('o1', [('pen', 2)])
        inv.reserve('o1', [('pen', 2)])
        return stock(inv)
    attempt(failures, 'retry does not reserve twice', retried, {'pen': 3, 'pad': 1, 'ink': 2})

    def conflicting():
        inv = Inventory({'pen': 5})
        inv.reserve('o1', [('pen', 2)])
        inv.reserve('o1', [('pen', 3)])
    attempt(failures, 'same id, different lines', conflicting, raises=ValueError)

    def duplicate_lines():
        inv = Inventory({'pen': 3})
        try:
            inv.reserve('o1', [('pen', 2), ('pen', 2)])
        except OutOfStock:
            return inv.available('pen')
        return 'reserved'
    attempt(failures, 'duplicate skus are summed before checking', duplicate_lines, 3)

    def released():
        inv = Inventory({'pen': 5, 'pad': 1, 'ink': 2})
        inv.reserve('o1', [('pen', 2), ('ink', 2)])
        first = inv.release('o1')
        return first, inv.release('o1'), inv.release('missing'), stock(inv)
    attempt(failures, 'release', released, (True, False, False, {'pen': 5, 'pad': 1, 'ink': 2}))

    def release_then_retry():
        inv = Inventory({'pen': 2})
        inv.reserve('o1', [('pen', 2)])
        inv.release('o1')
        inv.reserve('o1', [('pen', 2)])
        return inv.available('pen')
    attempt(failures, 'released order can be reserved again', release_then_retry, 0)

    def place():
        inv = Inventory({'pen': 1, 'pad': 1})
        rejected = checkout.place_order(inv, 'o1', [('pen', 1), ('pad', 2)])
        return rejected['status'], stock(inv), checkout.place_order(inv, 'o2', [('pen', 1)])['status']
    attempt(failures, 'checkout rejection keeps stock', place, ('rejected', {'pen': 1, 'pad': 1, 'ink': 0}, 'reserved'))
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
