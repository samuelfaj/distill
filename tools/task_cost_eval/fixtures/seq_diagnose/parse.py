def parse_amount(text):
    """Parse a money string such as '1,234.50' or '-5.10' into integer cents."""
    return int(float(text.replace(",", "")) * 100)
