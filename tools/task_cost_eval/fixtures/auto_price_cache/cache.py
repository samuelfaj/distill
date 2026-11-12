class PriceCache:
    """Remembers catalog prices so repeated quotes do not hit the catalog."""

    def __init__(self, catalog):
        self.catalog = catalog
        self.single = {}
        self.bulk = {}

    def price(self, sku):
        if sku not in self.single:
            self.single[sku] = self.catalog.price(sku)
        return self.single[sku]

    def prices(self, skus):
        key = tuple(skus)
        if key not in self.bulk:
            self.bulk[key] = [self.catalog.price(sku) for sku in key]
        return list(self.bulk[key])
