def summarize(rows):
    """Totals per region, sorted by region name."""
    totals = {}
    for row in rows:
        entry = totals.setdefault(row['region'], {'region': row['region'], 'total_cents': 0, 'orders': 0})
        entry['total_cents'] += row['amount_cents']
        entry['orders'] += 1
    return [totals[region] for region in sorted(totals)]


def render_text(summary):
    lines = [f"{row['region']}: ${row['total_cents'] / 100:.2f} in {row['orders']} orders" for row in summary]
    return '\n'.join(lines)


def render_csv(summary):
    import csv
    import io
    buffer = io.StringIO()
    writer = csv.writer(buffer)
    writer.writerow(['region', 'total', 'orders'])
    for row in summary:
        writer.writerow([row['region'], f"{row['total_cents'] / 100:.2f}", row['orders']])
    return buffer.getvalue()
