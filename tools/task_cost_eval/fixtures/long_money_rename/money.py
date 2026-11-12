class Money:
    """An amount of integer cents in one currency."""

    def __init__(self, amount_cents, currency='USD'):
        if type(amount_cents) is not int:
            raise TypeError('amount_cents must be an int')
        self.amount_cents = amount_cents
        self.currency = currency

    def __add__(self, other):
        if other.currency != self.currency:
            raise ValueError('currency mismatch')
        return Money(self.amount_cents + other.amount_cents, self.currency)

    def __eq__(self, other):
        return isinstance(other, Money) and (self.amount_cents, self.currency) == (other.amount_cents, other.currency)

    def __repr__(self):
        return f'Money({self.amount_cents!r}, {self.currency!r})'

    def to_dict(self):
        """Wire format shared with other services."""
        return {'amount_cents': self.amount_cents, 'currency': self.currency}

    @classmethod
    def from_dict(cls, data):
        return cls(data['amount_cents'], data['currency'])
