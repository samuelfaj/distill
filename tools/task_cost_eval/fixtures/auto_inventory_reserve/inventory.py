class OutOfStock(Exception):
    pass


class Inventory:
    def __init__(self, stock):
        self.stock = dict(stock)
        self.reservations = {}

    def available(self, sku):
        return self.stock.get(sku, 0)

    def reserve(self, order_id, lines):
        """Hold stock for an order. lines is a list of (sku, quantity)."""
        held = self.reservations.setdefault(order_id, [])
        for sku, quantity in lines:
            if self.available(sku) < quantity:
                raise OutOfStock(sku)
            self.stock[sku] -= quantity
            held.append((sku, quantity))
        return list(held)
