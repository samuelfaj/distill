class Catalog:
    def __init__(self, prices):
        self._prices = dict(prices)
        self._listeners = []
        self.lookups = 0

    def subscribe(self, listener):
        """listener(sku) is called after a price changes."""
        self._listeners.append(listener)

    def price(self, sku):
        self.lookups += 1
        return self._prices[sku]

    def update_price(self, sku, cents):
        self._prices[sku] = cents
        for listener in list(self._listeners):
            listener(sku)
