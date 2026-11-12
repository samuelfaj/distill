#!/usr/bin/env python3
import sys

from _par_common import attempt, finish, fixture_dir, import_fresh


def main(argv):
    failures = []
    try:
        gateway_module, client = import_fresh(fixture_dir(argv, 'hard_retry_charge'), 'gateway', 'client')
    except Exception as error:
        return finish(['import: ' + repr(error)])
    FakeGateway, CardDeclined = gateway_module.FakeGateway, gateway_module.CardDeclined

    def lost_responses():
        gateway = FakeGateway(['timeout_after', 'timeout_after', 'ok'])
        return client.pay(gateway, 'o1', 500), len(gateway.charges), gateway.calls
    attempt(failures, 'lost responses are retried without a second charge', lost_responses, ('ch_1', 1, 3))

    def paid_again_later():
        gateway = FakeGateway()
        first = client.pay(gateway, 'o7', 900)
        second = client.pay(gateway, 'o7', 900)
        return first, second, len(gateway.charges)
    attempt(failures, 'calling pay again for the same order', paid_again_later, ('ch_1', 'ch_1', 1))

    def lost_then_paid_again():
        gateway = FakeGateway(['timeout_after'] * 3)
        try:
            client.pay(gateway, 'o2', 300)
        except TimeoutError:
            pass
        return client.pay(gateway, 'o2', 300), len(gateway.charges)
    attempt(failures, 'retrying an order after all attempts timed out', lost_then_paid_again, ('ch_1', 1))

    def different_orders():
        gateway = FakeGateway()
        return [client.pay(gateway, order, 100) for order in ('a', 'b', 'a')], len(gateway.charges)
    attempt(failures, 'different orders are charged separately', different_orders, (['ch_1', 'ch_2', 'ch_1'], 2))

    def three_timeouts():
        gateway = FakeGateway(['timeout_before'] * 4)
        try:
            client.pay(gateway, 'o3', 100)
        except TimeoutError:
            return 'TimeoutError', gateway.calls
        return 'no error', gateway.calls
    attempt(failures, 'three attempts in total', three_timeouts, ('TimeoutError', 3))

    def declined():
        gateway = FakeGateway(['timeout_before'], declined=['o4'])
        try:
            client.pay(gateway, 'o4', 100)
        except CardDeclined:
            return 'CardDeclined', gateway.calls
        return 'no error', gateway.calls
    attempt(failures, 'a decline is not retried', declined, ('CardDeclined', 2))
    return finish(failures)


if __name__ == '__main__':
    raise SystemExit(main(sys.argv))
