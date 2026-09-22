def total_with_tax(amount_cents: int, rate_bps: int) -> int:
    return amount_cents * rate_bps // 10000
