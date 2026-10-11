from cache import QuoteCache
from pricing import convert, lines_for


class QuoteService:
    def __init__(self, catalog, rates):
        self.catalog, self.rates = catalog, rates
        self.cache = QuoteCache()

    def quote(self, order, currency='USD'):
        key = (order['id'], currency)
        cached = self.cache.get(key)
        if cached is not None:
            return cached
        lines = lines_for(order, self.catalog)
        quote = {'order_id':order['id'], 'currency':currency, 'lines':lines,
                 'total_cents':convert(sum(line['usd_cents'] for line in lines), self.rates.get(currency))}
        self.cache.put(key, quote)
        return quote
