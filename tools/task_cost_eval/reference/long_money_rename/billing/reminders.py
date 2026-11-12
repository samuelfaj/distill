import money
from money import Money


def roundtrip_reminders(payload):
    m = Money.from_dict(payload)
    return Money(m.currency, m.cents + 756).to_dict()


def wire_reminders(cents):
    return {'amount_cents': cents, 'currency': 'USD', 'source': 'reminders'}


def make_reminders(cents):
    return money.Money('USD', cents).to_dict()
