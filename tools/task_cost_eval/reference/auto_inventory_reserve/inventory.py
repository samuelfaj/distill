from collections import Counter


class OutOfStock(Exception):
    pass


class Inventory:
    def __init__(self, stock):
        self.stock = dict(stock)
        self.reservations = {}

    def available(self, sku):
        return self.stock.get(sku, 0)

    def reserve(self, order_id, lines):
        """Hold stock for an order, all lines or none. A retried order returns its existing hold."""
        wanted = Counter()
        for sku, quantity in lines:
            if quantity < 1:
                raise ValueError('quantity must be positive')
            wanted[sku] += quantity
        wanted = dict(wanted)
        if order_id in self.reservations:
            if self.reservations[order_id] != wanted:
                raise ValueError(f'order {order_id} was already reserved with different lines')
            return sorted(wanted.items())
        for sku, quantity in wanted.items():
            if self.available(sku) < quantity:
                raise OutOfStock(sku)
        for sku, quantity in wanted.items():
            self.stock[sku] -= quantity
        self.reservations[order_id] = wanted
        return sorted(wanted.items())

    def release(self, order_id):
        """Return an order's held stock; False when the order holds nothing."""
        held = self.reservations.pop(order_id, None)
        if held is None:
            return False
        for sku, quantity in held.items():
            self.stock[sku] = self.stock.get(sku, 0) + quantity
        return True
