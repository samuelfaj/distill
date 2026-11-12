from fmt import format_amount


def summary_line(label, total):
    return f'{label}: {format_amount(total, 0)}'


def table(rows):
    return '\n'.join(f'{name:<10}{format_amount(amount, 2, " "):>14}' for name, amount in rows)
