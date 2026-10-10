class Catalog:
    def __init__(self, entries):
        self.entries = dict(entries)
        self.revision = 0

    def get(self, sku):
        return self.entries[sku]

    def set_price(self, sku, cents):
        if type(cents) is not int or cents < 0:
            raise ValueError('cents must be a nonnegative integer')
        self.entries[sku]['cents'] = cents
        self.revision += 1
