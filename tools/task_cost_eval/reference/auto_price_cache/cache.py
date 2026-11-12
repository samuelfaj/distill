class PriceCache:
    """Remembers catalog prices so repeated quotes do not hit the catalog.

    Entries for a sku are dropped when the catalog reports a price change.
    """

    def __init__(self, catalog):
        self.catalog = catalog
        self.single = {}
        catalog.subscribe(self.invalidate)

    def invalidate(self, sku):
        self.single.pop(sku, None)

    def price(self, sku):
        if sku not in self.single:
            self.single[sku] = self.catalog.price(sku)
        return self.single[sku]

    def prices(self, skus):
        return [self.price(sku) for sku in skus]
