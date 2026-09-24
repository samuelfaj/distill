from formatter import format_line


def render_inventory(items):
    totals = {item["sku"]: item["quantity"] for item in items}
    return "\n".join(format_line(sku, quantity) for sku, quantity in sorted(totals.items()))
