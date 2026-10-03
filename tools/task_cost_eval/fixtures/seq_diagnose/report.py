from parse import parse_amount


def total_cents(lines):
    return sum(parse_amount(line) for line in lines)


def format_total(lines):
    cents = total_cents(lines)
    sign = "-" if cents < 0 else ""
    return f"{sign}{abs(cents) // 100}.{abs(cents) % 100:02d}"
