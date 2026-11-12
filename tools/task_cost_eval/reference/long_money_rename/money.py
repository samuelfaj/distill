class Money:
    """An amount of integer cents in one currency."""

    def __init__(self, currency, cents):
        if type(cents) is not int:
            raise TypeError('cents must be an int')
        self.currency = currency
        self.cents = cents

    def __add__(self, other):
        if other.currency != self.currency:
            raise ValueError('currency mismatch')
        return Money(self.currency, self.cents + other.cents)

    def __eq__(self, other):
        return isinstance(other, Money) and (self.currency, self.cents) == (other.currency, other.cents)

    def __repr__(self):
        return f'Money({self.currency!r}, {self.cents!r})'

    def to_dict(self):
        """Wire format shared with other services."""
        return {'amount_cents': self.cents, 'currency': self.currency}

    @classmethod
    def from_dict(cls, data):
        return cls(data['currency'], data['amount_cents'])
