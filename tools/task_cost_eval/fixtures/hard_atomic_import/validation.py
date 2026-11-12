def validate(record):
    """Raise ValueError when a record cannot be imported."""
    if not isinstance(record, dict):
        raise ValueError('record must be an object')
    sku = record.get('sku')
    if not isinstance(sku, str) or not sku.strip():
        raise ValueError('sku is required')
    qty = record.get('qty')
    if type(qty) is not int or qty < 0:
        raise ValueError('qty must be a non-negative integer')
    price = record.get('price_cents')
    if type(price) is not int or price <= 0:
        raise ValueError('price_cents must be a positive integer')
