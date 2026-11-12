def pay(gateway, order_id, amount_cents):
    """Charge an order; returns the charge id."""
    return gateway.charge(order_id, amount_cents)
