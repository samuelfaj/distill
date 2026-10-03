#!/usr/bin/env python3
import sys

from _par_common import attempt, finish, fixture_dir, import_fresh


def main(argv):
    root = fixture_dir(argv, "seq_rename")
    failures = []
    for path in sorted(root.glob("*.py")):
        if "fmt_price" in path.read_text():
            failures.append(f"{path.name} still mentions fmt_price")
    try:
        pricing, cart, invoice, receipt = import_fresh(root, "pricing", "cart", "invoice", "receipt")
    except Exception as error:
        failures.append(f"import failed: {type(error).__name__}: {error}")
        return finish(failures)
    attempt(failures, "format_price default", lambda: pricing.format_price(1250), "$12.50")
    attempt(failures, "format_price EUR", lambda: pricing.format_price(1250, currency="EUR"), "€12.50")
    attempt(failures, "format_price unknown", lambda: pricing.format_price(1, currency="XXX"), raises=Exception)
    attempt(failures, "cart.line_text", lambda: cart.line_text("pen", 250, 3), "3 x pen: $7.50")
    attempt(failures, "receipt_total", lambda: receipt.receipt_total([100, 205]), "Total: $3.05")
    attempt(failures, "invoice_line", lambda: invoice.invoice_line("rent", 99900), "rent: €999.00")
    return finish(failures)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
