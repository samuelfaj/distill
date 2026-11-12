SYMBOLS = {'USD': '$', 'EUR': '€', 'GBP': '£'}


def format_amount(value, places=2, sep=',', *, currency=None):
    """Format a number with thousands separators: format_amount(1234.5) -> '1,234.50'.

    currency puts its symbol (or the code and a space) between the sign and the digits.
    """
    text = f'{abs(value):,.{places}f}'
    if sep != ',':
        text = text.replace(',', sep)
    if currency is not None:
        text = SYMBOLS.get(currency, currency + ' ') + text
    return ('-' if value < 0 else '') + text
