import uuid

ATTEMPTS = 3


def pay(gateway, order_id, amount_cents):
    """Charge an order; returns the charge id."""
    key = str(uuid.uuid4())
    for attempt in range(1, ATTEMPTS + 1):
        try:
            return gateway.charge(order_id, amount_cents, idempotency_key=key)
        except TimeoutError:
            if attempt == ATTEMPTS:
                raise
