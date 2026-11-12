def format_amount(value, places=2, sep=','):
    """Format a number with thousands separators: format_amount(1234.5) -> '1,234.50'."""
    text = f'{abs(value):,.{places}f}'
    if sep != ',':
        text = text.replace(',', sep)
    return ('-' if value < 0 else '') + text
