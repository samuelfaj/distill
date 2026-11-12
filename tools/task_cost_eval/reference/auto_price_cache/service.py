from cache import PriceCache


class Shop:
    def __init__(self, catalog):
        self.catalog = catalog
        self.cache = PriceCache(catalog)

    def set_price(self, sku, cents):
        self.catalog.update_price(sku, cents)

    def quote(self, cart):
        """cart: list of (sku, quantity). Returns the total in cents."""
        skus = [sku for sku, _ in cart]
        return sum(price * quantity for price, (_, quantity) in zip(self.cache.prices(skus), cart))
