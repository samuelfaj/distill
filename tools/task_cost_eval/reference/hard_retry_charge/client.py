ATTEMPTS = 3


def pay(gateway, order_id, amount_cents):
    """Charge an order; returns the charge id.

    The idempotency key is derived from the order, so retries and later calls for the
    same order can never create a second charge.
    """
    key = f'order:{order_id}'
    for attempt in range(1, ATTEMPTS + 1):
        try:
            return gateway.charge(order_id, amount_cents, idempotency_key=key)
        except TimeoutError:
            if attempt == ATTEMPTS:
                raise
