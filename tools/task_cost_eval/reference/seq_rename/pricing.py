SYMBOLS = {"USD": "$", "EUR": "€"}


def format_price(cents, currency="USD"):
    return f"{SYMBOLS[currency]}{cents // 100}.{cents % 100:02d}"
