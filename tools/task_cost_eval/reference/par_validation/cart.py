def add_item(cart, name, qty):
    if not isinstance(name, str) or not name.strip():
        raise ValueError("name must be a non-empty string")
    if isinstance(qty, bool) or not isinstance(qty, int) or qty <= 0:
        raise ValueError("qty must be a positive int")
    cart[name] = cart.get(name, 0) + qty
    return cart
