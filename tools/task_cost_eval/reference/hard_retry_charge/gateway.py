class CardDeclined(Exception):
    pass


class FakeGateway:
    """In-memory stand-in for the payment provider.

    outcomes is consumed one per call: 'ok', 'timeout_before' (TimeoutError, nothing charged) or
    'timeout_after' (the charge is made but the response is lost: TimeoutError). A repeated
    idempotency_key returns the first charge's id without charging again.
    """

    def __init__(self, outcomes=(), declined=()):
        self.outcomes = list(outcomes)
        self.declined = set(declined)
        self.charges = []
        self.calls = 0
        self._by_key = {}

    def charge(self, order_id, amount_cents, idempotency_key=None):
        self.calls += 1
        outcome = self.outcomes.pop(0) if self.outcomes else 'ok'
        if outcome == 'timeout_before':
            raise TimeoutError('gateway timeout')
        if order_id in self.declined:
            raise CardDeclined(order_id)
        if idempotency_key is not None and idempotency_key in self._by_key:
            charge_id = self._by_key[idempotency_key]
        else:
            charge_id = f'ch_{len(self.charges) + 1}'
            self.charges.append({'id': charge_id, 'order_id': order_id, 'amount_cents': amount_cents})
            if idempotency_key is not None:
                self._by_key[idempotency_key] = charge_id
        if outcome == 'timeout_after':
            raise TimeoutError('gateway timeout')
        return charge_id
