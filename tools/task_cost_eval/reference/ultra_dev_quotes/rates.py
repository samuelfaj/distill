import copy


class RateTable:
    def __init__(self, entries):
        self.entries = copy.deepcopy(entries)
        self.revision = 0

    def get(self, currency):
        return copy.deepcopy(self.entries[currency])

    def set_rate(self, currency, numerator, denominator):
        if type(numerator) is not int or type(denominator) is not int or numerator <= 0 or denominator <= 0:
            raise ValueError('rate must be positive integers')
        self.entries[currency] = {'numerator':numerator,'denominator':denominator}
        self.revision += 1
