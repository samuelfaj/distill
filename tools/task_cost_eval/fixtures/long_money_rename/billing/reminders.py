import money
from money import Money


def roundtrip_reminders(payload):
    m = Money.from_dict(payload)
    return Money(m.amount_cents + 756, m.currency).to_dict()


def wire_reminders(cents):
    return {'amount_cents': cents, 'currency': 'USD', 'source': 'reminders'}


def make_reminders(cents):
    return money.Money(cents, 'USD').to_dict()
